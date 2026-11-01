use agent_mail::Mailroom;
use anyhow::{Context as _, Result, bail};
use fleet_web::{origin, publish, signature::OriginKey};
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
fleet-web — thread history + rig queues on the web (plan 051)

USAGE:
  fleet-web serve [--bind 127.0.0.1:8787]
                       the read-only origin behind the Fastly edge (run on the
                       orchestrator rig, exposed with `tailscale funnel 8787`)
  fleet-web publish [--db PATH] [--to RIG] [--every SECS]
                       mirror this rig's Zed threads to the origin rig (default:
                       rigs.json `orchestrator`); only changed threads are sent;
                       --every keeps running
  fleet-web key        print the origin key (hex) for the Fastly secret store

State: $AGENT_MAIL_HOME/web (default ~/.agent-mail/web).";

fn main() {
    if let Err(error) = run(std::env::args().skip(1).collect()) {
        eprintln!("fleet-web: {error:#}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<()> {
    let mailroom = Mailroom::from_env()?;
    let web_home = mailroom.home().join("web");
    let key_path = web_home.join("origin.key");
    let (command, flags) = match args.split_first() {
        Some((command, flags)) => (command.as_str(), flags),
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };
    match command {
        "serve" => {
            let bind = flag(flags, "--bind")?.unwrap_or_else(|| "127.0.0.1:8787".to_string());
            let key = OriginKey::load_or_create(&key_path)?;
            origin::serve(origin::Origin::new(mailroom, web_home, key), &bind)
        }
        "publish" => {
            let db = match flag(flags, "--db")? {
                Some(path) => PathBuf::from(path),
                None => publish::default_threads_db().context("no Zed data dir; pass --db")?,
            };
            let every = flag(flags, "--every")?
                .map(|seconds| seconds.parse::<u64>().context("--every SECS"))
                .transpose()?;
            loop {
                if let Err(error) = publish_once(&mailroom, &web_home, &db, flag(flags, "--to")?) {
                    match every {
                        Some(_) => eprintln!("fleet-web: {error:#}"),
                        None => return Err(error),
                    }
                }
                match every {
                    Some(seconds) => std::thread::sleep(Duration::from_secs(seconds.max(30))),
                    None => return Ok(()),
                }
            }
        }
        "key" => {
            OriginKey::load_or_create(&key_path)?;
            println!("{}", std::fs::read_to_string(&key_path)?.trim());
            Ok(())
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn publish_once(
    mailroom: &Mailroom,
    web_home: &std::path::Path,
    db: &std::path::Path,
    to: Option<String>,
) -> Result<()> {
    let rigs = mailroom.rigs()?;
    let rig = agent_mail::validate_name(&rigs.self_rig)?.to_string();
    let origin_rig = to
        .or(rigs.orchestrator)
        .context("no origin rig: pass --to or set `orchestrator` in rigs.json")?;
    let (report, files) = publish::export_changed(db, &rig, web_home)?;
    for failure in &report.failed {
        eprintln!("fleet-web: skipped {failure}");
    }
    let shipped = match origin_rig == rig {
        true => 0,
        false => publish::ship(mailroom, &origin_rig, &rig, web_home, files)?,
    };
    println!(
        "{rig}: {} threads, {} exported, {} removed, {} failed, {shipped} files → {origin_rig}",
        report.total,
        report.exported,
        report.removed,
        report.failed.len()
    );
    Ok(())
}

fn flag(flags: &[String], name: &str) -> Result<Option<String>> {
    match flags.iter().position(|flag| flag == name) {
        Some(index) => flags
            .get(index + 1)
            .cloned()
            .map(Some)
            .with_context(|| format!("{name} needs a value")),
        None => Ok(None),
    }
}
