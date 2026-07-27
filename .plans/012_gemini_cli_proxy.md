# Plan 012: Gemini CLI proxy via logged-in browser session

## What was asked

> Refer to https://github.com/google-gemini/gemini-cli. We have
> https://zed.dev/docs/ai/external-agents#gemini-cli but it needs an API key
> which I don't have — I only have organizational access through
> https://gemini.google.com/. Mod it into an extension that:
> 1. Logs in via CLI with `katopz@maxion.game`.
> 2. Works like external-agents#gemini-cli but with a browser-like session.
> 3. May use https://github.com/h4ckf0r0day/obscura headless, or anything
>    else that works (e.g. https://github.com/mattsse/chromiumoxide).
> 4. Right-click "Ask Gemini" like the prior plan (commit `fefaa0ac` on the
>    `gemini_browser` branch — see prior plans 012/013 there for prior art).
> 5. New branch `gemini_cli_proxy` from `de146c3528c8ad00b023609d08cbc2a032620e41`.
> 6. Is this possible? If so, need a new plan.

## Critical correction to the premise

The premise "external-agents#gemini-cli needs an API key" is **factually
wrong**. Verified against both the official gemini-cli README and the live
Zed external-agents doc:

- gemini-cli supports three auth modes (quoting
  https://github.com/google-gemini/gemini-cli README):
  1. **Sign in with Google (OAuth, no API key)** — free tier, 60 req/min and
     1,000 req/day, just sign in with any Google account including Workspace.
  2. Gemini API key (paid / higher limits).
  3. Vertex AI (enterprise).
- The Zed doc (https://zed.dev/docs/ai/external-agents#gemini-cli) explicitly
  says: *"Gemini CLI owns its own authentication and may prompt you to log in
  with Google, Vertex AI, or another Gemini-supported flow."* It only passes
  `GEMINI_API_KEY` *if you already have one configured* — it does not require
  one.

So `katopz@maxion.game` can almost certainly use the legitimate, supported
path: install Gemini CLI from Zed's ACP Registry, start a thread, pick
"Sign in with Google", done. No browser proxy, no obscura, no scraping, no
ToS grey area. **This is the GOAT** for the common case.

## The only scenario where the proxy approach is justified

The proxy approach (browser-driven `gemini.google.com`) is justified **only
if** the OAuth flow fails because maxion.game's Workspace admin has blocked
third-party OAuth app access (a common strict-enterprise policy). In that
case the web UI is the user's only path and the proxy is the workaround.

This is cheap and free to verify before writing any code.

## Decision tree

```
Phase 0  (5 min, free, no code):
    In Zed: Agent Settings → External Agents → Install "Gemini CLI" from
    ACP Registry. Start a Gemini CLI thread. Choose "Sign in with Google".
    Login as katopz@maxion.game.
        |
        +---> SUCCESS: Plan is DONE. Use the stock integration.
        |     No `gemini_cli_proxy` branch needed. Close this plan.
        |
        +---> FAIL (org policy blocks OAuth / "Access blocked"):
              Proceed to Phase 1.
```

Everything below is conditional on Phase 0 failing.

## Scope decisions to confirm with the user before Phase 1

These are NOT decisions to make unilaterally — confirm before writing code.

1. **Revive `gemini_browser` or start fresh on `gemini_cli_proxy`?**
   The `gemini_browser` branch (tip `fefaa0ac`, branched off the same
   `de146c3` base) already delivers:
   - Hand-written CDP client over `async-tungstenite`/smol
     (`crates/gemini_browser/src/cdp.rs`, 412 lines) — deliberately avoided
     `chromiumoxide` because it's tokio-based and would have meant hosting a
     tokio runtime inside a smol process for four CDP domains' worth of use.
   - Gemini page automation with candidate selectors, `Input.insertText`
     (not JS text assignment, because Gemini's composer is a Quill
     `contenteditable`), text-stability response-completion detection
     (`crates/gemini_browser/src/gemini_page.rs`).
   - Right-click "Ask Gemini" in editor, markdown preview, terminal, project
     panel, agent threads.
   - `gemini_browser.enabled` setting (default off).
   - `cargo check`, `cargo clippy`, `cargo test -p gemini_browser` all clean.
   - Caveat: **never run against the real Gemini site** — selectors are
     best-guess, expected to need live tuning via the built-in
     `gemini_browser: diagnose gemini browser` action.

   Three options:
   - **A. Cherry-pick `fefaa0ac` onto `gemini_cli_proxy`.** ~5 min, keeps
     all the increment-1 work, ~2100 lines ready to test against real DOM.
   - **B. Start blank on `gemini_cli_proxy`, reuse the design but rewrite.**
     Higher effort, no clear gain.
   - **C. Just rename `gemini_browser` → `gemini_cli_proxy`.** Cheapest of
     all, no code change.

   **Recommended: A (cherry-pick)** unless there's a concrete reason the old
   branch is unwanted. The branch name `gemini_browser` is in some sense
   *better* than `gemini_cli_proxy` because this is explicitly NOT the
   gemini-cli path — it's the browser path.

2. **Engine: hand-written CDP, `chromiumoxide`, or obscura?**
   - **Hand-written CDP (status quo on `gemini_browser`)** — chosen last
     time for the tokio/smol mismatch. Uses `Target`, `Runtime`, `Input`,
     `Page` only. Proven to type-check and lint clean. **Recommended**.
   - **`chromiumoxide`** — actively maintained, full protocol coverage, but
     tokio-based. Means hosting a tokio runtime inside Zed's smol process.
     Not worth it unless we need CDP domains beyond the four above.
   - **obscura** — already rejected in prior plan 012. No layout/paint
     pipeline, so it can never be the visible "with render" half; it also
     has a `stealth` mode we explicitly won't use (anti-detection evasion
     is the line we won't cross even for a personal PoC). Drop it.

3. **ACP server or right-click action?**
   The user said "work like external-agents#gemini-cli" — that suggests
   ACP integration (start a "Gemini (browser)" thread in the agent panel),
   not just right-click. But `gemini_browser` only did right-click.
   - **Right-click "Ask Gemini"** — already built in `gemini_browser`. One
     shot, no thread continuity.
   - **ACP server binary** — a standalone process speaking ACP, registered
     under `agent_servers`, that drives the browser tab internally. Gives
     you a real thread in the agent panel with history. Significantly more
     work — prior plan 013 scoped this as `crates/gemini_mcp` (MCP server
     binary), but ACP is the protocol you'd actually need to appear in the
     agent panel like gemini-cli does. Needs a new ACP server crate.
   - **Both** — ACP for thread-style use, right-click for quick lookups.
     Recommended if the goal is full parity with external-agents#gemini-cli.

   Confirm: is ACP-thread integration in scope, or is right-click enough?

## Plan (conditional on Phase 0 failing — pick A from above)

Assuming decision (1) = A (cherry-pick `gemini_browser` onto the new
branch), decision (2) = hand-written CDP, decision (3) = TBD pending user
clarification.

### Branch setup

- [ ] `git checkout de146c3528c8ad00b023609d08cbc2a032620e41`
- [ ] `git checkout -b gemini_cli_proxy`
- [ ] `git cherry-pick 45e867b3ec fefaa0ac1695c7976e978d62e50131354241655e`
      (brings along `.plans/012_gemini_browser_proxy_poc.md` and
      `.plans/013_gemini_browser_tab_implementation.md` from the old branch
      — keep as prior-art references, or move them to `.docs/` to keep
      `.plans/` clean)
- [ ] Resolve: this `.plans/012_gemini_cli_proxy.md` (this file) is on
      `develop`, not on the new branch. Either merge `develop` into the new
      branch or `git checkout develop -- .plans/012_gemini_cli_proxy.md`.

### Verification (do this BEFORE the cherry-pick if possible)

- [ ] **Phase 0 verify OAuth works.** If yes, stop here, close the plan,
      delete the branch. This is the GOAT gate.
- [ ] If OAuth fails, capture the exact failure message (org policy name,
      error text). Useful for the eventual README "why this exists".

### Make increment 1 actually work against real Gemini

The `gemini_browser` increment 1 was never tested against the real
gemini.google.com DOM. Selectors are best-guess.

- [ ] `gemini_browser: open gemini browser` → log in once as
      `katopz@maxion.game` in the Chrome window that opens. Cookie persists
      in `<zed-data-dir>/gemini_browser/profile`.
- [ ] `gemini_browser: diagnose gemini browser` — capture which candidate
      selectors actually match on the signed-in page.
- [ ] Update `COMPOSER_SELECTORS` / `RESPONSE_SELECTORS` in
      `crates/gemini_browser/src/gemini_page.rs` from the diagnose output.
- [ ] Right-click → Ask Gemini on a real selection. Verify end-to-end.
- [ ] Update `.plans/012_gemini_cli_proxy.md` with the working selectors.

### Polish (optional, deferred until increment 1 actually answers)

- [ ] Graceful `Browser.close` on Zed shutdown so the Chrome profile isn't
      left dirty (currently Chrome is process-group-killed on exit, which
      can leave Chrome offering "restore pages?" on next launch).
- [ ] Cap prompt size at a sane limit and tell Gemini when it's truncated
      (file-contents path already does this via `MAX_GEMINI_FILE_BYTES` /
      `truncate_at_char_boundary`; selection path does not).
- [ ] Streaming response into the Zed Markdown buffer instead of waiting
      for text-stability before opening the reply.

### Optional: ACP server (decision 3 = "both" or "ACP only")

Only if right-click isn't enough and real thread-in-agent-panel parity is
required.

- [ ] New crate `crates/gemini_browser_acp` — standalone binary speaking
      ACP over stdio (mirrors how `gemini-cli` itself is launched by Zed).
      Each `Prompt` event from Zed drives `GeminiPage::ask` and streams
      `Message` events back.
- [ ] Register under `agent_servers.gemini-browser` in user settings as a
      custom agent (`type: "custom"`, `command` pointing at the binary).
- [ ] Local thread-mapping store (Gemini conversation URL ↔ Zed
      `session_id`) — there is no real bidirectional sync API; this is a
      locally-stored mapping, per prior plan 012.

## Hard constraints (carried over from prior plan 012)

These are non-negotiable regardless of which scenario applies:

- **No `stealth`/fingerprint spoofing.** Real Chrome + a real profile is
  undetectable because it *is* a real browser; we don't go further into
  anti-bot-evasion territory.
- **Feature gated off by default** (`gemini_browser.enabled = false`).
  Never wired into any release build path. Local opt-in only.
- **Single session, human-paced.** No bulk automation, no multi-account.
- **Never ship as a default Zed provider.** If this proves out and we want
  to ship something for other Zed users, the answer is the real Gemini API
  path (`crates/google_ai` + `crates/language_models/src/provider/google.rs`),
  not the browser-scraping path.
- **Never `mv` or `rm` the prior `gemini_browser` branch to "simulate
  absence"** — per house rules. The branch is read-only to this session.

## Open questions for the user (block Phase 1)

1. Phase 0 result: did OAuth login with `katopz@maxion.game` actually fail,
   and with what error? (If it succeeded, this plan is moot.)
2. Decision (1): cherry-pick the old `gemini_browser` work, start blank, or
   just rename the branch?
3. Decision (3): is ACP-thread parity in scope, or is right-click "Ask
   Gemini" enough?

## Summary

The technical answer to "is this possible?" is **yes** — the prior
`gemini_browser` branch already built a working increment 1 (CDP-driven
Chrome, right-click Ask Gemini, all the right surfaces), it just needs
live DOM tuning. The bigger answer is "try OAuth first — it probably makes
the whole plan unnecessary."
