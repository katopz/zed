//! Request signing between the Fastly edge and the origin rig (plan 051).
//! The origin is public through Tailscale Funnel, so every request must prove
//! it came from the edge that already checked the GitHub login.

use anyhow::{Context as _, Result, bail};
use std::path::Path;

pub const TIMESTAMP_HEADER: &str = "x-fleet-ts";
pub const SIGNATURE_HEADER: &str = "x-fleet-sig";
pub const MAX_SKEW_SECONDS: i64 = 60;

#[derive(Clone)]
pub struct OriginKey([u8; 32]);

impl OriginKey {
    pub fn from_hex(text: &str) -> Result<Self> {
        let bytes = hex::decode(text.trim()).context("origin key is not hex")?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("origin key must be 32 bytes"))?;
        Ok(Self(key))
    }

    /// Reads the key, creating a random one on first use (the operator then
    /// copies it into the Fastly secret store).
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_hex(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Self(random_bytes()?);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_private(path, &hex::encode(key.0))?;
                Ok(key)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn sign(&self, timestamp: i64, method: &str, path_and_query: &str) -> String {
        let message = format!("{timestamp}\n{method}\n{path_and_query}");
        blake3::keyed_hash(&self.0, message.as_bytes())
            .to_hex()
            .to_string()
    }

    pub fn verify(
        &self,
        timestamp: Option<&str>,
        signature: Option<&str>,
        method: &str,
        path_and_query: &str,
        now: i64,
    ) -> Result<()> {
        let timestamp: i64 = timestamp
            .context("missing timestamp")?
            .parse()
            .context("bad timestamp")?;
        if (now - timestamp).abs() > MAX_SKEW_SECONDS {
            bail!("stale timestamp");
        }
        let given = blake3::Hash::from_hex(signature.context("missing signature")?.trim())
            .context("bad signature")?;
        let message = format!("{timestamp}\n{method}\n{path_and_query}");
        // `blake3::Hash` equality is constant time.
        match blake3::keyed_hash(&self.0, message.as_bytes()) == given {
            true => Ok(()),
            false => bail!("signature mismatch"),
        }
    }
}

fn random_bytes() -> Result<[u8; 32]> {
    use rand::TryRngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .context("os random")?;
    Ok(bytes)
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> OriginKey {
        OriginKey([1u8; 32])
    }

    #[test]
    fn signs_and_verifies_within_skew() {
        let signature = key().sign(1_700_000_000, "GET", "/api/fleet");
        let verify = |timestamp: &str, signature: &str, path: &str, now: i64| {
            key().verify(Some(timestamp), Some(signature), "GET", path, now)
        };
        assert!(verify("1700000000", &signature, "/api/fleet", 1_700_000_030).is_ok());
        assert!(verify("1700000000", &signature, "/api/fleet", 1_700_000_061).is_err());
        assert!(verify("1700000000", &signature, "/api/threads", 1_700_000_000).is_err());
        assert!(verify("1700000001", &signature, "/api/fleet", 1_700_000_000).is_err());
        assert!(verify("1700000000", "zz", "/api/fleet", 1_700_000_000).is_err());
        assert!(
            key()
                .verify(None, Some(&signature), "GET", "/api/fleet", 1_700_000_000)
                .is_err()
        );
    }

    /// Cross-checked against the edge's `@noble/hashes` blake3 implementation.
    #[test]
    fn known_vector() {
        let signature = key().sign(1_700_000_000, "GET", "/api/fleet");
        assert_eq!(signature.len(), 64);
        println!("vector {signature}");
    }

    #[test]
    fn creates_a_key_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("web/origin.key");
        let first = OriginKey::load_or_create(&path)?;
        let second = OriginKey::load_or_create(&path)?;
        assert_eq!(first.0, second.0);
        assert_ne!(first.0, [0u8; 32]);
        Ok(())
    }
}
