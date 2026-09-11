# GLM endpoint rejects non-text content parts + WAF 405 blocks

status: open (follow-ups from 1210 title-gen fix c876a4ef8a; WAF item owner-gated)

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

- [ ] Main-turn path has the same latent hazard: if a user switches a thread
      from an image-capable model to a text-only model, replayed history still
      contains image parts → 1210 on the next turn. Consider applying
      `strip_unsupported_images` (or equivalent capability gating) to the main
      request build path in `crates/agent/src/thread.rs`.
- [ ] Owner: tune Aliyun WAF rules for the GLM endpoint (405 HTML block pages
      abort whole turns; log tag `[acp_thread] Error in run turn`).
- [x] Title/summary 1210 — fixed in c876a4ef8a (remove this file once both
      remaining boxes are closed).
