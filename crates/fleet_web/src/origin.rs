//! `fleet-web serve`: the read-only origin behind the Fastly edge (plan 051).
//! Fleet status comes from agent-mail's probes, threads from the files
//! `fleet-web publish` mirrored here. Nothing here writes rig state.

use crate::publish::{self, is_safe_id};
use crate::signature::{OriginKey, SIGNATURE_HEADER, TIMESTAMP_HEADER};
use crate::thread_export::{self, ThreadMeta};
use agent_mail::Mailroom;
use agent_mail::rig::{self, RigProbe};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Matches the edge cache TTL: a tab refreshing every 15 s costs one probe round.
const FLEET_TTL: Duration = Duration::from_secs(15);
const WORKERS: usize = 4;

#[derive(Serialize)]
pub struct FleetRig {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub latency_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<RigProbe>,
}

#[derive(Serialize)]
pub struct Fleet {
    pub at: String,
    pub self_rig: String,
    pub rigs: Vec<FleetRig>,
}

#[derive(Serialize)]
struct ThreadIndex {
    at: String,
    threads: Vec<ThreadMeta>,
}

pub struct Origin {
    mailroom: Mailroom,
    web_home: PathBuf,
    key: OriginKey,
    fleet: Mutex<Option<(Instant, Arc<Vec<u8>>)>>,
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    fn json(body: Vec<u8>) -> Self {
        Self { status: 200, body }
    }

    fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({ "error": message })
                .to_string()
                .into_bytes(),
        }
    }
}

impl Origin {
    pub fn new(mailroom: Mailroom, web_home: PathBuf, key: OriginKey) -> Self {
        Self {
            mailroom,
            web_home,
            key,
            fleet: Mutex::new(None),
        }
    }

    pub fn handle(
        &self,
        method: &str,
        path_and_query: &str,
        timestamp: Option<&str>,
        signature: Option<&str>,
        now: i64,
    ) -> Response {
        if let Err(error) = self
            .key
            .verify(timestamp, signature, method, path_and_query, now)
        {
            return Response::error(401, &error.to_string());
        }
        if method != "GET" {
            return Response::error(405, "read-only");
        }
        let path = path_and_query.split('?').next().unwrap_or("");
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let outcome = match segments.as_slice() {
            ["api", "fleet"] => self.fleet().map(|body| Response::json(body.to_vec())),
            ["api", "threads"] => self.thread_index().map(Response::json),
            ["api", "threads", rig, id] => Ok(self.thread(rig, id)),
            _ => Ok(Response::error(404, "not found")),
        };
        outcome.unwrap_or_else(|error| Response::error(500, &format!("{error:#}")))
    }

    fn fleet(&self) -> Result<Arc<Vec<u8>>> {
        // Held across the probe on purpose: concurrent misses wait for one probe
        // round instead of each fanning out SSH to every rig.
        let mut memo = self
            .fleet
            .lock()
            .map_err(|_| anyhow::anyhow!("fleet memo poisoned"))?;
        if let Some((at, body)) = memo.as_ref()
            && at.elapsed() < FLEET_TTL
        {
            return Ok(body.clone());
        }
        let rigs = self.mailroom.rigs()?;
        let mut results = rig::status(&self.mailroom, &[])?;
        results.sort_by(|a, b| a.0.cmp(&b.0));
        let fleet = Fleet {
            at: chrono::Utc::now().to_rfc3339(),
            self_rig: rigs.self_rig.clone(),
            rigs: results
                .into_iter()
                .map(|(name, outcome, latency)| {
                    let role = rigs.rigs.get(&name).and_then(|rig| rig.role.clone());
                    let (ok, error, probe) = match outcome {
                        Ok(probe) => (true, None, Some(probe)),
                        Err(error) => (false, Some(format!("{error:#}")), None),
                    };
                    FleetRig {
                        name,
                        role,
                        ok,
                        error,
                        latency_ms: latency.as_millis(),
                        probe,
                    }
                })
                .collect(),
        };
        let body = Arc::new(serde_json::to_vec(&fleet)?);
        *memo = Some((Instant::now(), body.clone()));
        Ok(body)
    }

    fn thread_index(&self) -> Result<Vec<u8>> {
        let now = chrono::Utc::now();
        let mut threads = Vec::new();
        if let Ok(rig_dirs) = std::fs::read_dir(self.web_home.join("threads")) {
            for rig_dir in rig_dirs.flatten() {
                threads.extend(publish::read_index(&rig_dir.path()));
            }
        }
        for meta in &mut threads {
            meta.live = thread_export::is_live(&meta.updated_at, now);
        }
        threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(serde_json::to_vec(&ThreadIndex {
            at: now.to_rfc3339(),
            threads,
        })?)
    }

    fn thread(&self, rig: &str, id: &str) -> Response {
        if agent_mail::validate_name(rig).is_err() || !is_safe_id(id) {
            return Response::error(400, "bad thread path");
        }
        let path = publish::rig_dir(&self.web_home, rig).join(format!("{id}.json"));
        let Ok(bytes) = std::fs::read(&path) else {
            return Response::error(404, "no such thread");
        };
        let Ok(mut view) = serde_json::from_slice::<thread_export::ThreadView>(&bytes) else {
            return Response::error(500, "unreadable thread");
        };
        view.meta.live = thread_export::is_live(&view.meta.updated_at, chrono::Utc::now());
        match serde_json::to_vec(&view) {
            Ok(body) => Response::json(body),
            Err(error) => Response::error(500, &error.to_string()),
        }
    }
}

pub fn serve(origin: Origin, bind: &str) -> Result<()> {
    let server =
        Arc::new(tiny_http::Server::http(bind).map_err(|error| anyhow::anyhow!("{error}"))?);
    let origin = Arc::new(origin);
    eprintln!("fleet-web origin on http://{bind}");
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let server = server.clone();
            let origin = origin.clone();
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    respond(&origin, request);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().ok();
    }
    Ok(())
}

fn respond(origin: &Origin, request: tiny_http::Request) {
    let header = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv(name))
            .map(|header| header.value.as_str().to_string())
    };
    let timestamp = header(TIMESTAMP_HEADER);
    let signature = header(SIGNATURE_HEADER);
    let response = origin.handle(
        request.method().as_str(),
        request.url(),
        timestamp.as_deref(),
        signature.as_deref(),
        chrono::Utc::now().timestamp(),
    );
    let content_type = tiny_http::Header::from_bytes("content-type", "application/json").ok();
    let cache = tiny_http::Header::from_bytes("cache-control", "private, no-store").ok();
    let mut reply = tiny_http::Response::from_data(response.body).with_status_code(response.status);
    for header in [content_type, cache].into_iter().flatten() {
        reply.add_header(header);
    }
    if let Err(error) = request.respond(reply) {
        eprintln!("fleet-web: respond failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(dir: &std::path::Path) -> Result<Origin> {
        let key = OriginKey::from_hex(&"01".repeat(32))?;
        Ok(Origin::new(
            Mailroom::new(dir.join("mail")),
            dir.join("web"),
            key,
        ))
    }

    fn signed(origin: &Origin, path: &str, now: i64) -> Response {
        let signature = origin.key.sign(now, "GET", path);
        origin.handle("GET", path, Some(&now.to_string()), Some(&signature), now)
    }

    #[test]
    fn rejects_unsigned_and_serves_signed_threads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let origin = origin(dir.path())?;
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            origin.handle("GET", "/api/threads", None, None, now).status,
            401
        );
        let forged = origin.key.sign(now, "GET", "/api/fleet");
        assert_eq!(
            origin
                .handle(
                    "GET",
                    "/api/threads",
                    Some(&now.to_string()),
                    Some(&forged),
                    now
                )
                .status,
            401,
            "a signature for another path must not unlock this one"
        );

        let rig_dir = publish::rig_dir(&dir.path().join("web"), "m3");
        std::fs::create_dir_all(&rig_dir)?;
        let row = thread_export::ThreadRow {
            id: "abc-1".into(),
            summary: "s".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            created_at: None,
            parent_id: None,
            folder_paths: None,
        };
        let view = thread_export::export("m3", &row, &serde_json::json!({"messages": []}));
        std::fs::write(rig_dir.join("abc-1.json"), serde_json::to_vec(&view)?)?;
        std::fs::write(
            rig_dir.join(publish::INDEX_FILE),
            serde_json::to_vec(&vec![view.meta])?,
        )?;

        let index = signed(&origin, "/api/threads", now);
        assert_eq!(index.status, 200);
        let index: serde_json::Value = serde_json::from_slice(&index.body)?;
        assert_eq!(index["threads"][0]["id"], "abc-1");
        assert_eq!(index["threads"][0]["live"], true);

        assert_eq!(signed(&origin, "/api/threads/m3/abc-1", now).status, 200);
        assert_eq!(signed(&origin, "/api/threads/m3/nope", now).status, 404);
        assert_eq!(signed(&origin, "/api/threads/..%2F/abc-1", now).status, 400);
        assert_eq!(signed(&origin, "/api/threads/m3/..", now).status, 400);
        assert_eq!(signed(&origin, "/etc/passwd", now).status, 404);
        Ok(())
    }
}
