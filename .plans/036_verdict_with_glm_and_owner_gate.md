# 036 — "Verdict with GLM" secondary reviewer + owner-gate decisions
Status: IMPLEMENTED — clippy + unit tests green; live GLM click-through pending

## Why
- Only one verdict reviewer existed ("Verdict with Claude" via the `claude_code`
  backend). A second opinion from GLM is wanted next to it.
- GLM is an `openai_compatible` provider with 4 keys. Routing the GLM review as a
  native subagent pinned to the GLM model means the provider's existing multi-key
  rotation (`open_ai_compatible.rs`, 429 backoff + `reset_key_session`) applies.
  A single shell `curl` with one key does not rotate, which is why an earlier
  out-of-band GLM review failed on one key's 429 (code 1310) even though 3 keys
  were healthy.
- Agents keep parking work as "owner gated" and stopping. The verdict prompt now
  asks the reviewer for an `#OwnerGate` decision per parked item, and the worker
  must act on those decisions.

## Design (substrate reused, no parallel system)
- Setting `agent.verdict_secondary_model: LanguageModelSelection` (default null).
  Unset hides the secondary entry. The label is `Verdict with <provider>`.
- `request_verdict` input gains `reviewer: "default" | "secondary"` (default
  `default`). `secondary` → `ReviewerRoute::Secondary(model)`, which runs on the
  existing native subagent path with a model override. It never falls back to
  the default reviewer: if no model is set, it returns an error.
- `ThreadEnvironment::create_verdict_subagent` takes `model_override`.
- UI: a second icon button (`UserGroup`) after the primary verdict button, plus a
  second right-click entry. Both reuse `handle_request_verdict_click(reviewer)`
  and `request_verdict(reviewer, ...)`.
- Prompt: the `#Verdict:` first line stays mandatory, so `parse_verdict` is
  unchanged. An optional `#OwnerGate` section holds lines of the form
  `- <item>: DO <action> | SKIP | DEFER — reason`. After closing, the worker
  executes `DO` items and records `SKIP`/`DEFER` items. Destructive,
  irreversible, paid, or outward-publishing actions stay with the user.

## Tasks
- [x] settings_content + agent_settings field
- [x] tool route + `RequestedReviewer` input + unit test
- [x] env trait `model_override`
- [x] UI button + context menu entry + owner-gate prompt
- [x] user settings: `verdict_secondary_model = GLM/glm-5.3`
- [x] clippy + `request_verdict_tool` tests green (7/7 incl. `secondary_reviewer_routes_to_its_model_and_never_falls_back`)
- [-] live check: click "Verdict with GLM" on a summary and confirm keys rotate on a 429 — needs a rebuilt Zed running with the GLM keys (a manual UI step); the routing is unit-tested and the key rotation is the existing `open_ai_compatible` path
