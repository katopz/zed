use agentd::attach::{attach, query};
use agentd::daemon::{Paths, serve};
use agentd::types::{BackendSpec, DetachedPolicy, Handshake, RetentionPolicy};
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;

const USAGE: &str = "\
zed-agentd — keeps ACP agents running on this host while the Zed GUI detaches

USAGE:
  zed-agentd attach [--detached-permission wait|allow|deny] [--env KEY=VALUE]... [--pass-env KEY]... -- <agent> [args...]
  zed-agentd serve     run the daemon in the foreground (attach starts it automatically)
  zed-agentd status    print sessions as JSON
  zed-agentd stop      stop the daemon (running agents are terminated)
  zed-agentd gc [--max-age-days N] [--max-total-mb N]
                       remove idle sessions older than N days (default 30), then the
                       oldest idle ones until update logs fit N MiB (default 1024);
                       the daemon also runs this with the defaults when it starts

State lives in $ZED_AGENTD_HOME (default ~/.local/share/zed-agentd).";

fn main() {
    if let Err(error) = run() {
        eprintln!("zed-agentd: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let paths = Paths::from_env()?;
    match args.next().as_deref() {
        Some("attach") => attach(&paths, parse_attach(args.collect())?),
        Some("serve") => serve(&paths),
        Some("status") => {
            print!("{}", query(&paths, Handshake::Status)?);
            Ok(())
        }
        Some("gc") => {
            print!("{}", query(&paths, parse_gc(args.collect())?)?);
            Ok(())
        }
        Some("stop") => {
            print!("{}", query(&paths, Handshake::Stop)?);
            Ok(())
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn parse_gc(args: Vec<String>) -> Result<Handshake> {
    let mut policy = RetentionPolicy::default();
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        let value = rest
            .next()
            .with_context(|| format!("{arg} needs a number"))?;
        let number: u64 = value
            .parse()
            .with_context(|| format!("{arg} needs a number, got {value:?}"))?;
        match arg.as_str() {
            "--max-age-days" => policy.max_age_days = number,
            "--max-total-mb" => policy.max_total_bytes = number.saturating_mul(1024 * 1024),
            other => bail!("unexpected argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Handshake::Gc { policy })
}

fn parse_attach(args: Vec<String>) -> Result<Handshake> {
    let mut detached_permission = match std::env::var("ZED_AGENTD_DETACHED_PERMISSION").as_deref() {
        Ok("allow") => DetachedPolicy::Allow,
        Ok("deny") => DetachedPolicy::Deny,
        _ => DetachedPolicy::default(),
    };
    let mut env = BTreeMap::new();
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--" => break,
            "--detached-permission" => {
                detached_permission = match rest.next().as_deref() {
                    Some("wait") => DetachedPolicy::Wait,
                    Some("allow") => DetachedPolicy::Allow,
                    Some("deny") => DetachedPolicy::Deny,
                    other => bail!("invalid --detached-permission {other:?}"),
                }
            }
            "--env" => {
                let pair = rest.next().context("--env needs KEY=VALUE")?;
                let (key, value) = pair.split_once('=').context("--env needs KEY=VALUE")?;
                env.insert(key.to_string(), value.to_string());
            }
            "--pass-env" => {
                let name = rest.next().context("--pass-env needs a variable name")?;
                if let Ok(value) = std::env::var(&name) {
                    env.insert(name, value);
                }
            }
            other => bail!("unexpected argument {other:?}\n\n{USAGE}"),
        }
    }
    let command = rest.next().context("missing agent command after --")?;
    Ok(Handshake::Attach {
        backend: BackendSpec {
            command,
            args: rest.collect(),
            env,
        },
        detached_permission,
    })
}
