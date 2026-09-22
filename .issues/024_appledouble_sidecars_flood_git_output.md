# AppleDouble sidecars in .git flood every git command with errors

status: fixed 2026-09-22 — 1075 sidecars + 1 stale ref lock removed, `git fsck` clean.
Will recur (exFAT volume); recovery is the one-liner below.

## Symptom

Every single git invocation in this repo printed ~150 lines of:

```
error: non-monotonic index .git/objects/pack/._pack-7aa41b0fba74bbfdfc0cb157eba850ff2bf75649.idx
```

repeated once per object-database access. `git status`, `git commit`, `git push`
— all of them. The real output was buried at the bottom, so every agent session
burned tokens scrolling past it, and a genuine git error would have been easy to
miss in the noise.

## Cause

`/Volumes/SDXC1TB` is **exFAT**, which has no native extended-attribute support.
macOS emulates xattrs and resource forks by writing a sidecar file named
`._<original>` next to each file. 1075 of these had accumulated inside `.git/`.

Git enumerates `.git/objects/pack/*.idx` by glob, picks up `._pack-*.idx`, tries
to parse a 4096-byte AppleDouble blob as a pack index, and fails — once per
access. The sidecars contain **no git data**; every one was confirmed
`AppleDouble encoded Macintosh file`, and each had an intact real counterpart.

Also found and removed: `refs/remotes/upstream/background-agent/mvp-zed-4vt-20260220.lock`,
a zero-byte lock from 2026-08-19 left by an interrupted fetch. It was the only
entry in its directory — the ref it was locking never got created — and it was
the sole remaining `git fsck` complaint.

## Fix / recovery

```bash
find .git -name "._*" -type f -delete
find .git/refs -name "*.lock" -type f -mtime +1 -delete   # stale locks only
git fsck --no-dangling                                    # must print nothing
```

Verified after cleanup: `git fsck --no-dangling` silent, `git log`/`git status`
clean, `HEAD == origin/develop`.

## Notes for whoever hits this next

- **`tar` cannot back these up.** bsdtar recognises `._*` as AppleDouble metadata
  and silently merges/skips them — a backup tarball comes out empty (29 bytes) or
  partial. That behaviour is itself confirmation they are macOS cruft, not data.
  The real safety net is the remote: confirm `HEAD == origin/<branch>` and a clean
  tree before deleting, which makes the whole object store reconstructible.
- **It will come back** as long as the repo lives on exFAT. There is no setting
  that stops macOS writing sidecars to a local exFAT volume
  (`DSDontWriteNetworkStores` only covers network mounts). Re-run the one-liner
  when the noise reappears.
- Don't "fix" this by moving the repo off the volume without asking — disk layout
  is an owner call (see `.issues/013` on the same volume's launch latency).
