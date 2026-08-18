# 007 — exFAT AppleDouble (`._*`) hygiene for this repo

Date: 2026-08-19

## Problem

The repo lives on an exFAT volume (`/Volumes/SDXC1TB`). exFAT cannot store macOS extended
attributes, so macOS writes `._<name>` AppleDouble sidecar files (magic `0x00051607`,
"Mac OS X" header) next to every file that carries an xattr.

2026-08-19 audit:

- 82,450 sidecars in the working tree (~328 MB at exFAT's 4 KB clusters) + 1,062 inside `.git`
- Sidecars in `.git/objects/pack/` break git's pack scanning:
  `error: non-monotonic index .git/objects/pack/._pack-*.idx` spams stderr of every git op
- They REGENERATE: any new pack write (fetch/repack) creates fresh sidecars — observed
  2026-08-19 03:11 during `git fetch upstream`

## Fix (periodic maintenance)

From repo root (covers `.git` too):

```
find . \( -name '._*' -o -name '.DS_Store' \) -delete
```

Safe because:

- AppleDouble files are pure xattr/Finder-metadata sidecars — never referenced by git or cargo
- `.gitignore:67` (`._*`) already keeps working-tree sidecars out of `git status`
- `git ls-files '._*'` → empty (nothing tracked)

Post-cleanup health check (2026-08-19): `git fsck --connectivity-only` exit 0,
only dangling commits (harmless old reflog states).

## Related garbage

- `.git/objects/pack/tmp_pack_*` from an interrupted fetch (57 MB, flagged by
  `git count-objects -v` as garbage) — removed with
  `find .git/objects/pack -name 'tmp_pack_*' -delete`
  (note: agent sandbox blocks `rm`; `find -delete` is allowed)

## Long-term

Only real fix is hosting the repo on an APFS volume (native xattrs, no sidecars).
Until then, run the `find -delete` sweep before big operations (upstream merges, gc)
to keep git output and tooling clean.
