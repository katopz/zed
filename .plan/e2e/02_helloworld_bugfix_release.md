# E2E Plan 02: HelloWorld Bugfix & Second Release

## Status

- [ ] Step 1: Inject bug into `greet()` on `develop` and commit
- [ ] Step 2: Ask AI to diagnose and fix the failing tests
- [ ] Step 3: Verify fix commit uses `fix:` conventional commit prefix
- [ ] Step 4: Release v0.2.0 — bump version, tag, merge to `main`
- [ ] Step 5: Final verification of complete git history

---

## Prerequisites

This plan runs AFTER Plan 01 (`01_helloworld_core.md`) completes.
The project should have:

| Branch | HEAD commit | Tests | Version |
|--------|------------|-------|---------|
| `main` | `release: v0.1.0` | 4 passing | `0.1.0` |
| `develop` | `merge: release/v0.1.0` | 4 passing | `0.1.0` |

**Tags**: `v0.1.0`

---

## Step 1: Inject Bug into `greet()` on `develop`

Switch to `develop` branch:

```
git checkout develop
```

Edit `src/lib.rs` — change the `greet()` function to return a wrong value:

```rust
pub fn greet() -> String {
    "Goodbye, world!".to_string()
}

pub fn greet_by_name(name: &str) -> String {
    format!("hi, {name}!")
}
```

Commit the bug:

```
git add src/lib.rs
git commit -m "intentional bug: wrong greeting for testing fix flow"
```

Run tests to confirm failure: `cargo test`

**Verify**: `cargo test` fails (greet returns "Goodbye, world!" instead of "hi!"). Bug commit exists on `develop`.

---

## Step 2: Ask AI to Diagnose and Fix the Failing Tests

Prompt the AI:

```
cargo test is failing. Diagnose the bug and fix it.
```

The AI should:

1. Run `cargo test` and see the failure
2. Read `src/lib.rs` and find the bug in `greet()`
3. Fix `greet()` to return `"hi!"` instead of `"Goodbye, world!"`
4. Run `cargo test` to confirm the fix
5. Commit with conventional commit: `fix: correct greeting to return hi!`

**Verify**: `cargo test` passes with 4 tests. `src/lib.rs` has `greet()` returning `"hi!"`.

---

## Step 3: Verify Fix Commit Uses `fix:` Prefix

Run verification:

```bash
git log --oneline -1
```

The latest commit should start with `fix:`.

Also verify:

```bash
cargo test --quiet
grep -q '"hi!"' src/lib.rs
```

**Verify**: Latest commit starts with `fix:`. All tests pass. `greet()` returns `"hi!"`.

---

## Step 4: Release v0.2.0

Bump version in `Cargo.toml` to `0.2.0`:

```toml
[package]
name = "helloworld"
version = "0.2.0"
edition = "2021"
```

Create release branch from `develop`:

```
git checkout -b release/v0.2.0 develop
git add Cargo.toml
git commit -m "chore: bump version to 0.2.0"
```

Run tests: `cargo test`

Merge to `main`:

```
git checkout main
git merge --no-ff release/v0.2.0 -m "release: v0.2.0"
git tag -a v0.2.0 -m "Release v0.2.0: Bug fix release"
```

Merge back to `develop`:

```
git checkout develop
git merge --no-ff release/v0.2.0 -m "merge: release/v0.2.0 into develop"
git branch -d release/v0.2.0
```

**Verify**: `git tag -l` shows both `v0.1.0` and `v0.2.0`. Version in `Cargo.toml` is `0.2.0` on both branches.

---

## Step 5: Final Verification of Complete Git History

Run these verification commands:

```bash
# All tags
git tag -l
# Expected: v0.1.0, v0.2.0

# All branches (should only be main and develop)
git branch
# Expected: main, develop

# Full history graph
git log --oneline --graph --all

# Tests pass on main
git checkout main && cargo test --quiet

# Tests pass on develop
git checkout develop && cargo test --quiet

# Verify version
grep 'version = "0.2.0"' Cargo.toml
```

**Expected final state**:

| Branch | HEAD commit | Tests | Version |
|--------|------------|-------|---------|
| `main` | `release: v0.2.0` | 4 passing | `0.2.0` |
| `develop` | `merge: release/v0.2.0` | 4 passing | `0.2.0` |

**Tags**: `v0.1.0`, `v0.2.0`

**Conventional commits across both plans**:
- `chore: initialize helloworld project`
- `feat: simplify greeting to hi`
- `feat: implement hello world with greet function and tests`
- `feat: add greet_by_name function with tests`
- `fix: correct greeting to return hi!`
- `chore: bump version to 0.2.0`
- Release merges with `release:` prefix

**Verify**: All checks pass. Complete git flow history is intact. Both tags exist. No stale branches remain.