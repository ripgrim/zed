# Plan: Review Fixes for PR #49137

## Change 1: Remove `.expect()` in `FakeGitRepository::rename_worktree` (fixes #1 and #5)

**File:** `crates/fs/src/fake_git_repo.rs`

**Lines ~521-526** — In `rename_worktree`, the third phase (state update) uses `.expect("worktree was validated above")` which will panic if the worktree disappeared between the validation lock and the mutation lock. Replace with a fallible `.context()?` so it returns an error instead of panicking.

### Before

```rust
fs.with_git_state(&dot_git_path, true, move |state| {
    let worktree = state
        .worktrees
        .iter_mut()
        .find(|worktree| worktree.path == old_path)
        .expect("worktree was validated above");
    worktree.path = new_path;
    Ok::<(), anyhow::Error>(())
})??;
```

### After

```rust
fs.with_git_state(&dot_git_path, true, move |state| {
    let worktree = state
        .worktrees
        .iter_mut()
        .find(|worktree| worktree.path == old_path)
        .context("worktree disappeared between validation and state update")?;
    worktree.path = new_path;
    Ok::<(), anyhow::Error>(())
})??;
```

### Verification

- Confirm `use anyhow::Context as _` (or equivalent) is already in scope in this file. Looking at line 2: `use anyhow::{Context as _, Result, bail};` — yes, `Context` is already imported.
- No test changes needed; the success path behaves identically.

---

## Change 2: Remove `trim()` from `parse_worktrees_from_str` (fixes #4)

**File:** `crates/git/src/repository.rs`

**Lines ~214-218** — Remove the `line.trim()` call and the `if line.is_empty() { continue; }` guard. Add a comment explaining why we don't trim: git's porcelain output is well-defined, and trimming would silently corrupt paths that legitimately contain leading/trailing whitespace (which is valid on Linux/macOS).

### Before

```rust
for line in entry.lines() {
    let line = line.trim();
    if line.is_empty() {
        continue;
    }
    if let Some(rest) = line.strip_prefix("worktree ") {
```

### After

```rust
for line in entry.lines() {
    // Don't trim whitespace — git's porcelain output has a well-defined
    // format with no extraneous whitespace, and filesystem paths can
    // legitimately contain leading or trailing spaces.
    if line.is_empty() {
        continue;
    }
    if let Some(rest) = line.strip_prefix("worktree ") {
```

Note: keep the `if line.is_empty()` guard — that handles blank lines within an entry block (which can happen). We just remove the `trim()`.

### Test update

**Same file, lines ~3692-3699** — The test case for "Leading/trailing whitespace on lines should be tolerated" now tests the *opposite* expectation: whitespace-padded lines should NOT match, because we no longer trim. Change this test to verify that whitespace-padded input produces zero results (since `"  worktree /home/user/project  "` won't match `strip_prefix("worktree ")`).

#### Before

```rust
// Leading/trailing whitespace on lines should be tolerated
let input =
    "  worktree /home/user/project  \n  HEAD abc123  \n  branch refs/heads/main  \n\n";
let result = parse_worktrees_from_str(input);
assert_eq!(result.len(), 1);
assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
assert_eq!(result[0].sha.as_ref(), "abc123");
assert_eq!(result[0].ref_name.as_ref(), "refs/heads/main");
```

#### After

```rust
// Leading/trailing whitespace on lines is NOT trimmed — git's porcelain
// format is well-defined and path components could contain spaces.
// Whitespace-padded lines won't match the expected prefixes.
let input =
    "  worktree /home/user/project  \n  HEAD abc123  \n  branch refs/heads/main  \n\n";
let result = parse_worktrees_from_str(input);
assert_eq!(result.len(), 0, "whitespace-padded lines should not parse");
```

---

## Execution Order

1. **Change 1** (`crates/fs/src/fake_git_repo.rs`) — Replace `.expect()` with `.context()?`
2. **Change 2** (`crates/git/src/repository.rs`) — Remove `trim()`, update comment, update test

These two changes are independent and touch different files, so order doesn't matter. But doing them in this order keeps the simpler change first.

## Verification

After both changes, run:

```
cargo test -p fs fake_git_repo::tests::test_fake_worktree_lifecycle
cargo test -p git repository::tests::test_parse_worktrees_from_str
```

Then run the full collab integration tests for the PR:

```
cargo test -p collab git_tests
```
