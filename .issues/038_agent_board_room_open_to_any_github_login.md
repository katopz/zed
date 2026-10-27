# 038 — Agent board: any GitHub account can steer agents in a derivable room
Status: open, latent — no current exposure: the board stack is disabled by default and OBSOLETE (issue 030), and the worker URL `agent-board-worker.foxfox.workers.dev` returns Cloudflare 1042 / 404 like a nonexistent worker (checked 2026-10-27, so not deployed or its workers.dev route is off). Fix before any revival.

## Finding (2026-10-27)

`agent-board-worker` authorizes browser callers with `verifyGithubToken`, and
`ALLOWED_LOGIN = ""` in `wrangler.toml` (set deliberately in `ae1501d9b9`)
accepts **any** GitHub account that completes the device flow. That caller can:

- `POST /v1/rooms/<room>/reply`: the reply is delivered to the target Zed
  device and injected into an agent thread as a steering or user message
  (plan 015 W4/W5).
- Open the room's WebSocket (`RoomCoordinator::authenticate`, github path) and
  read every agent state in the room.

The only remaining barrier is the room name, and it is not secret: the default
room is `blake3(raw_ed25519_pubkey)` (`identity.rs::room_id`), and the public
key is published at `https://github.com/<login>.keys`. Anyone with a GitHub
account can compute the room and send text into agents that run shell
commands: remote prompt injection with code-execution reach.

## Fix (when reviving the board)

- [ ] Fail closed: blank `ALLOWED_LOGIN` rejects every GitHub login
      (`isLoginAllowed` in `src/index.js`), and `wrangler.toml` sets the
      owner's login. Allow-all needs an explicit opt-in value, not a blank.
- [ ] Bind rooms to a secret, not a public key: derive the default room from
      `blake3(private key material || "agent-board-room")` or a random room
      token stored beside the key, so knowing the public key is not enough.
- [ ] Tag web-originated replies in the injected message (plan 015's 🌐
      badge) so an agent can treat them as untrusted input.
- [ ] Worker test: a token whose login is not allowlisted gets 401 on
      `/reply` and on WebSocket auth.

## Deferred items made moot

The browser and deploy checks still marked `- [-]` in plans 013, 015 and 024
(`wrangler dev` smoke, cross-device visibility, sign-in, WebSocket, reply
delivery) all exercise this retired stack. They stay deferred with the stack;
none should be run against a deployed worker until the fixes above land.
