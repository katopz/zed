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
# and an edit_file turn that must arrive as a structured ACP diff.
python3 -I script/agentd-smoke/acp_smoke.py target/debug/eval-cli fake/m 18736
# Same, through zed-agentd, plus a second client that lists and replays the session.
SMOKE_AGENTD=target/debug/zed-agentd python3 -I script/agentd-smoke/acp_smoke.py target/debug/eval-cli fake/m 18737
# Kill the client mid-turn; the turn finishes detached and replays on reattach. Prints PASS or FAIL.
python3 -I script/agentd-smoke/detach_smoke.py target/debug/eval-cli target/debug/zed-agentd 18738
```

The last argument is a free localhost port for the fake model.
