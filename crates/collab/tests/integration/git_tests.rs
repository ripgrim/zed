use std::path::{Path, PathBuf};
use std::sync::Arc;

use call::ActiveCall;
use fs::Fs as _;
use git::status::{FileStatus, StatusCode, TrackedStatus};
use git_ui::project_diff::ProjectDiff;
use gpui::{AppContext as _, BackgroundExecutor, Entity, TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use project::git_store::{Repository, RepositoryEvent};
use project::{Project, ProjectPath};
use serde_json::json;
use util::{path, rel_path::rel_path};
use workspace::{MultiWorkspace, Workspace};

//
use crate::{TestClient, TestServer};

#[gpui::test]
async fn test_project_diff(cx_a: &mut TestAppContext, cx_b: &mut TestAppContext) {
    let mut server = TestServer::start(cx_a.background_executor.clone()).await;
    let client_a = server.create_client(cx_a, "user_a").await;
    let client_b = server.create_client(cx_b, "user_b").await;
    cx_a.set_name("cx_a");
    cx_b.set_name("cx_b");

    server
        .create_room(&mut [(&client_a, cx_a), (&client_b, cx_b)])
        .await;

    client_a
        .fs()
        .insert_tree(
            path!("/a"),
            json!({
                ".git": {},
                "changed.txt": "after\n",
                "unchanged.txt": "unchanged\n",
                "created.txt": "created\n",
                "secret.pem": "secret-changed\n",
            }),
        )
        .await;

    client_a.fs().set_head_and_index_for_repo(
        Path::new(path!("/a/.git")),
        &[
            ("changed.txt", "before\n".to_string()),
            ("unchanged.txt", "unchanged\n".to_string()),
            ("deleted.txt", "deleted\n".to_string()),
            ("secret.pem", "shh\n".to_string()),
        ],
    );
    let (project_a, worktree_id) = client_a.build_local_project(path!("/a"), cx_a).await;
    let active_call_a = cx_a.read(ActiveCall::global);
    let project_id = active_call_a
        .update(cx_a, |call, cx| call.share_project(project_a.clone(), cx))
        .await
        .unwrap();

    cx_b.update(editor::init);
    cx_b.update(git_ui::init);
    let project_b = client_b.join_remote_project(project_id, cx_b).await;
    let window_b = cx_b.add_window(|window, cx| {
        let workspace = cx.new(|cx| {
            Workspace::new(
                None,
                project_b.clone(),
                client_b.app_state.clone(),
                window,
                cx,
            )
        });
        MultiWorkspace::new(workspace, window, cx)
    });
    let cx_b = &mut VisualTestContext::from_window(*window_b, cx_b);
    let workspace_b = window_b
        .root(cx_b)
        .unwrap()
        .read_with(cx_b, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });

    cx_b.update(|window, cx| {
        window
            .focused(cx)
            .unwrap()
            .dispatch_action(&git_ui::project_diff::Diff, window, cx)
    });
    let diff = workspace_b.update(cx_b, |workspace, cx| {
        workspace.active_item(cx).unwrap().act_as::<ProjectDiff>(cx)
    });
    let diff = diff.unwrap();
    cx_b.run_until_parked();

    diff.update(cx_b, |diff, cx| {
        assert_eq!(
            diff.excerpt_paths(cx),
            vec![
                rel_path("changed.txt").into_arc(),
                rel_path("deleted.txt").into_arc(),
                rel_path("created.txt").into_arc()
            ]
        );
    });

    client_a
        .fs()
        .insert_tree(
            path!("/a"),
            json!({
                ".git": {},
                "changed.txt": "before\n",
                "unchanged.txt": "changed\n",
                "created.txt": "created\n",
                "secret.pem": "secret-changed\n",
            }),
        )
        .await;
    cx_b.run_until_parked();

    project_b.update(cx_b, |project, cx| {
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path("unchanged.txt").into(),
        };
        let status = project.project_path_git_status(&project_path, cx);
        assert_eq!(
            status.unwrap(),
            FileStatus::Tracked(TrackedStatus {
                worktree_status: StatusCode::Modified,
                index_status: StatusCode::Unmodified,
            })
        );
    });

    diff.update(cx_b, |diff, cx| {
        assert_eq!(
            diff.excerpt_paths(cx),
            vec![
                rel_path("deleted.txt").into_arc(),
                rel_path("unchanged.txt").into_arc(),
                rel_path("created.txt").into_arc()
            ]
        );
    });
}

async fn setup_remote_git_project(
    executor: &BackgroundExecutor,
    server: &mut TestServer,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) -> (TestClient, TestClient, Entity<Project>, Entity<Repository>) {
    let client_a = server.create_client(cx_a, "user_a").await;
    let client_b = server.create_client(cx_b, "user_b").await;
    server
        .create_room(&mut [(&client_a, cx_a), (&client_b, cx_b)])
        .await;
    let active_call_a = cx_a.read(ActiveCall::global);

    client_a
        .fs()
        .insert_tree(path!("/project"), json!({ ".git": {} }))
        .await;
    client_a
        .fs()
        .insert_branches(Path::new(path!("/project/.git")), &["main"]);

    let (project_a, _) = client_a.build_local_project(path!("/project"), cx_a).await;
    let project_id = active_call_a
        .update(cx_a, |call, cx| call.share_project(project_a.clone(), cx))
        .await
        .unwrap();
    let project_b = client_b.join_remote_project(project_id, cx_b).await;
    executor.run_until_parked();

    let repo_b = cx_b.update(|cx| project_b.read(cx).active_repository(cx).unwrap());

    (client_a, client_b, project_a, repo_b)
}

#[gpui::test]
async fn test_repository_remove_worktree_remote_roundtrip(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Verify we can call branches() on the remote repo (proven pattern).
    let branches = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.branches()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        branches.iter().any(|b| b.name() == "main"),
        "should see main branch via remote"
    );

    // Pre-populate a worktree on the host so we can remove it via remote.
    // Create the directory first since remove_worktree does filesystem operations.
    client_a
        .fs()
        .create_dir(Path::new("/worktrees/test-branch"))
        .await
        .unwrap();
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/test-branch"),
                ref_name: "refs/heads/test-branch".into(),
                sha: "abc123".into(),
            });
        })
        .unwrap();

    // Verify the worktree exists before removing it.
    let worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worktrees.len(),
        1,
        "should have one worktree before removal"
    );

    // Remove the worktree via the remote RPC path.
    cx_b.update(|cx| {
        repo_b.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/test-branch"), false, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify the worktree was removed on the host.
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert!(
                state.worktrees.is_empty(),
                "worktree should be removed on host"
            );
        })
        .unwrap();

    // Verify the remote client also sees the updated (empty) worktree list.
    let remote_worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        remote_worktrees.is_empty(),
        "remote client should see no worktrees after removal"
    );
}

#[gpui::test]
async fn test_repository_rename_worktree_remote_roundtrip(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Pre-populate a worktree on the host so we can rename it via remote.
    // Create the directory first since rename_worktree does filesystem operations.
    client_a
        .fs()
        .create_dir(Path::new("/worktrees/old-branch"))
        .await
        .unwrap();
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/old-branch"),
                ref_name: "refs/heads/old-branch".into(),
                sha: "abc123".into(),
            });
        })
        .unwrap();

    // Verify the worktree exists before renaming it.
    let worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should have one worktree before rename");

    // Rename the worktree via the remote RPC path.
    cx_b.update(|cx| {
        repo_b.update(cx, |repo, cx| {
            repo.rename_worktree(
                PathBuf::from("/worktrees/old-branch"),
                PathBuf::from("/worktrees/new-branch"),
                cx,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify the worktree was renamed on the host.
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert_eq!(state.worktrees.len(), 1, "should still have one worktree");
            assert_eq!(
                state
                    .worktrees
                    .first()
                    .expect("should have one worktree")
                    .path,
                PathBuf::from("/worktrees/new-branch"),
                "worktree path should be renamed on host"
            );
        })
        .unwrap();

    // Verify the remote client also sees the renamed worktree.
    let remote_worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        remote_worktrees.len(),
        1,
        "remote client should see one worktree"
    );
    assert_eq!(
        remote_worktrees[0].path,
        PathBuf::from("/worktrees/new-branch"),
        "remote client should see the renamed path"
    );
}

#[gpui::test]
async fn test_repository_remove_nonexistent_worktree_remote_error(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Try to remove a worktree that doesn't exist via the remote RPC path.
    let result = cx_b
        .update(|cx| {
            repo_b.update(cx, |repo, cx| {
                repo.remove_worktree(PathBuf::from("/worktrees/nonexistent"), false, cx)
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "removing a nonexistent worktree should return an error"
    );
    executor.run_until_parked();

    // Verify host state is unchanged (still no worktrees).
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert!(
                state.worktrees.is_empty(),
                "host should still have no worktrees"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn test_repository_worktree_ops_local(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    _cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let client = server.create_client(cx_a, "user").await;

    client
        .fs()
        .insert_tree(path!("/project"), json!({ ".git": {} }))
        .await;
    client
        .fs()
        .insert_branches(Path::new(path!("/project/.git")), &["main"]);

    let (project, _) = client.build_local_project(path!("/project"), cx_a).await;
    executor.run_until_parked();

    let repo = cx_a.update(|cx| project.read(cx).active_repository(cx).unwrap());

    // --- Test remove_worktree locally ---

    // Set up a worktree on disk + in state.
    client
        .fs()
        .create_dir(Path::new("/worktrees/remove-me"))
        .await
        .unwrap();
    client
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/remove-me"),
                ref_name: "refs/heads/remove-me".into(),
                sha: "aaa111".into(),
            });
        })
        .unwrap();

    // Verify it exists.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worktrees.len(),
        1,
        "should have one worktree before removal"
    );

    // Track WorktreesChanged events.
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscription = cx_a.update(|cx| {
        let events = events.clone();
        cx.subscribe(&repo, move |_, event: &RepositoryEvent, _| {
            events.lock().push(event.clone());
        })
    });

    // Remove the worktree via the local code path.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/remove-me"), false, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify removal.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        worktrees.is_empty(),
        "should have no worktrees after removal"
    );

    // Verify event was emitted.
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted for remove"
    );
    events.lock().clear();

    // --- Test rename_worktree locally ---

    // Set up another worktree.
    client
        .fs()
        .create_dir(Path::new("/worktrees/old-name"))
        .await
        .unwrap();
    client
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/old-name"),
                ref_name: "refs/heads/old-name".into(),
                sha: "bbb222".into(),
            });
        })
        .unwrap();

    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should have one worktree before rename");

    // Rename via local code path.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.rename_worktree(
                PathBuf::from("/worktrees/old-name"),
                PathBuf::from("/worktrees/new-name"),
                cx,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify rename.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should still have one worktree");
    assert_eq!(
        worktrees[0].path,
        PathBuf::from("/worktrees/new-name"),
        "worktree should be renamed"
    );

    // Verify event was emitted.
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted for rename"
    );

    drop(subscription);
}

#[gpui::test]
async fn test_repository_rename_nonexistent_worktree_remote_error(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Try to rename a worktree that doesn't exist via the remote RPC path.
    let result = cx_b
        .update(|cx| {
            repo_b.update(cx, |repo, cx| {
                repo.rename_worktree(
                    PathBuf::from("/worktrees/nonexistent"),
                    PathBuf::from("/worktrees/new-name"),
                    cx,
                )
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "renaming a nonexistent worktree should return an error"
    );
    executor.run_until_parked();

    // Verify host state is unchanged (still no worktrees).
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert!(
                state.worktrees.is_empty(),
                "host should still have no worktrees"
            );
        })
        .unwrap();
}
