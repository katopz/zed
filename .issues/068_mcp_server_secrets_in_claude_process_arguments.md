# 068 — MCP server secrets forwarded to Claude Code end up in its process arguments

Status: OPEN — filed 2026-11-16 (zed-b3@m3), owner-directed. Implementation in flight in the same session.

## What

Zed forwards every configured context server to an external agent on `session/new`
(`mcp_servers_for_project`), including HTTP headers and stdio env values verbatim. `claude-agent-acp`
hands them to the Claude CLI as one `--mcp-config <json>` argument, so the secrets sit in argv:

    ps -o command= -p <claude pid>   # shows "Authorization":"Bearer <z.ai key>" and "Z_AI_API_KEY":"..."

Observed 2026-11-16 on m3: an agent ran `ps` during an unrelated probe and the z.ai key landed in its
transcript (and therefore at the model provider). That is the demonstrated path: any agent that lists
processes captures the key. argv is also readable by every local user; env is not.

This is not Claude "calling GLM": the forwarded servers are z.ai's MCP tools (web reader, web search,
vision), handed to every external agent. Claude never talks to the GLM chat model.

## Severity

Hardening with one demonstrated leak path (process listing by an agent -> transcript). Prerequisite is
local code execution as the same user, which already reads `settings.json`; the marginal exposure is the
accidental one (transcripts, logs, other local users), not a privilege gain.

## Design

Claude Code expands `${VAR}` in MCP configs, including inline `--mcp-config` (verified 2026-11-16: args
and env both expanded). So for the Claude agent only:

- replace each header value and stdio env value with `${ZED_MCP_<SERVER>_<KEY>}`;
- send the real values as `_meta.claudeCode.options.env` on `session/new` / `load` / `resume`, which
  travels over the stdio JSON-RPC pipe and becomes process environment, never argv.

Names are deterministic so load/resume does not look like an options change (which rebuilds the session).
Other agents keep the literal values: placeholder expansion is a Claude Code behaviour.

Not covered (named, not fixed): secrets written in a stdio server's own `args` (they are that server's
argv by definition) and same-user `ps e` reading the environment.

## Tasks

- [ ] Placeholder indirection + env meta for the Claude agent (v1 new/load/resume)
- [ ] Tests: no secret value in the serialized `mcpServers`; env meta carries it; other agents unchanged
- [ ] Re-run the reproducer on a live session (`ps` shows placeholders, MCP tools still work)
- [ ] Skill telling Claude how to hand work to a Zed GLM agent (owner ask, same thread)
- [-] ACP v2 lane (`acp-beta` flag) — same change once v2 carries session `_meta`
- [-] Roll the z.ai key (owner): it is in one session transcript

## Refs

- `crates/agent_servers/src/acp/v1.rs` (`mcp_servers_for_project`), `crates/agent_servers/src/acp.rs`
