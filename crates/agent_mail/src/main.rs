use agent_mail::{Address, Mailroom, decode_payload, render};
use anyhow::{Context as _, Result, bail};
use std::io::{IsTerminal as _, Read as _};

const USAGE: &str = "\
agent-mail — message coding agents on other machines (and this one) over SSH

USAGE:
  agent-mail send <name@rig | *@rig> [--from NAME] [--reply-to ID] [--at WHEN] [MESSAGE...]
                       MESSAGE defaults to stdin; `*@rig` = first free agent there.
                       --at schedules it (30m, 8h, 2d, 23:45, or RFC 3339): it lands, and
                       wakes the recipient, only then. Send to yourself for a timed wake-up
                       instead of a sleep loop.
  agent-mail recv [--as NAME]...   claim and print your mail (also the rig-wide `*` box)
  agent-mail peek [--as NAME]...   print pending mail without claiming it
  agent-mail scheduled [--as NAME]...  print your scheduled (not yet due) mail
  agent-mail whoami                your address (Claude session name, or $AGENT_MAIL_NAME)
  agent-mail rigs                  list rigs and time an SSH round trip to each
  agent-mail init <rig>            name this rig in ~/.agent-mail/rigs.json
  agent-mail claude-hook [--wait]  Claude Code hook (PostToolUse/UserPromptSubmit; --wait for async Stop)
  agent-mail deliver <payload>     receiving side of a remote send (run over SSH)
  agent-mail version

State: $AGENT_MAIL_HOME (default ~/.agent-mail). Rigs: ~/.agent-mail/rigs.json.";

fn main() {
    match run(std::env::args().skip(1).collect()) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("agent-mail: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run(args: Vec<String>) -> Result<i32> {
    let room = Mailroom::from_env()?;
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("send") => send(&room, args.collect()),
        Some(command @ ("recv" | "peek" | "scheduled")) => {
            let mut names = vec![agent_mail::ANY_NAME.to_string()];
            let mut explicit = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--as" => explicit.push(args.next().context("--as needs a name")?),
                    other => bail!("unexpected argument {other:?}\n\n{USAGE}"),
                }
            }
            let self_name = match explicit.is_empty() {
                true => Some(room.whoami()?),
                false => None,
            };
            names.extend(self_name.iter().cloned());
            for name in explicit {
                names.push(agent_mail::validate_name(&name)?.to_string());
            }
            let mail = match command {
                "recv" => room.claim(&names)?,
                "scheduled" => room.scheduled(&names)?,
                _ => room.peek(&names)?,
            };
            match mail.is_empty() {
                true => println!("no mail"),
                false => print!("{}", render(&mail, self_name.as_deref())),
            }
            Ok(0)
        }
        Some("whoami") => {
            println!("{}@{}", room.whoami()?, room.rigs()?.self_rig);
            Ok(0)
        }
        Some("rigs") => {
            println!("self: {}", room.rigs()?.self_rig);
            for (rig, target, outcome) in room.ping_rigs()? {
                match outcome {
                    Ok(elapsed) => println!("{rig:<8} {target:<32} ok {} ms", elapsed.as_millis()),
                    Err(error) => println!("{rig:<8} {target:<32} FAIL {error:#}"),
                }
            }
            Ok(0)
        }
        Some("init") => {
            let rig = args.next().context("init needs this rig's name")?;
            println!("wrote {}", room.init(&rig)?.display());
            Ok(0)
        }
        Some("claude-hook") => {
            let wait = match args.next().as_deref() {
                None => false,
                Some("--wait") => true,
                Some(other) => bail!("unexpected argument {other:?}"),
            };
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let outcome = room.claude_hook(&input, wait)?;
            if let Some(stdout) = outcome.stdout {
                println!("{stdout}");
            }
            if let Some(stderr) = outcome.stderr {
                eprintln!("{stderr}");
            }
            Ok(outcome.exit_code)
        }
        Some("deliver") => {
            let envelope = decode_payload(&args.next().context("deliver needs a payload")?)?;
            room.deliver(&envelope)?;
            println!("delivered {}", envelope.id);
            Ok(0)
        }
        Some("version") => {
            println!("agent-mail {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        _ => {
            println!("{USAGE}");
            Ok(0)
        }
    }
}

fn send(room: &Mailroom, args: Vec<String>) -> Result<i32> {
    let mut args = args.into_iter();
    let to = Address::parse(&args.next().context("send needs a recipient")?)?;
    let mut from = None;
    let mut reply_to = None;
    let mut deliver_at = None;
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => from = Some(args.next().context("--from needs a name")?),
            "--reply-to" => reply_to = Some(args.next().context("--reply-to needs an id")?),
            "--at" => {
                let when = args.next().context("--at needs a time (30m, 8h, 23:45)")?;
                deliver_at = Some(agent_mail::parse_when(&when, chrono::Local::now())?);
            }
            _ => words.push(arg),
        }
    }
    let body = match words.is_empty() {
        false => words.join(" "),
        true if std::io::stdin().is_terminal() => {
            bail!("no message given (pass it as arguments or on stdin)")
        }
        true => {
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body)?;
            body
        }
    };
    let from = match from {
        Some(from) => from,
        None => room.whoami()?,
    };
    let mut envelope = room.compose(&to, &from, body, reply_to)?;
    envelope.deliver_at = deliver_at.map(|at| at.to_rfc3339());
    let report = room.send(&envelope)?;
    println!(
        "sent {} to {}{}{} in {} ms",
        report.id,
        report.to,
        report
            .via
            .map(|via| format!(" via {via}"))
            .unwrap_or_default(),
        deliver_at
            .map(|at| format!(
                ", delivered at {}",
                at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %Z")
            ))
            .unwrap_or_default(),
        report.elapsed.as_millis()
    );
    Ok(0)
}
