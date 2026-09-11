# GLM endpoint rejects non-text content parts + WAF 405 blocks

status: open (WAF item owner-gated; 1210 fully fixed in c876a4ef8a + c2df6463d3)

## Context

2026-09-11 investigation of a stuck agent thread (`~/Library/Logs/Zed/Zed.log`)
found the GLM (Zhipu) OpenAI-compatible endpoint rejecting requests in two
independent ways:

1. **Error 1210** `messages.content.type is invalid, allowed values: ['text']`
   — thread title/summary generation replayed full thread history (including
   pasted image parts) to the text-only summarization model. **FIXED** in
   c876a4ef8a: `strip_unsupported_images` at the `stream_completion_with_retry`
   choke point, capability-gated on `model.supports_images()`, with 3 tests.

2. **405 Method Not Allowed** with an Aliyun WAF block page (07:08 log entry)
   — the WAF fronting the endpoint intermittently blocks Zed's POST bodies as
   "potential threats". Owner-gated: needs WAF rule tuning / whitelisting on
   the endpoint operator side. Nothing to patch in Zed.

## Remaining follow-ups

- [x] Main-turn + compaction paths sanitized (was: latent hazard when a user
      switches a thread from an image-capable model to a text-only model).
      Fixed in c2df6463d3: `run_turn_internal` sanitizes against the model
      actually used (covers refusal fallback), `build_compaction_request`
      sanitizes against the compaction model; helper takes the request by
      value with an image-presence early-out so the hot path pays a scan, not
      a copy. 2 end-to-end tests + compaction suite green.
- [ ] Owner: tune Aliyun WAF rules for the GLM endpoint (405 HTML block pages
      abort whole turns; log tag `[acp_thread] Error in run turn`).
- [x] Title/summary 1210 — fixed in c876a4ef8a (remove this file once both
      remaining boxes are closed).
