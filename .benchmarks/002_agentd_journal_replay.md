# Bench 002 — zed-agentd journal append + reattach replay (plan 037)
Status: DONE — GOAT promoted to default (cached append handles, −92% append)

Harness: `crates/agentd/benches/journal_replay.rs` (criterion), 10k updates per
session (one per streamed token, a `tool_call` every 500), M3 Max, APFS.

```sh
cargo bench -p agentd --bench journal_replay -- --warm-up-time 1 --measurement-time 3
```

| bench | before (open per append) | after (cached handle) | change |
|---|---:|---:|---:|
| `append_10k_chunks` | 249.7 ms (25 µs/token) | 19.7 ms (2 µs/token) | **−92.1%** |
| `read_10k_chunks` | 4.11 ms | 4.10 ms | noise |
| `coalesce_10k_chunks` | 4.26 ms | 4.23 ms | noise |
| `replay_10k_chunks` (read + coalesce) | 7.57 ms | 7.53 ms | noise |

## Verdict

- Append was the hot path: the hub thread journals every `session/update`
  inline, so 25 µs/token of open+close stalled forwarding to the GUI. Cached
  per-session `File` handles (cap 64, evict-all on overflow, drop on write
  error) cut it 12.7×. Semantics unchanged: still unbuffered `write_all`, so
  a crash loses at most the torn last line, which `updates()` already skips.
- Reattach replay of a 10k-token turn costs 7.5 ms — not worth optimizing
  further (dominated by `serde_json` parse; GUI render dwarfs it).
- No feature gate: pure perf with identical on-disk format and durability;
  covered by `file_journal_appends_survive_handle_eviction`.
- Follow-up constraint for plan 037 P4 `zed-agentd gc`: it must drop the
  cached handle before deleting a session log, or appends land in an unlinked inode.
