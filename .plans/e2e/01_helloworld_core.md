# E2E Plan 01: HelloWorld Core Git Flow

## Status

- [ ] Step 1: Initialize git repo with Cargo project structure on `main`
- [ ] Step 2: Quick auto-prompt verification — simple text change and stop
- [ ] Step 3: Implement hello world with passing tests on `develop`
- [ ] Step 4: Add greet-by-name feature on `feature/01_greet_by_name` branch
- [ ] Step 5: Merge feature to `develop`, verify tests pass
- [ ] Step 6: Release v0.1.0 — bump version, tag, merge to `main`

---

## Project Structure

```
.
├── .plan/
│   ├── 01_helloworld_core.md         (this file)
│   └── 02_helloworld_bugfix_release.md
├── Cargo.toml
├── src/
│   ├── main.rs
│   └── lib.rs
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

**Verify**: 
- `git log --oneline` shows 2 commits on `main`
- Latest commit starts with `feat:`
- `src/main.rs` contains `"hi!"`

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

**Verify**: `cargo test` passes with 2 tests. `git log --oneline` shows 3 commits on `develop`.

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

## Expected State After Plan 01

| Branch | HEAD commit | Tests | Version |
|--------|------------|-------|---------|
| `main` | `release: v0.1.0` | 4 passing | `0.1.0` |
| `develop` | `merge: release/v0.1.0` | 4 passing | `0.1.0` |

**Tags**: `v0.1.0`

**Conventional commits in history**:
- `chore: initialize helloworld project`
- `feat: simplify greeting to hi`
- `feat: implement hello world with greet function and tests`
- `feat: add greet_by_name function with tests`
- Release merges with `release:` prefix