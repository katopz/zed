# GLM endpoint rejects non-text content parts + WAF 405 blocks

status: closed 2026-09-20 (1210 fixed in c876a4ef8a + c2df6463d3; WAF item deferred
— endpoint-operator config, not reproducible, owner call)

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
- [-] Owner: tune Aliyun WAF rules for the GLM endpoint (405 HTML block pages
      abort whole turns; log tag `[acp_thread] Error in run turn`). **Deferred
      2026-09-20** — the fix lives in Z.ai/Aliyun WAF config, not in this repo,
      and there is nothing left to patch in Zed. No recurrence in the retained
      logs: `Zed.log` + `Zed.log.old` contain zero `405` / `Method Not Allowed`
      HTTP responses (every `405` hit is a plan filename). Reopen only if a
      fresh HTML block page shows up — capture the response body + Aliyun trace
      id from `~/Library/Logs/Zed/Zed.log` and hand it to the endpoint operator.
- [x] Title/summary 1210 — fixed in c876a4ef8a.

## Tombstone note

Kept (not deleted) because this is the only write-up of *why* image
sanitization is capability-gated on `model.supports_images()` and applied at
the `stream_completion_with_retry` choke point rather than per-provider.

Unrelated-but-adjacent, for whoever greps this next: GLM turns can also abort
with `error sending HTTP request to GLM API` (transport/connect failure, seen
3× on 2026-09-20). That is **not** the WAF 405 signature — don't reopen this
issue for it.
