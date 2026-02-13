use std::collections::HashSet;
use std::path::{Path, PathBuf};

use agent::AgentGitWorktreeInfo;
use agent_client_protocol as acp;
use anyhow::{Context as _, Result};
use gpui::SharedString;
use project::project_settings::ProjectSettings;
use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
use settings::Settings;
use workspace::{MultiWorkspace, OpenOptions};

use crate::agent_panel::{AgentPanel, ThreadTarget, WorktreeCreationStatus};

/// Generate a branch name for an agent worktree.
///
/// Format: `zed/agent/<short-id>` where `<short-id>` is a random
/// 5-character alphanumeric string.
pub fn generate_branch_name() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const ID_LENGTH: usize = 5;

    let mut rng = rand::rng();
    let short_id: String = (0..ID_LENGTH)
        .map(|_| {
            let index = rand::Rng::random_range(&mut rng, 0..CHARSET.len());
            CHARSET[index] as char
        })
        .collect();

    format!("zed/agent/{short_id}")
}

/// Resolve the directory where an agent worktree should be created.
///
/// The resolution order is:
/// 1. If `configured_directory` is `Some` and is an absolute path, use it directly.
/// 2. If `configured_directory` is `Some` and is a relative path, resolve it
///    relative to `project_root`.
/// 3. If `configured_directory` is `None`, use the default location under the
///    Zed data directory: `<data_dir>/agent-worktrees/<repo_name>/`.
pub fn resolve_worktree_directory(
    configured_directory: Option<&str>,
    project_root: &Path,
    repo_name: &str,
) -> Result<PathBuf> {
    match configured_directory {
        Some(directory) => {
            let path = PathBuf::from(directory);
            if path.is_absolute() {
                Ok(path)
            } else {
                Ok(project_root.join(path))
            }
        }
        None => {
            let data_dir = paths::data_dir();
            Ok(data_dir.join("agent-worktrees").join(repo_name))
        }
    }
}

/// Extract the repository name from a project root path.
///
/// Uses the last component of the path as the repo name, falling back
/// to "unknown" if the path has no file name component.
pub fn repo_name_from_path(project_root: &Path) -> &str {
    project_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
}

/// Spawn the async orchestration that creates a git worktree, opens it as a
/// new workspace in the current MultiWorkspace, and starts an agent thread there.
///
/// This is called from `AgentPanel::new_thread` when the user has
/// `ThreadTarget::NewWorktree` selected.
pub fn create_worktree_and_start_thread(
    agent_panel: &mut AgentPanel,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<AgentPanel>,
) {
    agent_panel.worktree_creation_status = Some(WorktreeCreationStatus::Creating);
    agent_panel.thread_target = ThreadTarget::LocalProject;
    cx.notify();

    let project = agent_panel.project.clone();
    let workspace = agent_panel.workspace.clone();

    #[cfg(any(test, feature = "test-support"))]
    let simulate_post_creation_failure = agent_panel.simulate_post_creation_failure;

    cx.spawn_in(window, async move |this, cx| {
        // Step 1: Get the active repository and project settings.
        let (repo, work_dir, configured_directory, fs) = cx
            .update(|_window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let repo = git_store
                    .read(cx)
                    .active_repository()
                    .context("no active git repository")?;

                let work_dir = repo.read(cx).snapshot().work_directory_abs_path;

                let settings = ProjectSettings::get_global(cx);
                let configured_directory = settings.git.agent_worktree_directory.clone();

                let fs = project.read(cx).fs().clone();

                anyhow::Ok((repo, work_dir, configured_directory, fs))
            })
            .context("failed to read project state")?
            .context("failed to read project")?;

        // Step 2: Generate branch name and resolve storage directory.
        let branch_name = generate_branch_name();
        let repo_name = repo_name_from_path(&work_dir);
        let worktree_directory =
            resolve_worktree_directory(configured_directory.as_deref(), &work_dir, repo_name)?;

        // Ensure the parent directory exists.
        fs.create_dir(&worktree_directory)
            .await
            .context("failed to create worktree storage directory")?;

        // Step 3: Create the git worktree. Use "HEAD" as the base commit so the
        // agent starts from the same state the user is looking at.
        let create_result = repo
            .update(cx, |repo, _cx| {
                repo.create_worktree(
                    branch_name.clone(),
                    worktree_directory.clone(),
                    Some("HEAD".to_string()),
                )
            })
            .await;

        let create_result = match create_result {
            Ok(inner) => inner,
            Err(error) => Err(anyhow::anyhow!("{error:#}")),
        };

        if let Err(error) = create_result {
            let message: SharedString = format!("{error:#}").into();
            this.update_in(cx, |agent_panel, _window, cx| {
                agent_panel.worktree_creation_status = Some(WorktreeCreationStatus::Error(message));
                cx.notify();
            })?;
            return anyhow::Ok(());
        }

        // From this point on, if anything fails we need to roll back
        // the git worktree that was successfully created.
        let new_worktree_path = worktree_directory.join(&branch_name);
        let worktree_info = AgentGitWorktreeInfo {
            branch: branch_name.clone(),
            worktree_path: new_worktree_path.clone(),
            base_ref: "HEAD".to_string(),
        };

        let result = create_worktree_post_steps(
            &this,
            &workspace,
            &repo,
            &new_worktree_path,
            worktree_info,
            #[cfg(any(test, feature = "test-support"))]
            simulate_post_creation_failure,
            cx,
        )
        .await;

        // Rollback: if workspace open or thread startup failed after the
        // git worktree was already created, clean it up.
        if let Err(error) = result {
            let remove_result = repo
                .update(cx, |repo, _cx| {
                    repo.remove_worktree(new_worktree_path.clone(), true)
                })
                .await;

            let remove_result = match remove_result {
                Ok(inner) => inner,
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            };

            if let Err(rollback_error) = remove_result {
                log::warn!("failed to roll back git worktree: {rollback_error:#}");
            }

            let message: SharedString = format!("{error:#}").into();
            this.update_in(cx, |agent_panel, _window, cx| {
                agent_panel.worktree_creation_status = Some(WorktreeCreationStatus::Error(message));
                cx.notify();
            })?;
            return anyhow::Ok(());
        }

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Post-creation steps: trust the path, clear status, open workspace, start
/// thread, and persist `AgentGitWorktreeInfo`. Extracted so that on failure
/// the caller can roll back the git worktree.
async fn create_worktree_post_steps(
    this: &gpui::WeakEntity<AgentPanel>,
    workspace: &gpui::WeakEntity<workspace::Workspace>,
    repo: &gpui::Entity<project::git_store::Repository>,
    new_worktree_path: &Path,
    worktree_info: AgentGitWorktreeInfo,
    #[cfg(any(test, feature = "test-support"))] simulate_failure: bool,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    if simulate_failure {
        anyhow::bail!("simulated post-creation failure");
    }

    // Step 4: Trust the new worktree path (following worktree_picker.rs pattern).
    workspace
        .update(cx, |workspace, cx| {
            let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
                return;
            };

            let project_handle = workspace.project();
            let repo_snapshot = repo.read(cx).snapshot();
            let repo_path = &repo_snapshot.work_directory_abs_path;

            let Some((parent_worktree, _)) = project_handle.read(cx).find_worktree(repo_path, cx)
            else {
                return;
            };

            let worktree_store = project_handle.read(cx).worktree_store();
            let parent_id = parent_worktree.read(cx).id();

            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                if trusted_worktrees.can_trust(&worktree_store, parent_id, cx) {
                    trusted_worktrees.trust(
                        &worktree_store,
                        HashSet::from_iter([PathTrust::AbsPath(new_worktree_path.to_path_buf())]),
                        cx,
                    );
                }
            });
        })
        .ok();

    // Step 5: Get app_state and window handle for opening a new workspace
    // within the existing MultiWorkspace.
    let (app_state, window_handle) = workspace
        .update_in(cx, |workspace, window, _cx| {
            let app_state = workspace.app_state().clone();
            let window_handle = window.window_handle().downcast::<MultiWorkspace>();
            (app_state, window_handle)
        })
        .context("failed to read workspace state")?;

    // Step 6: Open the worktree as a new workspace in the same MultiWorkspace.
    // Using `open_new_workspace: Some(true)` ensures we always create a new
    // workspace rather than reusing an existing one. Setting `replace_window`
    // to the current window handle causes `Workspace::new_local` to add the
    // workspace to the existing MultiWorkspace instead of opening a new OS window.
    let worktree_path = new_worktree_path.to_path_buf();
    let open_task = cx.update(|_window, cx| {
        workspace::open_paths(
            &[worktree_path],
            app_state,
            OpenOptions {
                replace_window: window_handle,
                open_new_workspace: Some(true),
                ..Default::default()
            },
            cx,
        )
    })?;

    let (multi_workspace_handle, _items) = open_task
        .await
        .context("failed to open worktree as workspace")?;

    // Step 7: Read the new workspace and project from the MultiWorkspace.
    // We use `read_with` on the window handle (rather than reading from
    // inside an `update_in` callback) because `update_window` temporarily
    // takes the window out of the map, making nested reads fail.
    let (new_workspace, new_project) = multi_workspace_handle
        .read_with(cx, |multi, cx| {
            let workspace = multi.workspace().clone();
            let project = workspace.read(cx).project().clone();
            (workspace.downgrade(), project)
        })
        .context("failed to find new workspace in MultiWorkspace")?;

    // Step 8: Clear creation status, start the thread, and persist worktree info.
    this.update_in(cx, |agent_panel, window, cx| {
        agent_panel.worktree_creation_status = None;
        cx.notify();

        agent_panel.start_native_thread_in_workspace(new_workspace, new_project, window, cx);

        // Step 9: Persist AgentGitWorktreeInfo on the newly created thread.
        if let Some(thread) = agent_panel.active_native_agent_thread(cx) {
            thread.update(cx, |thread, _cx| {
                thread.set_git_worktree_info(worktree_info);
            });
        }
    })?;

    Ok(())
}

/// Clean up a git worktree (if any) associated with a thread, then delete the
/// thread from the database. Called when the user deletes a single history entry.
pub fn cleanup_and_delete_thread(
    agent_panel: &mut AgentPanel,
    session_id: &acp::SessionId,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<AgentPanel>,
) {
    let session_id = session_id.clone();
    let thread_store = agent_panel.thread_store.clone();
    let project = agent_panel.project.clone();
    let acp_history = agent_panel.acp_history.clone();

    cx.spawn_in(_window, async move |this, cx| {
        let worktree_info = thread_store
            .update(cx, |store, cx| store.load_thread(session_id.clone(), cx))
            .await?
            .and_then(|thread| thread.git_worktree_info);

        if let Some(info) = &worktree_info {
            remove_worktree_workspace(&this, info, cx).ok();
            cleanup_git_worktree(info, &project, &mut *cx).await;
        }

        acp_history
            .update(cx, |history, cx| history.delete_session(&session_id, cx))
            .await?;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Clean up all git worktrees associated with threads, then delete all threads
/// from the database. Called when the user clears all history.
pub fn cleanup_and_delete_all_threads(
    agent_panel: &mut AgentPanel,
    _window: &mut gpui::Window,
    cx: &mut gpui::Context<AgentPanel>,
) {
    let thread_store = agent_panel.thread_store.clone();
    let project = agent_panel.project.clone();
    let acp_history = agent_panel.acp_history.clone();

    cx.spawn_in(_window, async move |this, cx| {
        let worktree_session_ids: Vec<acp::SessionId> = thread_store.read_with(cx, |store, _cx| {
            store
                .entries()
                .filter(|entry| entry.worktree_branch.is_some())
                .map(|entry| entry.id)
                .collect()
        });

        for session_id in worktree_session_ids {
            let info = thread_store
                .update(cx, |store, cx| store.load_thread(session_id, cx))
                .await?
                .and_then(|thread| thread.git_worktree_info);

            if let Some(info) = &info {
                remove_worktree_workspace(&this, info, cx).ok();
                cleanup_git_worktree(info, &project, &mut *cx).await;
            }
        }

        acp_history
            .update(cx, |history, cx| history.delete_sessions(cx))
            .await?;

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Remove a git worktree from the repository. Logs warnings on failure rather
/// than propagating errors, because cleanup is best-effort — the thread
/// deletion should still proceed even if the worktree removal fails.
fn remove_worktree_workspace(
    this: &gpui::WeakEntity<AgentPanel>,
    info: &AgentGitWorktreeInfo,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let worktree_path = info.worktree_path.clone();
    this.update_in(cx, |_agent_panel, window, cx| {
        let Some(Some(multi)) = window.root::<MultiWorkspace>() else {
            return;
        };
        let workspaces = multi.read(cx).workspaces().to_vec();
        for (index, workspace) in workspaces.iter().enumerate().rev() {
            let has_matching_worktree = workspace
                .read(cx)
                .worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == worktree_path.as_path());
            if has_matching_worktree {
                multi.update(cx, |multi, cx| {
                    multi.remove_workspace(index, window, cx);
                });
                break;
            }
        }
    })?;
    Ok(())
}

async fn cleanup_git_worktree(
    info: &AgentGitWorktreeInfo,
    project: &gpui::Entity<project::Project>,
    cx: &mut gpui::AsyncApp,
) {
    let worktree_path = info.worktree_path.clone();

    let remove_result = project.update(cx, |project, cx| {
        let Some(repo) = project.git_store().read(cx).active_repository() else {
            log::warn!(
                "no active repository to clean up worktree at {}",
                worktree_path.display()
            );
            return None;
        };
        Some(repo.update(cx, |repo, _cx| {
            repo.remove_worktree(worktree_path.clone(), true)
        }))
    });

    if let Some(receiver) = remove_result {
        match receiver.await {
            Ok(Ok(())) => {
                log::info!(
                    "cleaned up agent worktree at {}",
                    info.worktree_path.display()
                );
            }
            Ok(Err(error)) => {
                log::warn!(
                    "failed to remove agent worktree at {}: {error:#}",
                    info.worktree_path.display()
                );
            }
            Err(error) => {
                log::warn!(
                    "failed to remove agent worktree at {}: {error:#}",
                    info.worktree_path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_branch_name_generation() {
        let name = generate_branch_name();

        // Verify the prefix
        assert!(
            name.starts_with("zed/agent/"),
            "branch name should start with 'zed/agent/', got: {name}"
        );

        // Verify the short-id length
        let short_id = name.strip_prefix("zed/agent/").unwrap();
        assert_eq!(
            short_id.len(),
            5,
            "short id should be 5 characters, got: {short_id}"
        );

        // Verify all characters are alphanumeric
        assert!(
            short_id.chars().all(|c| c.is_ascii_alphanumeric()),
            "short id should be alphanumeric, got: {short_id}"
        );

        // Verify uniqueness across multiple calls
        let names: std::collections::HashSet<String> =
            (0..20).map(|_| generate_branch_name()).collect();
        assert!(
            names.len() > 1,
            "generated names should be unique across calls"
        );
    }

    #[test]
    fn test_branch_name_is_valid_git_ref() {
        for _ in 0..50 {
            let name = generate_branch_name();
            // Git branch names cannot contain spaces, ~, ^, :, ?, *, [, \
            // or start/end with a dot, or contain ".."
            assert!(
                !name.contains(' ')
                    && !name.contains('~')
                    && !name.contains('^')
                    && !name.contains(':')
                    && !name.contains('?')
                    && !name.contains('*')
                    && !name.contains('[')
                    && !name.contains('\\')
                    && !name.contains("..")
                    && !name.starts_with('.')
                    && !name.ends_with('.'),
                "branch name should be a valid git ref: {name}"
            );
        }
    }

    #[test]
    fn test_resolve_worktree_directory_default() {
        let result =
            resolve_worktree_directory(None, Path::new("/home/user/project"), "my-project")
                .unwrap();

        let expected = paths::data_dir().join("agent-worktrees").join("my-project");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_resolve_worktree_directory_absolute() {
        let result = resolve_worktree_directory(
            Some("/custom/worktrees"),
            Path::new("/home/user/project"),
            "my-project",
        )
        .unwrap();

        assert_eq!(result, PathBuf::from("/custom/worktrees"));
    }

    #[test]
    fn test_resolve_worktree_directory_relative() {
        let result = resolve_worktree_directory(
            Some(".worktrees"),
            Path::new("/home/user/project"),
            "my-project",
        )
        .unwrap();

        assert_eq!(result, PathBuf::from("/home/user/project/.worktrees"));
    }

    #[test]
    fn test_repo_name_from_path() {
        assert_eq!(
            repo_name_from_path(Path::new("/home/user/my-project")),
            "my-project"
        );
        assert_eq!(repo_name_from_path(Path::new("/home/user/zed")), "zed");
        assert_eq!(repo_name_from_path(Path::new("/")), "unknown");
    }
}
