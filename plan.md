# Multi-Agent Git Worktree Support — Plan (Option C: Hybrid)

> **Goal**: Allow users to start agent threads in isolated git worktrees so that
> multiple agents can work on the same codebase in parallel without interfering
> with each other.

> **Approach**: Hybrid — automatic by default, fully configurable. The user picks
> "New Worktree" from a dropdown before starting a thread. Zed creates a git
> worktree, opens it as a new workspace inside the `MultiWorkspace`, and starts
> the agent thread there.

## Scope and Rollout

- Primary UI target: `AgentPanel` in `crates/agent_ui`.
- `agent_ui_v2` / `AgentsPanel` is out of scope and will be removed.
- Local + remote support are both in scope.
- Remote plumbing is the final implementation milestone, after local behavior is
  complete and stable.

---

## Terminology

Throughout this document:

| Term               | Meaning                                                                                                                                         |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| **Zed Worktree**   | A folder open in a Zed project (`project::Worktree` / `worktree::Worktree`). This is Zed's concept of a directory tree, **not** a git worktree. |
| **Git Worktree**   | A `git worktree add` checkout — an additional working directory sharing the same `.git` object store (`git::repository::Worktree`).             |
| **Workspace**      | A `workspace::Workspace` entity — one project context inside a Zed window.                                                                      |
| **MultiWorkspace** | The root view of a Zed window (`workspace::MultiWorkspace`). Contains one or more `Workspace` entities and can switch between them.             |

The git worktree APIs live in `crates/git/src/repository.rs` on the `GitRepository` trait:

- `fn worktrees() -> Vec<git::repository::Worktree>` — lists git worktrees
- `fn create_worktree(name, directory, from_commit) -> Result<()>` — creates a git worktree
- `fn rename_branch(branch, new_name) -> Result<()>` — already exists

These must not be confused with `project::Worktree` / `worktree::Worktree` which
represent open folders in the editor.

---

## Feature Flag

All work is gated behind the existing `agent-v2` feature flag
(`AgentV2FeatureFlag` in `crates/feature_flags/src/flags.rs`).

Use `cx.has_flag::<AgentV2FeatureFlag>()` to gate UI and behavior. No new flag
is needed.

---

## Phase 1: UI — Thread Target Selector

### 1.1 "Start Thread In…" Dropdown

**Where**: The agent panel toolbar, adjacent to the existing "New Thread" (`+`)
menu.

**What the user sees**:

```
┌──────────────────────┐
│ Start Thread In...   │
│ Local Project        │  ← default, current workspace
│ New Worktree         │  ← creates a git worktree
└──────────────────────┘
```

With a tooltip on "New Worktree":

> You currently have "main" checked out and by default, Zed creates a new
> branch for worktrees based off the selected branch.

**Reuse existing UI components**:

- `PopoverMenu` + `ContextMenu` — same pattern as the mode selector
  (`crates/agent_ui/src/acp/mode_selector.rs`) and the existing new-thread menu
  in `AgentPanel::render_toolbar()`.
- `ContextMenuEntry` with `.icon()`, `.icon_color()`, `.handler()`,
  `.documentation_aside()` for the tooltip — all already used in the new-thread
  menu.
- `Tooltip::for_action_in()` for keyboard shortcut hints.
- `Button` with `.label_size(LabelSize::Small)` + `.icon(IconName::ChevronDown)`
  as the trigger — same pattern as `ModeSelector`'s trigger button.

**Implementation location**: `crates/agent_ui/src/agent_panel.rs` — modify
`render_toolbar()` and the new-thread menu construction.

**Behavior**:

- "Local Project" is always available and is the default.
- "New Worktree" is only shown when `cx.has_flag::<AgentV2FeatureFlag>()`.
- "New Worktree" is disabled (grayed out with tooltip) when:
  - The project has no git repository.
  - The project is remote/collab during the local-only implementation milestones.
- Selecting "New Worktree" does **not** immediately create the worktree. It sets
  the panel's next thread target to `ThreadTarget::NewWorktree`.
- Worktree creation happens when the user explicitly starts a new ACP thread from
  the toolbar/menu. It does **not** wait for first message send.

**New types**:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ThreadTarget {
    #[default]
    LocalProject,
    NewWorktree,
    ExistingWorktree {
        /// Path to the git worktree on disk.
        path: PathBuf,
        /// The branch name checked out in the worktree.
        branch: String,
    },
}
```

Store `ThreadTarget` on the `AgentPanel` or on the individual thread view state.

### 1.2 Worktree Indicator on Running Threads

**Where**: ACP thread history rendering in
`crates/agent_ui/src/acp/thread_history.rs` (`render_history_entry()`).

Current ACP history uses `ListItem`, not `ThreadItem`. We should add a secondary
line in the existing `ListItem` entry that shows branch/worktree data from
session metadata.

**What the user sees**:

```
┌──────────────────────────────────────┐
│ ⑀  link-agent-panel                  │
└──────────────────────────────────────┘
```

The branch icon (⑀) + branch/worktree name is shown beneath the thread title.

**Wiring up**:

- Extend the thread history row model to include optional worktree display text.
- If session metadata contains worktree info, render a second muted line with
  branch icon + branch/worktree label.
- Use `ThreadItem` style as visual reference, but keep implementation in
  `AcpThreadHistory` for now to avoid broad UI refactors.

### 1.3 Loading / Indeterminate States

When "New Worktree" is selected and the user starts a new ACP thread, several
async operations happen before session creation:

1. Creating the git worktree (`git worktree add ...`)
2. Opening the worktree path as a new project
3. Adding the workspace to `MultiWorkspace`
4. Starting the agent thread in the new workspace

During this time, the UI needs to show indeterminate/loading states.

**Reuse existing components**:

- `SpinnerLabel` (`crates/ui/src/components/label/spinner_label.rs`) for
  creation progress.
- Existing error notification/prompt patterns from workspace/git UI for failures
  that occur before ACP thread creation.
- `AcpThreadView::thread_error` can still be used for failures after thread view
  exists.

**States to support**:

- The toolbar selector + new-thread trigger should be disabled while creation is
  in progress for that action.
- Show `SpinnerLabel` status text during worktree creation (e.g.,
  "Creating worktree…").
- If creation fails before thread creation, show an actionable error and keep
  the user in the current workspace without creating a partial thread.

> **Note**: The exact visual design of these loading states needs further
> discussion with the team. The infrastructure to support them should be built,
> but the specific UI treatment is TBD.

---

## Phase 2: Git Repository API Changes

### 2.1 Add `remove_worktree()` to `GitRepository`

The `GitRepository` trait (`crates/git/src/repository.rs`) currently has
`worktrees()` and `create_worktree()` but no way to remove a worktree.

**Add to the trait**:

```rust
fn remove_worktree(
    &self,
    path: PathBuf,
    force: bool,
) -> BoxFuture<'_, Result<()>>;
```

**Implementation on `RealGitRepository`**: run
`git worktree remove <path> [--force]`.

**Implementation on `FakeGitRepository`** (`crates/fs/src/fake_git_repo.rs`):
track created worktrees in `FakeGitRepositoryState` and allow removal. The
current implementations of `worktrees()` and `create_worktree()` on
`FakeGitRepository` are `unimplemented!()` — these need to be filled in as part
of this work to support testing.

### 2.2 Add `rename_worktree()` to `GitRepository`

Git worktrees can be moved/renamed via `git worktree move <worktree> <new-path>`.
This is useful if we want to rename the worktree directory to match the thread
title after it's generated.

**Add to the trait**:

```rust
fn rename_worktree(
    &self,
    old_path: PathBuf,
    new_path: PathBuf,
) -> BoxFuture<'_, Result<()>>;
```

**Implementation on `RealGitRepository`**: run
`git worktree move <old_path> <new_path>`.

**Implementation on `FakeGitRepository`**: update the tracked worktree entry.

### 2.3 Repository Handle + Proto Plumbing

`GitRepository` trait changes are not enough on their own. `AgentPanel` uses
project-level repository handles (`project::Repository` in
`crates/project/src/git_store.rs`), which handle both local and remote paths.

Add matching methods on `project::Repository`:

- `remove_worktree(path, force)`
- `rename_worktree(old_path, new_path)`

Add corresponding RPC plumbing:

- New messages in `crates/proto/proto/git.proto`
- Message registration in `crates/proto/src/proto.rs`
- Request handlers in `GitStore` for remote mode
- Local + remote branches in repository job execution logic

### 2.4 Branch Naming

When creating a git worktree for an agent thread, Zed needs to generate a branch
name. The default pattern:

```
zed/agent/<short-id>
```

Where `<short-id>` is a random 5-character alphanumeric string (e.g., `a4Xiu`).

The branch can be renamed via the existing `GitRepository::rename_branch()`
once the agent generates a thread title (similar to how Conductor handles it —
start with a placeholder, rename once context is available).

### 2.5 Worktree Storage Location

Git worktrees are created via `git worktree add <path> -b <branch> <base>`. The
`<path>` determines where the working directory lives on disk.

**Default location**: A managed directory under the Zed data directory.

On macOS:

```
~/Library/Application Support/Zed/agent-worktrees/<repo-name>/<branch-name>/
```

On Linux:

```
~/.local/share/zed/agent-worktrees/<repo-name>/<branch-name>/
```

This keeps worktrees out of the project directory and avoids `.gitignore` issues.

**Project-level override**: Users can configure a custom worktree path via the
`git` section of project settings. This requires changes in both:

- `crates/settings_content/src/project.rs` (`settings_content::GitSettings`)
- `crates/project/src/project_settings.rs` (`project::GitSettings`)

Add a new field to both `GitSettings` structs:

```rust
pub struct GitSettings {
    // ... existing fields ...

    /// Directory where agent worktrees are created.
    /// If not set, defaults to the Zed data directory.
    ///
    /// Can be an absolute path or relative to the project root.
    ///
    /// Default: null (uses system default)
    pub agent_worktree_directory: Option<String>,
}
```

Because this field is not `Copy`, existing `Copy` derives on Git settings types
must be removed where necessary.

This allows teams to colocate worktrees with the project (e.g.,
`.worktrees/`) or use a shared location. Users set this in
`.zed/settings.json`:

```json
{
  "git": {
    "agent_worktree_directory": ".worktrees"
  }
}
```

### 2.6 Creation Flow

Use the existing `GitRepository::create_worktree()` method:

```rust
fn create_worktree(
    &self,
    name: String,        // branch name
    directory: PathBuf,  // parent directory for the worktree
    from_commit: Option<String>, // base commit/branch (e.g., "main")
) -> BoxFuture<'_, Result<()>>;
```

This runs `git worktree add <directory>/<name> -b <name> <from_commit>`.

The base commit should be the currently checked-out branch HEAD in the main
worktree so the agent starts from the same state the user is looking at.

### 2.7 Trust Management

The existing worktree picker (`crates/git_ui/src/worktree_picker.rs`) already
handles trust for newly created worktrees via `TrustedWorktrees`. The same
pattern should be followed: if the parent project is trusted, automatically trust
the new worktree path. See `WorktreeListDelegate::create_worktree()` for the
reference implementation.

---

## Phase 3: Workspace Integration

### 3.1 Sequence of Operations

When the user starts a new ACP thread while `ThreadTarget::NewWorktree` is
selected:

1. **Create the git worktree** (async)
   - Generate branch name (`zed/agent/<id>`)
   - Determine storage path (default or from `GitSettings.agent_worktree_directory`)
   - Call `GitRepository::create_worktree()`
   - Trust the new path (following `worktree_picker.rs` pattern)

2. **Open the worktree as a new Zed project** (async)
   - Use the workspace open path flow with `open_new_workspace: Some(true)` so we
     deterministically create a new workspace instead of reusing an existing one.
   - Reference behavior: `workspace::open_paths` in
     `crates/workspace/src/workspace.rs`

3. **Attach workspace to the current `MultiWorkspace`**
   - If open-path flow returns a workspace not yet attached to this window's
     `MultiWorkspace`, call `add_workspace()` explicitly.

4. **Activate the new workspace**
   - Ensure the new workspace becomes active (`activate()` / `activate_index()` as
     appropriate).

5. **Start the thread**
   - Call `AgentPanel::new_agent_thread()` in the new workspace's agent panel.
   - The thread runs against the new workspace's project, which points at the
     git worktree on disk
   - Persist `AgentGitWorktreeInfo` as soon as the session id is known

6. **Failure rollback**
   - If worktree creation succeeds but workspace open or thread startup fails,
     clean up the created git worktree and surface an error to the user.

### 3.2 Implementation Location

The orchestration logic for steps 1–6 should live in a new module:
`crates/agent_ui/src/agent_worktree.rs`. This keeps it close to the UI that
triggers it while being separate enough to test independently.

The async work should be spawned via `cx.spawn()` from the `AgentPanel`, with
`WeakEntity<Workspace>` and the `MultiWorkspace` root captured for the later
workspace management steps.

### 3.3 Workspace ↔ Thread Association

Each thread needs to know which git worktree (if any) it belongs to.
Use persisted thread data as source of truth, with ACP meta as a transport copy.

```rust
pub struct AgentGitWorktreeInfo {
    /// The branch name in the git worktree.
    pub branch: String,
    /// Absolute path to the git worktree on disk.
    pub worktree_path: PathBuf,
    /// The base branch/commit the worktree was created from.
    pub base_ref: String,
}
```

Storage model:

- Persist `AgentGitWorktreeInfo` in the agent thread DB model (`DbThread`) so it
  survives restarts.
- Expose enough metadata in thread list results so history rendering can display
  branch/worktree labels without loading every full thread.
- Mirror this metadata into `AgentSessionInfo.meta` when returning session lists.
- Keep `AcpServerView` state as runtime cache only, not source of truth.

This enables:

- History rows to display branch/worktree labels
- Cleanup to find and remove git worktrees reliably
- Resume flow to reopen the correct workspace path

---

## Phase 4: Cleanup & Lifecycle

### 4.1 When to Clean Up

Git worktrees should be cleaned up (removed) when:

- The user explicitly discards a thread that was running in a worktree
- The user applies/merges changes back (future work)
- The worktree's workspace is closed and the thread is complete

Worktrees should **not** be cleaned up when:

- The thread is still running
- The user might want to resume the thread later
- The workspace is just temporarily not active in `MultiWorkspace`

### 4.2 Cleanup Operations

1. Look up persisted `AgentGitWorktreeInfo` for the session.
2. Remove the workspace from `MultiWorkspace::remove_workspace()` if present.
3. Run `remove_worktree(path, force)` through `project::Repository`.
4. Optionally delete the branch via existing branch APIs when safe.

### 4.3 Orphan Detection

On startup, compare:

- Persisted `AgentGitWorktreeInfo` entries from thread storage.
- Actual git worktrees from repository APIs.

If a managed worktree exists without an associated live thread/session record,
mark it orphaned and offer cleanup (or auto-cleanup after policy delay).

---

## Phase 5: Testing

### 5.1 Unit Tests — Git Repository API

Location: `crates/git/src/repository.rs` (in the existing `mod tests` block)

- **`test_parse_worktrees_from_str`**: Verify parsing of `git worktree list
--porcelain` output. Test with zero, one, and multiple worktrees. Test with
  detached HEAD entries and bare repos.
- **`test_create_and_list_worktrees`**: Create a real git repo in a temp dir,
  add a worktree via the API, verify `worktrees()` returns it.
- **`test_remove_worktree`**: Create a worktree, remove it, verify it's gone
  from `worktrees()` and the directory is deleted.
- **`test_remove_worktree_force`**: Create a worktree with uncommitted changes,
  verify non-force removal fails, force removal succeeds.
- **`test_rename_worktree`**: Create a worktree, move it to a new path, verify
  the old path is gone and the new path exists.

### 5.2 Unit Tests — FakeGitRepository

Location: `crates/fs/src/fake_git_repo.rs`

Implement `worktrees()`, `create_worktree()`, `remove_worktree()`, and
`rename_worktree()` on `FakeGitRepository` backed by
`FakeGitRepositoryState`. These are currently `unimplemented!()`.

- **`test_fake_worktree_lifecycle`**: Create, list, remove worktrees on the
  fake to verify the test infrastructure works.

### 5.3 Unit Tests — Branch Name Generation

Location: `crates/agent_ui/src/agent_worktree.rs`

- **`test_branch_name_generation`**: Verify generated names match the
  `zed/agent/<id>` pattern, are valid git branch names, and are unique across
  multiple calls.

### 5.4 Integration Tests — Worktree Creation Flow

Location: `crates/agent_ui/src/agent_worktree.rs`

Using `TestAppContext`, `FakeFs`, and `FakeGitRepository`:

- **`test_create_agent_worktree`**: Trigger the full flow (create git worktree
  → open project in new workspace → activate workspace → create ACP thread).
- **`test_create_agent_worktree_failure`**: Simulate `create_worktree()` failure
  and verify the error is surfaced to the UI (no silent failures).
- **`test_create_agent_worktree_rollback`**: Simulate failure after worktree
  creation and verify rollback/cleanup occurs.
- **`test_cleanup_agent_worktree`**: Create a worktree, then discard it. Verify
  the workspace is removed from `MultiWorkspace` and the git worktree is
  cleaned up.

### 5.5 Integration Tests — Thread ↔ Worktree Association

Location: `crates/agent_ui/src/agent_panel.rs` (in the existing test module)

- **`test_thread_target_local_project`**: Start a thread with
  `ThreadTarget::LocalProject` and verify it runs in the current workspace (no
  worktree created).
- **`test_thread_target_new_worktree`**: Start a thread with
  `ThreadTarget::NewWorktree`, verify a git worktree is created, a new workspace
  is added, and the thread runs there.
- **`test_history_row_displays_worktree`**: Create a thread with persisted
  `AgentGitWorktreeInfo`, render ACP history, and verify branch/worktree label
  appears in the existing `ListItem` row renderer.

### 5.6 Settings Tests

Location: `crates/project/src/project_settings.rs` and
`crates/settings_content/src/project.rs`

- **`test_agent_worktree_directory_default`**: Verify the default path resolution
  when `agent_worktree_directory` is `None`.
- **`test_agent_worktree_directory_absolute`**: Verify an absolute path is used
  as-is.
- **`test_agent_worktree_directory_relative`**: Verify a relative path is
  resolved relative to the project root.
- **`test_git_settings_deserialize_agent_worktree_directory`**: Verify settings
  content and runtime settings deserialize/merge correctly.

### 5.7 Persistence + Session List Tests

Location: `crates/agent/src/db.rs`, `crates/agent/src/agent.rs`

- **`test_db_thread_roundtrip_agent_git_worktree_info`**: Verify
  `AgentGitWorktreeInfo` persists and restores.
- **`test_session_list_includes_worktree_meta`**: Verify session list entries
  expose worktree metadata for history rendering/resume.

### 5.8 GitStore/Proto Tests

Location: `crates/project/src/git_store.rs`, `crates/proto/proto/git.proto`

- **`test_repository_remove_worktree_local`**
- **`test_repository_rename_worktree_local`**
- **`test_repository_remove_worktree_remote_roundtrip`**
- **`test_repository_rename_worktree_remote_roundtrip`**

---

## Task Breakdown

### Milestone 1: Git Repository API

- [ ] Add `remove_worktree(path, force)` to `GitRepository` trait
- [ ] Implement `remove_worktree` on `RealGitRepository` (runs `git worktree remove`)
- [ ] Add `rename_worktree(old_path, new_path)` to `GitRepository` trait
- [ ] Implement `rename_worktree` on `RealGitRepository` (runs `git worktree move`)
- [ ] Implement `worktrees()`, `create_worktree()`, `remove_worktree()`, and
      `rename_worktree()` on `FakeGitRepository` (currently `unimplemented!()`)
- [ ] Write tests: `test_parse_worktrees_from_str`, `test_create_and_list_worktrees`,
      `test_remove_worktree`, `test_remove_worktree_force`, `test_rename_worktree`
- [ ] Write test: `test_fake_worktree_lifecycle`

### Milestone 2: Project Repository + Proto Plumbing

- [ ] Add `remove_worktree(path, force)` and `rename_worktree(old_path, new_path)`
      on `project::Repository` (`crates/project/src/git_store.rs`)
- [ ] Add proto requests/responses for remove/rename worktree
- [ ] Wire `GitStore` request handlers (local + remote)
- [ ] Add local and remote roundtrip tests for new RPCs

### Milestone 3: Settings

- [ ] Add `agent_worktree_directory: Option<String>` in
      `crates/settings_content/src/project.rs`
- [ ] Add `agent_worktree_directory: Option<String>` to `GitSettings` in
      `crates/project/src/project_settings.rs`
- [ ] Remove/adjust `Copy` derives where incompatible with string field
- [ ] Implement path resolution logic (default data dir vs. absolute vs.
      project-relative)
- [ ] Write tests: `test_agent_worktree_directory_default`,
      `test_agent_worktree_directory_absolute`,
      `test_agent_worktree_directory_relative`,
      `test_git_settings_deserialize_agent_worktree_directory`

### Milestone 4: Persistence Model

- [ ] Define `AgentGitWorktreeInfo`
- [ ] Persist `AgentGitWorktreeInfo` in thread DB models
- [ ] Include worktree metadata in session list output (`AgentSessionInfo.meta`)
- [ ] Add persistence + session list tests

### Milestone 5: UI Shell (`AgentPanel`)

- [ ] Define `ThreadTarget` enum
- [ ] Add "Start Thread In…" dropdown to the new-thread UI in `agent_panel.rs`,
      gated behind `AgentV2FeatureFlag`, reusing `PopoverMenu`, `ContextMenu`,
      `ContextMenuEntry`, and `Button` patterns from the existing toolbar
- [ ] Add worktree label rendering to ACP history list rows in
      `thread_history.rs` using metadata
- [ ] Write test: `test_history_row_displays_worktree`

### Milestone 6: Local Worktree Orchestration + Workspace Integration

- [ ] Create `crates/agent_ui/src/agent_worktree.rs` with orchestration logic
- [ ] Implement branch name generation (`zed/agent/<id>`)
- [ ] On new thread creation with `ThreadTarget::NewWorktree`:
  - Create git worktree via `GitRepository::create_worktree()`
  - Trust the new path (reuse pattern from `worktree_picker.rs`)
  - Open as a guaranteed new workspace
  - Activate the new workspace
  - Start the agent thread in the new workspace's `AgentPanel`
- [ ] Persist `AgentGitWorktreeInfo`
- [ ] Add rollback on partial failure

### Milestone 7: Local Integration Tests (Mid-Project)

- [ ] Write tests: `test_branch_name_generation`, `test_create_agent_worktree`,
      `test_create_agent_worktree_failure`, `test_create_agent_worktree_rollback`,
      `test_thread_target_local_project`, `test_thread_target_new_worktree`

### Milestone 8: Remote Wiring (Final)

- [ ] Enable "New Worktree" target for remote/collab projects
- [ ] Route remove/rename worktree through remote git RPC
- [ ] Reuse remote workspace opening path (`open_remote_worktree` pattern)
- [ ] Add/extend remote integration tests

### Milestone 9: Cleanup + Orphan Lifecycle

- [ ] Implement cleanup when a worktree thread is discarded
- [ ] Remove workspace from `MultiWorkspace` on cleanup
- [ ] Run `remove_worktree(path, force)` + optional branch deletion
- [ ] Handle orphaned worktrees on startup
- [ ] Write test: `test_cleanup_agent_worktree`
- [ ] Persist `AgentGitWorktreeInfo` in thread DB as the canonical source of truth

---

## Key Files to Modify

| File                                        | Changes                                                                                |
| ------------------------------------------- | -------------------------------------------------------------------------------------- |
| `crates/git/src/repository.rs`              | Add `remove_worktree()`, `rename_worktree()` to trait + impl; add tests                |
| `crates/fs/src/fake_git_repo.rs`            | Implement `worktrees()`, `create_worktree()`, `remove_worktree()`, `rename_worktree()` |
| `crates/project/src/git_store.rs`           | Add project repository methods + local/remote routing + handlers                       |
| `crates/proto/proto/git.proto`              | Add remove/rename worktree request/response messages                                   |
| `crates/proto/src/proto.rs`                 | Register new proto messages                                                            |
| `crates/settings_content/src/project.rs`    | Add `agent_worktree_directory` to settings content `GitSettings`                       |
| `crates/project/src/project_settings.rs`    | Add runtime `agent_worktree_directory` field and mapping logic                         |
| `crates/agent_ui/src/agent_panel.rs`        | Thread target selector + trigger orchestration                                         |
| `crates/agent_ui/src/agent_worktree.rs`     | **New file** — orchestration, branch naming, error rollback, tests                     |
| `crates/agent_ui/src/acp/thread_history.rs` | Render worktree label in history rows                                                  |
| `crates/agent_ui/src/acp/thread_view.rs`    | Hook startup flow and resume/workspace behavior                                        |
| `crates/agent/src/db.rs`                    | Persist `AgentGitWorktreeInfo`                                                         |
| `crates/agent/src/agent.rs`                 | Populate session list meta from persisted worktree info                                |
| `crates/git_ui/src/worktree_picker.rs`      | Reference implementation for create + trust + open flow (no required changes)          |

## Existing UI Components to Reuse

| Component                            | Location                                          | Used For                                     |
| ------------------------------------ | ------------------------------------------------- | -------------------------------------------- |
| `PopoverMenu`                        | `gpui`                                            | The "Start Thread In…" dropdown container    |
| `ContextMenu` + `ContextMenuEntry`   | `ui`                                              | Menu items ("Local Project", "New Worktree") |
| `Button` with `ChevronDown` icon     | `ui`                                              | Dropdown trigger (same as `ModeSelector`)    |
| `Tooltip` / `.documentation_aside()` | `ui`                                              | Tooltip explaining worktree behavior         |
| `ListItem` + `Label` + `Icon`        | `crates/agent_ui/src/acp/thread_history.rs`       | History row rendering with worktree sublabel |
| `SpinnerLabel`                       | `crates/ui/src/components/label/spinner_label.rs` | Loading indicator during worktree creation   |
| Error prompt/toast patterns          | `workspace` / `git_ui`                            | Pre-thread creation error display            |
| `AcpThreadView::thread_error`        | `crates/agent_ui/src/acp/thread_view`             | Error display once thread view exists        |
| `TrustedWorktrees`                   | `crates/workspace`                                | Trust management for new worktree paths      |

## Open Questions

- **Branch renaming**: Should we rename the branch once the agent generates a
  thread title? If so, when exactly — after the first assistant response? This
  would use the existing `GitRepository::rename_branch()` + the new
  `GitRepository::rename_worktree()` if the directory should also change.
- **Multiple repos**: If a project has multiple git repositories, which one
  do we create the worktree in? Probably the "primary" one, but this needs
  design.
- **Metadata shape**: Should `AgentGitWorktreeInfo` be denormalized into a
  dedicated thread metadata table/column for fast list queries, or only stored
  in full `DbThread` blobs?
