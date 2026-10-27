use agentd::attach::{attach, query};
use agentd::daemon::{Paths, serve};
use agentd::types::{BackendSpec, DetachedPolicy, Handshake};
use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;

const USAGE: &str = "\
zed-agentd — keeps ACP agents running on this host while the Zed GUI detaches

USAGE:
  zed-agentd attach [--detached-permission wait|allow|deny] [--env KEY=VALUE]... [--pass-env KEY]... -- <agent> [args...]
  zed-agentd serve     run the daemon in the foreground (attach starts it automatically)
  zed-agentd status    print sessions as JSON
  zed-agentd stop      stop the daemon (running agents are terminated)

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
