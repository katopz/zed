# E2E Test: HelloWorld Auto-Prompt with Git Flow

## Status

- [x] Step 1: Initialize git repo with Cargo project structure on `main`
- [ ] Step 2: Quick auto-prompt verification — simple text change and stop
- [ ] Step 3: Implement hello world with passing tests on `develop`
- [ ] Step 4: Add greet-by-name feature on `feature/01_greet_by_name` branch
- [ ] Step 5: Merge feature to `develop`, verify tests pass
- [ ] Step 6: Release v0.1.0 — bump version, tag, merge to `main`
- [ ] Step 7: Verify git history matches expected flow
- [ ] Step 8: Inject bug, ask AI to diagnose and fix
- [ ] Step 9: Verify fix commit uses `fix:` conventional commit prefix
- [ ] Step 10: Release v0.2.0 — bump version, tag, merge to `main`
- [ ] Step 11: Final verification of complete git history

---

## Project Structure

```
.
├── .plan/
│   └── 01_helloworld_flow.md    (this file)
├── Cargo.toml
├── src/
│   └── main.rs
└── tests/
    └── integration_test.rs
```

## Step 1: Initialize Git Repo on `main`

Create the project:

```
cargo init --name helloworld
```

Edit `Cargo.toml` to set version `0.1.0`:

```toml
[package]
name = "helloworld"
version = "0.1.0"
edition = "2021"
```

Create minimal `src/main.rs`:

```rust
fn main() {
    println!("Hello, world!");
}
```

Create empty `tests/` directory.

Git operations:

```
git init
git add .
git commit -m "chore: initialize helloworld project"
git branch -M main
```

**Verify**: `git log --oneline` shows one commit. `cargo check` passes.

---

## Step 2: Quick Auto-Prompt Verification

Ask the AI to make a simple, quick change to verify auto-prompt is working:

**Important:** The AI **must use tools** (not direct editing) to trigger auto-prompt verification.

1. Use the `edit_file` tool to change `"Hello, world!"` in `src/main.rs` to `"hi!"`
2. Use the `terminal` tool to commit: `git add src/main.rs && git commit -m "feat: simplify greeting to hi"`
3. **Stop immediately after committing** — do not continue with additional work

Expected behavior:
- AI should use `edit_file` and `terminal` tools (not direct editing)
- AI should complete the task in under 30 seconds
- AI should explicitly stop and wait for next command
- Logs should show `[auto_prompt::decide]` with full decision process
- Logs should show auto-prompt evaluating and stopping with `NoAction`
- One commit should be added to `main`

**Verify**: 
- `git log --oneline` shows 2 commits on `main`
- Latest commit starts with `feat:`
- `src/main.rs` contains `"hi!"`
- AI stopped and waited for next instruction (auto-prompt verified)
- Logs contain `[auto_prompt::decide]` messages showing the evaluation process

---

## Step 3: Implement Hello World with Tests on `develop`

Create `develop` branch from `main`:

```
git checkout -b develop
```

Create `src/lib.rs` with a `greet()` function:

```rust
pub fn greet() -> String {
    "hi!".to_string()
}
```

Update `src/main.rs`:

```rust
use helloworld::greet;

fn main() {
    println!("{}", greet());
    // Restore original greeting for feature branch work
}
```

Create `tests/integration_test.rs`:

```rust
use helloworld::greet;

#[test]
fn test_greet() {
    assert_eq!(greet(), "hi!");
}

#[test]
fn test_greet_is_not_empty() {
    assert!(!greet().is_empty());
}
```

Run tests: `cargo test`

Commit:

```
git add .
git commit -m "feat: implement hello world with greet function and tests"
```

**Verify**: `cargo test` passes with 2 tests. `git log --oneline` shows 2 commits on `develop`.

---

## Step 4: Add Greet-by-Name Feature on Feature Branch

Create feature branch from `develop`:

```
git checkout -b feature/01_greet_by_name develop
```

Update `src/lib.rs`:

```rust
pub fn greet() -> String {
    "hi!".to_string()
}

pub fn greet_by_name(name: &str) -> String {
    format!("hi, {name}!")
}
```

Update `src/main.rs`:

```rust
use helloworld::{greet, greet_by_name};

fn main() {
    println!("{}", greet());
    println!("{}", greet_by_name("Developer"));
}
```

Add to `tests/integration_test.rs`:

```rust
use helloworld::{greet, greet_by_name};

#[test]
fn test_greet() {
    assert_eq!(greet(), "hi!");
}

#[test]
fn test_greet_is_not_empty() {
    assert!(!greet().is_empty());
}

#[test]
fn test_greet_by_name() {
    assert_eq!(greet_by_name("Alice"), "hi, Alice!");
}

#[test]
fn test_greet_by_name_empty() {
    assert_eq!(greet_by_name(""), "hi, !");
}
```

Run tests: `cargo test`

Commit:

```
git add .
git commit -m "feat: add greet_by_name function with tests"
```

**Verify**: `cargo test` passes with 4 tests. Branch is `feature/01_greet_by_name`.

---

## Step 5: Merge Feature to `develop`

```
git checkout develop
git merge --no-ff feature/01_greet_by_name -m "merge: feature/01_greet_by_name into develop"
git branch -d feature/01_greet_by_name
```

Run tests: `cargo test`

**Verify**: `cargo test` passes. `git log --oneline --graph` shows merge commit on `develop`. Feature branch deleted.

---

## Step 6: Release v0.1.0

Create release branch from `develop`:

```
git checkout -b release/v0.1.0 develop
```

Update `Cargo.toml` version to `0.1.0` (should already be, confirm).

Run final tests: `cargo test`

Merge to `main`:

```
git checkout main
git merge --no-ff release/v0.1.0 -m "release: v0.1.0"
git tag -a v0.1.0 -m "Release v0.1.0: Hello world with greet-by-name"
```

Merge back to `develop`:

```
git checkout develop
git merge --no-ff release/v0.1.0 -m "merge: release/v0.1.0 into develop"
git branch -d release/v0.1.0
```

**Verify**: `git tag -l` shows `v0.1.0`. `git log main --oneline` shows release merge. `develop` is up to date.

---

## Step 7: Verify Git History

Run these verification commands and confirm:

```bash
# Should show: main, develop
git branch -a

# Should show: v0.1.0
git tag -l

# Should show orderly commit history with merge commits
git log --oneline --graph --all

# Tests should pass on both main and develop
git checkout main && cargo test --quiet
git checkout develop && cargo test --quiet
```

**Expected git log structure** (from `main`):

```
*   merge: release/v0.1.0 (main HEAD)
|\
| *   merge: feature/01_greet_by_name into develop
| |\
| | * feat: add greet_by_name function with tests
| * feat: implement hello world with greet function and tests
|/
* chore: initialize helloworld project
```

---

## Step 8: Inject Bug and Ask AI to Fix

On `develop` branch, inject this bug in `src/lib.rs`:

Change the `greet()` function to return a wrong value:

```rust
pub fn greet() -> String {
    "Bye!".to_string()  // BUG: wrong greeting
}
```

Commit the bug:

```
git add src/lib.rs
git commit -m "intentional bug: wrong greeting for testing fix flow"
```

Now ask the AI to fix the failing test.

The AI should:

1. Run `cargo test` and see the failure
2. Diagnose the bug in `src/lib.rs`
3. Fix `greet()` to return `"hi!"`
4. Run `cargo test` to confirm fix
5. Commit with conventional commit: `fix: correct greeting to return hi!`

**Verify**: `cargo test` passes. `git log --oneline -1` shows `fix:` commit prefix.

---

## Step 9: Verify Fix Commit

```bash
# Latest commit should start with "fix:"
git log --oneline -1 | grep "^.\{8\}fix:"

# Tests should pass
cargo test --quiet

# The fix should be correct
grep -q '"hi!"' src/lib.rs
```

**Verify**: All checks pass. Commit message follows conventional format.

---

## Step 10: Release v0.2.0

Bump version in `Cargo.toml` to `0.2.0`.

Create release branch:

```
git checkout -b release/v0.2.0 develop
```

Update `Cargo.toml`:

```toml
[package]
name = "helloworld"
version = "0.2.0"
edition = "2021"
```

Commit version bump:

```
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

## Step 11: Final Verification

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

**Conventional commits in history**:
- `chore: initialize helloworld project`
- `feat: simplify greeting to hi`
- `feat: implement hello world with greet function and tests`
- `feat: add greet_by_name function with tests`
- `fix: correct greeting to return hi!`
- `chore: bump version to 0.2.0`
- Release merges with `release:` prefix

---

## Teardown

```bash
cd ..
rm -rf helloworld
```
