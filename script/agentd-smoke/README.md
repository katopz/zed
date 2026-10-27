# agentd smoke tests

End-to-end checks for `eval-cli --acp` (Zed's native agent served over ACP
stdio) and for running it detached under `zed-agentd` (`.issues/036`, plan 037).
A scripted OpenAI-compatible server (`fake_model.py`) stands in for the model,
so no API key is needed and every run is deterministic.

Each run uses a throwaway `HOME` and `ZED_AGENTD_HOME`, so nothing touches your
real Zed settings or agentd journal.

```sh
cargo build -p eval_cli -p agentd
# Direct: initialize, session/new, a read_file turn, a terminal turn with a permission round-trip,
# an edit_file turn that must arrive as a structured ACP diff, and a model switch via
# session/set_config_option that the next turn must use.
python3 -I script/agentd-smoke/acp_smoke.py target/debug/eval-cli fake/m 18736
# Same, through zed-agentd, plus a second client that lists and replays the session.
SMOKE_AGENTD=target/debug/zed-agentd python3 -I script/agentd-smoke/acp_smoke.py target/debug/eval-cli fake/m 18737
# Kill the client mid-turn; a client reattaching mid-turn is told the turn runs and when it ends;
# the turn finishes detached and replays on a later reattach. Prints PASS or FAIL.
python3 -I script/agentd-smoke/detach_smoke.py target/debug/eval-cli target/debug/zed-agentd 18738
# Same, but every client goes through `remote_server exec` and the agentd embedded in the
# remote server binary, the way Zed launches it on a host (cargo build -p remote_server first).
SMOKE_VIA_REMOTE_SERVER=1 python3 -I script/agentd-smoke/detach_smoke.py target/debug/eval-cli target/debug/remote_server 18739
```

The last argument is a free localhost port for the fake model.

## On a real remote host over SSH (plan 037 P3)

`SMOKE_SSH` (an ssh command prefix) runs the same detach check against a remote
Linux host: the binaries, the fake model and the project live on the host, and
each client is a local `ssh` process running `remote_server exec` there, so
killing a client drops a real SSH connection. Build static Linux binaries the
way Zed builds its dev remote server (needs `zig` and `cargo-zigbuild`); a debug
`eval-cli` must embed its assets because the host has no checkout:

```sh
export RUSTFLAGS=" -C target-feature=+crt-static"
cargo zigbuild -p remote_server --features debug-embed --target-dir target/remote_server --target x86_64-unknown-linux-musl
cargo zigbuild -p eval_cli --features util/debug-embed --target-dir target/remote_server --target x86_64-unknown-linux-musl
# Copy both binaries from target/remote_server/x86_64-unknown-linux-musl/debug/ to the host
# (strip and xz them first over a slow link), then:
SMOKE_VIA_REMOTE_SERVER=1 SMOKE_SSH="ssh my-host" \
  python3 -I script/agentd-smoke/detach_smoke.py /path/on/host/eval-cli /path/on/host/remote_server 18740
```

A WSL distro on a Windows box works as the host without opening any port:
run `wsl_sshd_setup.sh <your public key file>` inside the distro once, then
reach it through the Windows host's own sshd:

```
Host my-wsl
    User <wsl user>
    ProxyCommand ssh <windows user>@<windows host> wsl -d Ubuntu -- /usr/sbin/sshd -i -f /home/<wsl user>/.zed-sshd/sshd_config
```
