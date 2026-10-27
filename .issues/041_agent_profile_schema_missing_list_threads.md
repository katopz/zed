# settings: `test_agent_profile_tool_schema` fails on `list_threads`
Status: open — found 2026-10-27 while testing plan 039; not caused by it

## Problem

`cargo test -p settings --lib test_agent_profile_tool_schema` fails:

```
write tool list_threads should be suggested in the schema
```

`assets/settings/default.json` lists `list_threads` in the built-in `write`
profile's tools since `4765075d04` (plan 035, native agent `send_message` and
`list_threads` tools), but the agent profile tools schema
(`$defs/AgentProfileContent/properties/tools`) doesn't suggest it, so the
default profile has a tool the schema rejects (`additionalProperties: false`).

## Tasks

- [ ] Find where the profile tools schema gets its tool names and add
      `list_threads` (and check `send_message`, added in the same commit)
- [ ] `cargo test -p settings --lib` passes
