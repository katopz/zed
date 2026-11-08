use agent_mail::{Address, Mailroom, decode_payload, render};
use anyhow::{Context as _, Result, bail};
use std::io::{IsTerminal as _, Read as _};

const USAGE: &str = "\
agent-mail — message coding agents on other machines (and this one) over SSH

USAGE:
  agent-mail send <name@rig | *@rig | all@rig> [--from NAME] [--reply-to ID] [--at WHEN]
                       [--mode urgent|steering|fyi|queued] [MESSAGE...]
                       MESSAGE defaults to stdin; `*@rig` = first free agent there (ONE agent);
                       `all@rig` = broadcast, every agent on that rig gets a copy — refused
                       with the default mode (see below): broadcasts must say `fyi` (no wake)
                       or `urgent`/`steering` (wakes everyone) explicitly.
                       --at schedules it (30m, 8h, 2d, 23:45, or RFC 3339): it lands, and
                       wakes the recipient, only then. Send to yourself for a timed wake-up
                       instead of a sleep loop.
                       --mode (default queued): urgent interrupts a mid-turn recipient's
                       running turn and delivers now; steering ends its turn at the next
                       message boundary and delivers then; queued waits for the turn to end.
                       fyi never wakes an idle recipient: it is parked and read at its next
                       natural turn boundary — the right mode for broadcast FYIs. Cost note:
                       on a busy rig each ACTIVE agent still spends one boundary turn per
                       fyi batch; idle ones pay nothing.
                       The receipt names the `*@rig` agent that actually took the mail.
  agent-mail recv [--as NAME]...   claim and print your mail (also the rig-wide `*` box)
  agent-mail peek [--as NAME]...   print pending mail without claiming it
  agent-mail scheduled [--as NAME]...  print your scheduled (not yet due) mail
  agent-mail whoami                your address (Claude session name, or $AGENT_MAIL_NAME)
  agent-mail rigs                  list rigs and time an SSH round trip to each
  agent-mail init <rig>            name this rig in ~/.agent-mail/rigs.json
  agent-mail claude-hook [--wait]  Claude Code hook (PostToolUse/UserPromptSubmit; --wait for async Stop)
  agent-mail top [SECS]            terminal UI: rigs, jobs, live job log, mail (read-only)
  agent-mail rig status [RIG...] [--json] [--watch [SECS]]
                                   what every rig is busy with: heavy processes (riir-ai's
                                   contender rules), agents, load, memory, disk, power, GPU,
                                   and whether it is QUIET enough for a benchmark
  agent-mail rig run <RIG|auto> [--shared|--exclusive] [--key K] [--title T] [--cores N]
                [--mem GB] [--gpu-mem GB] [--needs cuda,metal] [--timeout 2h]
                [--worktree repo@sha] [--cwd DIR] [--notify name@rig] [--notify-mode M] -- <command>
                                   queue a job: shared jobs run when resources fit, an
                                   exclusive one waits for a quiet empty rig; mail on
                                   start/poison/end; same --key twice = one run (amo referees)
  agent-mail rig cancel <RIG> <ID> | rig log <RIG> <ID> [LINES]
  agent-mail rig watch [--once|--force]   orchestrator watchdog tick (zero tokens unless an
                                   incident is new); --install writes the launchd job,
                                   --interval MIN sets the tick interval (default 10)
  agent-mail flush                 retry mail parked for unreachable rigs now
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
            if let (Some(name), Ok(session_id)) =
                (&self_name, std::env::var("CLAUDE_CODE_SESSION_ID"))
            {
                for alias in room.session_aliases(&session_id, Some(name)) {
                    if !names.contains(&alias) {
                        names.push(alias);
                    }
                }
            }
            for name in explicit {
                names.push(agent_mail::validate_name(&name)?.to_string());
            }
            let reader = self_name.clone().or_else(|| {
                names
                    .iter()
                    .find(|name| name.as_str() != agent_mail::ANY_NAME)
                    .cloned()
            });
            let mut mail = match command {
                "recv" => room.claim(&names)?,
                "scheduled" => room.scheduled(&names)?,
                _ => room.peek(&names)?,
            };
            if let Some(reader) = &reader {
                match command {
                    "recv" => mail.extend(room.take_broadcasts(reader)),
                    "peek" => mail.extend(room.peek_broadcasts(reader)),
                    _ => {}
                }
            }
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
            // The resolved-to line goes BEFORE the `delivered` marker: the
            // sender's ssh reader returns the pre-marker lines (issue 046).
            if let Some(name) = room.deliver(&envelope)? {
                println!("resolved-to {name}");
            }
            println!("delivered {}", envelope.id);
            Ok(0)
        }
        Some("top") => {
            let every: u64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(15);
            #[cfg(feature = "tui")]
            agent_mail::tui::run(&room, std::time::Duration::from_secs(every.max(5)))?;
            #[cfg(not(feature = "tui"))]
            bail!("built without the `tui` feature; use `agent-mail rig status --watch {every}`");
            Ok(0)
        }
        Some("rig") => match args.next().as_deref() {
            Some("probe") => {
                let probe = agent_mail::rig::cached_probe(&room);
                println!("{}", serde_json::to_string(&probe)?);
                println!("{}", agent_mail::rig::PROBE_END);
                Ok(0)
            }
            Some("status") => {
                let mut json = false;
                let mut watch: Option<u64> = None;
                let mut only = Vec::new();
                let mut args = args.peekable();
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--json" => json = true,
                        "--watch" => {
                            watch = Some(
                                args.next_if(|next| next.parse::<u64>().is_ok())
                                    .and_then(|next| next.parse().ok())
                                    .unwrap_or(15),
                            )
                        }
                        "all" => {}
                        rig => only.push(rig.to_string()),
                    }
                }
                if let Some(every) = watch {
                    // Plain redraw loop for terminals where `top` is too much.
                    loop {
                        let results = agent_mail::rig::status(&room, &only)?;
                        let mail = room.mailbox_summary(5);
                        print!("\x1b[2J\x1b[H");
                        println!(
                            "agent-mail rig status · {} · every {every} s (Ctrl-C to stop)\n",
                            chrono::Local::now().format("%H:%M:%S")
                        );
                        print!("{}", agent_mail::rig::render(&results, &room.rigs()?));
                        println!("\nmail on {}:", room.rigs()?.self_rig);
                        for inbox in &mail.inboxes {
                            println!(
                                "  {:<20} {} new, {} scheduled",
                                inbox.name, inbox.pending, inbox.scheduled
                            );
                        }
                        for envelope in mail.broadcasts.iter().take(3) {
                            println!(
                                "  broadcast {} {}: {}",
                                envelope.sent_at.get(11..16).unwrap_or(""),
                                envelope.from,
                                envelope.body.lines().next().unwrap_or("")
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_secs(every.max(5)));
                    }
                }
                room.flush_outbox();
                let results = agent_mail::rig::status(&room, &only)?;
                match json {
                    true => {
                        let rows: Vec<serde_json::Value> = results
                            .iter()
                            .map(|(rig, outcome, elapsed)| match outcome {
                                Ok(probe) => serde_json::json!({
                                    "rig": rig, "online": true,
                                    "elapsed_ms": elapsed.as_millis(), "probe": probe,
                                }),
                                Err(error) => serde_json::json!({
                                    "rig": rig, "online": false,
                                    "elapsed_ms": elapsed.as_millis(), "error": format!("{error:#}"),
                                }),
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                    }
                    false => print!("{}", agent_mail::rig::render(&results, &room.rigs()?)),
                }
                Ok(0)
            }
            Some("run") => rig_run(&room, args.collect()),
            Some("enqueue") => {
                let spec: agent_mail::jobs::JobSpec =
                    agent_mail::jobs::decode(&args.next().context("enqueue needs a payload")?)?;
                room.enqueue_job(&spec)?;
                println!("queued {}", spec.id);
                Ok(0)
            }
            Some("kick") => {
                room.kick_until_drained()?;
                Ok(0)
            }
            Some("exec") => {
                room.execute_job(&args.next().context("exec needs a job id")?)?;
                Ok(0)
            }
            Some(command @ ("cancel" | "log")) => {
                let target = args.next().context("which rig?")?;
                let id = args.next().context("which job id?")?;
                let lines: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(40);
                let action = match command {
                    "cancel" => agent_mail::jobs::JobAction::Cancel,
                    _ => agent_mail::jobs::JobAction::Log(lines),
                };
                println!("{}", room.job_on_rig(&target, &id, action)?);
                Ok(0)
            }
            Some("cancel-local") => {
                println!("{}", room.cancel_job(&args.next().context("job id")?)?);
                println!("{END_MARKER}");
                Ok(0)
            }
            Some("log-local") => {
                let id = args.next().context("job id")?;
                let lines: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(40);
                println!("{}", room.job_log(&id, lines)?);
                println!("{END_MARKER}");
                Ok(0)
            }
            Some("key-claim") => {
                let claim: agent_mail::jobs::KeyClaim =
                    agent_mail::jobs::decode(&args.next().context("key-claim needs a payload")?)?;
                println!("{}", serde_json::to_string(&room.claim_key(&claim)?)?);
                println!("{}", agent_mail::jobs::KEY_VERDICT_END);
                Ok(0)
            }
            Some("key-release") => {
                let (key, summary): (String, String) =
                    agent_mail::jobs::decode(&args.next().context("key-release needs a payload")?)?;
                room.release_key(&key, &summary)?;
                println!("released");
                Ok(0)
            }
            Some("watch") => {
                let mut force = false;
                for arg in args.by_ref() {
                    match arg.as_str() {
                        "--once" => {}
                        "--force" => force = true,
                        "--install" => {
                            let path = agent_mail::watch::install_launchd(&room)?;
                            println!(
                                "wrote {}\nload it with: launchctl bootstrap gui/$(id -u) {}",
                                path.display(),
                                path.display()
                            );
                            return Ok(0);
                        }
                        "--interval" => {
                            let minutes: u64 = args
                                .next()
                                .context("--interval needs minutes")?
                                .parse()
                                .context("--interval needs whole minutes")?;
                            println!("wrote {}", room.set_watch_interval(minutes)?.display());
                            return Ok(0);
                        }
                        other => bail!("unexpected argument {other:?}"),
                    }
                }
                let report = agent_mail::watch::tick(&room, force)?;
                if report.ran {
                    println!(
                        "{} tick: {} event(s), {} incident(s), {} sent to the orchestrator, {} escalated",
                        chrono::Local::now().format("%Y-%m-%d %H:%M"),
                        report.events,
                        report.incidents,
                        report.woke,
                        report.escalated
                    );
                }
                Ok(0)
            }
            Some("zed-wake") => {
                let mut once = false;
                let mut install = false;
                for arg in args.by_ref() {
                    match arg.as_str() {
                        "--once" => once = true,
                        "--install" => install = true,
                        other => bail!("unexpected argument {other:?}"),
                    }
                }
                let _ = once; // --once is the invocation shape; a tick runs either way
                if install {
                    let path = agent_mail::zed_wake::install_launchd(&room)?;
                    println!(
                        "wrote {}\nload it with: launchctl bootstrap gui/$(id -u) {}",
                        path.display(),
                        path.display()
                    );
                    return Ok(0);
                }
                let report = agent_mail::zed_wake::tick(&room, chrono::Utc::now())?;
                if report.woke {
                    println!(
                        "zed-wake: launched Zed for {}",
                        report.candidates.join(", ")
                    );
                } else if report.candidates.is_empty() {
                    println!("zed-wake: nothing waiting");
                } else {
                    println!(
                        "zed-wake: {} waiting (cooldown or a live Zed)",
                        report.candidates.join(", ")
                    );
                }
                Ok(0)
            }
            _ => bail!(
                "usage: agent-mail rig status [RIG...|all] [--json] | rig watch [--once|--force|--install|--interval MIN] | rig zed-wake [--once|--install]"
            ),
        },
        Some("flush") => {
            let report = room.flush_outbox();
            println!(
                "outbox: {} delivered, {} still waiting",
                report.delivered, report.pending
            );
            for (rig, count, oldest) in room.outbox_status() {
                println!(
                    "  {rig}: {count} waiting (oldest sent {})",
                    oldest.unwrap_or_default()
                );
            }
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

use agent_mail::jobs::END_MARKER;

fn rig_run(room: &Mailroom, args: Vec<String>) -> Result<i32> {
    use agent_mail::jobs::{JobSpec, KeyClaim, KeyOutcome, Lane, Needs};
    let mut args = args.into_iter();
    let target = args.next().context("rig run needs a rig name or `auto`")?;
    let mut lane = Lane::Shared;
    let mut needs = Needs {
        cores: 1.0,
        mem_gb: 1.0,
        ..Needs::default()
    };
    let (mut key, mut title, mut cwd, mut notify, mut timeout, mut worktree) =
        (None, None, None, None, None, None);
    let mut notify_mode = agent_mail::MailMode::Queued;
    let mut argv = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--shared" => lane = Lane::Shared,
            "--exclusive" => lane = Lane::Exclusive,
            "--key" => key = Some(value("--key")?),
            "--title" => title = Some(value("--title")?),
            "--cwd" => cwd = Some(value("--cwd")?),
            "--notify" => notify = Some(value("--notify")?),
            "--notify-mode" => {
                notify_mode = agent_mail::MailMode::parse(&value("--notify-mode")?)?;
            }
            "--worktree" => worktree = Some(value("--worktree")?),
            "--timeout" => {
                timeout = Some(agent_mail::jobs::parse_duration(&value("--timeout")?)?.as_secs())
            }
            "--cores" => {
                needs.cores = value("--cores")?
                    .parse()
                    .context("--cores needs a number")?
            }
            "--mem" => needs.mem_gb = value("--mem")?.parse().context("--mem needs GB")?,
            "--gpu-mem" => {
                needs.gpu_mem_gb = value("--gpu-mem")?.parse().context("--gpu-mem needs GB")?
            }
            "--needs" => {
                needs.caps = value("--needs")?
                    .split(',')
                    .map(|cap| cap.trim().to_string())
                    .filter(|cap| !cap.is_empty())
                    .collect()
            }
            "--" => {
                argv.extend(args.by_ref());
                break;
            }
            other => bail!("unexpected argument {other:?} (put the command after `--`)"),
        }
    }
    anyhow::ensure!(
        !argv.is_empty(),
        "no command: `agent-mail rig run <rig|auto> [options] -- <command>`"
    );
    let rigs = room.rigs()?;
    let notify = notify.or_else(|| {
        room.whoami()
            .ok()
            .map(|name| format!("{name}@{}", rigs.self_rig))
    });
    let rig_name = match target.as_str() {
        "auto" => {
            let results = agent_mail::rig::status(room, &[])?;
            agent_mail::jobs::pick_rig(&results, &rigs, &needs, lane)?
        }
        name => name.to_string(),
    };
    let rig = rigs
        .rigs
        .get(&rig_name)
        .with_context(|| format!("unknown rig {rig_name:?}"))?;
    anyhow::ensure!(
        rig.accepts_jobs,
        "rig {rig_name} takes no jobs (owner rule)"
    );
    let missing: Vec<&String> = needs
        .caps
        .iter()
        .filter(|cap| !rig.caps.contains(cap))
        .collect();
    anyhow::ensure!(missing.is_empty(), "rig {rig_name} lacks {missing:?}");
    let spec = JobSpec {
        id: uuid::Uuid::now_v7().to_string(),
        lane,
        title: title.unwrap_or_else(|| argv.join(" ").chars().take(80).collect()),
        argv,
        cwd,
        needs,
        key: key.clone(),
        notify: notify.clone(),
        notify_mode,
        timeout_secs: timeout,
        worktree,
        submitted_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Some(key) = &key {
        let claim = KeyClaim {
            key: key.clone(),
            rig: rig_name.clone(),
            id: spec.id.clone(),
            notify: notify.clone(),
        };
        match room.claim_key_remote(&claim) {
            Ok(KeyOutcome::Duplicate { rig, id }) => {
                println!(
                    "DUPLICATE: key `{key}` is already job {id} on {rig}; not starting another. \
                     You are subscribed: its result mail comes to {}.",
                    notify.as_deref().unwrap_or("(no address)")
                );
                return Ok(0);
            }
            Ok(KeyOutcome::Claimed) => {}
            Err(error) => {
                eprintln!("DEDUP-DEGRADED (orchestrator unreachable, key not checked): {error:#}")
            }
        }
    }
    let queued = match rig_name == rigs.self_rig {
        true => room.enqueue_job(&spec),
        false => {
            let command = format!(
                "{} rig enqueue {}",
                rig.bin.as_deref().unwrap_or("agent-mail"),
                agent_mail::jobs::encode_spec(&spec)?
            );
            let mut result = Err(anyhow::anyhow!("rig {rig_name} has no ssh route"));
            for route in &rig.ssh {
                result = room.ssh_lines(route, &command, "queued ").map(|_| ());
                if result.is_ok() {
                    break;
                }
            }
            result
        }
    };
    if let Err(error) = queued {
        if let Some(key) = &key {
            room.release_key_after_failed_enqueue(key);
        }
        return Err(error.context(format!("could not queue on {rig_name}")));
    }
    println!(
        "queued job {} on {rig_name} ({:?} lane). Mail to {} when it starts and ends; \
         `agent-mail rig status {rig_name}` shows it.",
        spec.id,
        spec.lane,
        notify.as_deref().unwrap_or("(nobody: pass --notify)")
    );
    Ok(0)
}

fn send(room: &Mailroom, args: Vec<String>) -> Result<i32> {
    let mut args = args.into_iter();
    let to = Address::parse(&args.next().context("send needs a recipient")?)?;
    let mut from = None;
    let mut reply_to = None;
    let mut deliver_at = None;
    let mut mode = agent_mail::MailMode::Queued;
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => from = Some(args.next().context("--from needs a name")?),
            "--reply-to" => reply_to = Some(args.next().context("--reply-to needs an id")?),
            "--at" => {
                let when = args.next().context("--at needs a time (30m, 8h, 23:45)")?;
                deliver_at = Some(agent_mail::parse_when(&when, chrono::Local::now())?);
            }
            "--mode" => {
                let raw = args
                    .next()
                    .context("--mode needs urgent, steering, fyi or queued")?;
                mode = agent_mail::MailMode::parse(&raw)?;
            }
            "--fyi" => mode = agent_mail::MailMode::Fyi,
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
    agent_mail::ensure_broadcast_mode(&to, mode)?;
    let mut envelope = room.compose(&to, &from, body, reply_to)?;
    envelope.deliver_at = deliver_at.map(|at| at.to_rfc3339());
    envelope.mode = mode;
    let report = room.send(&envelope)?;
    if let Some(reason) = &report.queued {
        println!(
            "QUEUED {} for {}: rig unreachable, parked in the outbox and retried automatically \
             (`agent-mail flush` to retry now)\n  {}",
            report.id,
            report.to,
            reason.replace('\n', "\n  ")
        );
        return Ok(0);
    }
    let mode_note = match mode {
        agent_mail::MailMode::Queued => String::new(),
        mode => format!(", mode {}", mode.as_str()),
    };
    let resolved_note = report
        .resolved_to
        .map(|name| format!(" (took it: {name})"))
        .unwrap_or_default();
    println!(
        "sent {} to {}{}{}{}{} in {} ms",
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
        mode_note,
        resolved_note,
        report.elapsed.as_millis()
    );
    Ok(0)
}
