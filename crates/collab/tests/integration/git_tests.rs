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

    // Track WorktreesChanged events on the remote repo.
    let remote_events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = cx_b.update(|cx| {
        let remote_events = remote_events.clone();
        cx.subscribe(&repo_b, move |_, event: &RepositoryEvent, _| {
            remote_events.lock().push(event.clone());
        })
    });

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

    // Verify WorktreesChanged event was emitted on the remote client.
    assert!(
        remote_events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted on remote client after remove"
    );

    // Verify the directory was removed from the filesystem.
    assert!(
        !client_a
            .fs()
            .is_dir(Path::new("/worktrees/test-branch"))
            .await,
        "worktree directory should be removed from filesystem"
    );

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

    // Track WorktreesChanged events on the remote repo.
    let remote_events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = cx_b.update(|cx| {
        let remote_events = remote_events.clone();
        cx.subscribe(&repo_b, move |_, event: &RepositoryEvent, _| {
            remote_events.lock().push(event.clone());
        })
    });

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

    // Verify WorktreesChanged event was emitted on the remote client.
    assert!(
        remote_events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted on remote client after rename"
    );

    // Verify the filesystem reflects the rename.
    assert!(
        !client_a
            .fs()
            .is_dir(Path::new("/worktrees/old-branch"))
            .await,
        "old worktree directory should no longer exist"
    );
    assert!(
        client_a
            .fs()
            .is_dir(Path::new("/worktrees/new-branch"))
            .await,
        "new worktree directory should exist"
    );

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
    let _subscription = cx_a.update(|cx| {
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

    // Verify the directory was removed from the filesystem.
    assert!(
        !client.fs().is_dir(Path::new("/worktrees/remove-me")).await,
        "worktree directory should be removed from filesystem"
    );

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

    // Verify the filesystem reflects the rename.
    assert!(
        !client.fs().is_dir(Path::new("/worktrees/old-name")).await,
        "old worktree directory should no longer exist"
    );
    assert!(
        client.fs().is_dir(Path::new("/worktrees/new-name")).await,
        "new worktree directory should exist"
    );

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

    drop(_subscription);
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

#[gpui::test]
async fn test_repository_remove_dirty_worktree(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Pre-populate a dirty worktree on the host.
    client_a
        .fs()
        .create_dir(Path::new("/worktrees/dirty-branch"))
        .await
        .unwrap();
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/dirty-branch"),
                ref_name: "refs/heads/dirty-branch".into(),
                sha: "abc123".into(),
            });
            state
                .dirty_worktrees
                .insert(PathBuf::from("/worktrees/dirty-branch"));
        })
        .unwrap();

    // Verify the worktree exists.
    let worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should have one worktree");

    // Try to remove without force — should fail because it's dirty.
    let result = cx_b
        .update(|cx| {
            repo_b.update(cx, |repo, cx| {
                repo.remove_worktree(PathBuf::from("/worktrees/dirty-branch"), false, cx)
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "removing a dirty worktree without force should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("modified or untracked"),
        "error should mention modified or untracked files, got: {err_msg}"
    );
    executor.run_until_parked();

    // Verify the worktree is still there.
    let worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worktrees.len(),
        1,
        "worktree should still exist after failed removal"
    );

    // Now remove with force — should succeed.
    cx_b.update(|cx| {
        repo_b.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/dirty-branch"), true, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify the directory was removed from the filesystem.
    assert!(
        !client_a
            .fs()
            .is_dir(Path::new("/worktrees/dirty-branch"))
            .await,
        "worktree directory should be removed from filesystem after force removal"
    );

    // Verify the worktree was removed.
    let worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        worktrees.is_empty(),
        "worktree should be removed after force removal"
    );

    // Verify host state is also clean.
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert!(
                state.worktrees.is_empty(),
                "host should have no worktrees after force removal"
            );
            assert!(
                state.dirty_worktrees.is_empty(),
                "host should have no dirty worktrees after force removal"
            );
        })
        .unwrap();
}

#[gpui::test]
async fn test_repository_create_worktree_emits_event(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
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

    // Track WorktreesChanged events.
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = cx_a.update(|cx| {
        let events = events.clone();
        cx.subscribe(&repo, move |_, event: &RepositoryEvent, _| {
            events.lock().push(event.clone());
        })
    });

    // Create a worktree via the local code path.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.create_worktree(
                "new-feature".to_string(),
                PathBuf::from("/worktrees"),
                None,
                cx,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify WorktreesChanged event was emitted.
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted after create_worktree"
    );

    // Verify the worktree was actually created.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should have one worktree after create");
    assert_eq!(
        worktrees[0].path,
        PathBuf::from("/worktrees/new-feature"),
        "worktree should be at expected path"
    );
}

#[gpui::test]
async fn test_repository_remove_dirty_worktree_local(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
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

    // Set up a dirty worktree.
    client
        .fs()
        .create_dir(Path::new("/worktrees/dirty-local"))
        .await
        .unwrap();
    client
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/dirty-local"),
                ref_name: "refs/heads/dirty-local".into(),
                sha: "abc123".into(),
            });
            state
                .dirty_worktrees
                .insert(PathBuf::from("/worktrees/dirty-local"));
        })
        .unwrap();

    // Non-force removal should fail.
    let result = cx_a
        .update(|cx| {
            repo.update(cx, |repo, cx| {
                repo.remove_worktree(PathBuf::from("/worktrees/dirty-local"), false, cx)
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "removing a dirty worktree without force should fail locally"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("modified or untracked"),
        "error should mention modified or untracked files, got: {err_msg}"
    );

    // Directory should still exist.
    assert!(
        client
            .fs()
            .is_dir(Path::new("/worktrees/dirty-local"))
            .await,
        "directory should still exist after failed removal"
    );

    // Force removal should succeed.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/dirty-local"), true, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    assert!(
        !client
            .fs()
            .is_dir(Path::new("/worktrees/dirty-local"))
            .await,
        "directory should be removed after force removal"
    );
}

#[gpui::test]
async fn test_repository_rename_worktree_destination_exists(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
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

    // Set up a source worktree and a pre-existing destination directory.
    client
        .fs()
        .create_dir(Path::new("/worktrees/source"))
        .await
        .unwrap();
    client
        .fs()
        .create_dir(Path::new("/worktrees/destination"))
        .await
        .unwrap();
    client
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/source"),
                ref_name: "refs/heads/source".into(),
                sha: "abc123".into(),
            });
        })
        .unwrap();

    // Rename should fail because destination already exists.
    let result = cx_a
        .update(|cx| {
            repo.update(cx, |repo, cx| {
                repo.rename_worktree(
                    PathBuf::from("/worktrees/source"),
                    PathBuf::from("/worktrees/destination"),
                    cx,
                )
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "rename to an existing destination should fail"
    );

    // Source directory should still exist.
    assert!(
        client.fs().is_dir(Path::new("/worktrees/source")).await,
        "source directory should still exist after failed rename"
    );

    // Worktree should still be at original path.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].path, PathBuf::from("/worktrees/source"));
}

#[gpui::test]
async fn test_repository_create_then_remove_worktree(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
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

    // No worktrees initially.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(worktrees.is_empty(), "should start with no worktrees");

    // Create a worktree.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.create_worktree("feature".to_string(), PathBuf::from("/worktrees"), None, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worktrees.len(), 1, "should have one worktree after create");
    assert_eq!(worktrees[0].path, PathBuf::from("/worktrees/feature"));
    assert!(
        client.fs().is_dir(Path::new("/worktrees/feature")).await,
        "worktree directory should exist after create"
    );

    // Remove the worktree.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/feature"), false, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        worktrees.is_empty(),
        "should have no worktrees after remove"
    );
    assert!(
        !client.fs().is_dir(Path::new("/worktrees/feature")).await,
        "worktree directory should be removed"
    );
}

#[gpui::test]
async fn test_repository_create_worktree_remote_emits_event(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (_client_a, _client_b, _project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Track WorktreesChanged events on the remote repo.
    let remote_events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = cx_b.update(|cx| {
        let remote_events = remote_events.clone();
        cx.subscribe(&repo_b, move |_, event: &RepositoryEvent, _| {
            remote_events.lock().push(event.clone());
        })
    });

    // Create a worktree via the remote RPC path.
    cx_b.update(|cx| {
        repo_b.update(cx, |repo, cx| {
            repo.create_worktree(
                "remote-feature".to_string(),
                PathBuf::from("/worktrees"),
                None,
                cx,
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // Verify WorktreesChanged event was emitted on the remote client.
    assert!(
        remote_events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "WorktreesChanged event should have been emitted on remote client after create"
    );

    // Verify the worktree was created.
    let remote_worktrees = cx_b
        .update(|cx| repo_b.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        remote_worktrees.len(),
        1,
        "remote client should see one worktree after create"
    );
    assert_eq!(
        remote_worktrees[0].path,
        PathBuf::from("/worktrees/remote-feature"),
        "remote client should see the created worktree at the expected path"
    );
}

#[gpui::test]
async fn test_repository_create_worktree_duplicate_branch(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
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

    // No worktrees initially.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert!(worktrees.is_empty(), "should start with no worktrees");

    // Create a worktree for branch "feature" — should succeed.
    cx_a.update(|cx| {
        repo.update(cx, |repo, cx| {
            repo.create_worktree("feature".to_string(), PathBuf::from("/worktrees"), None, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worktrees.len(),
        1,
        "should have one worktree after first create"
    );

    // Attempt to create a second worktree with the same branch name — should fail.
    let result = cx_a
        .update(|cx| {
            repo.update(cx, |repo, cx| {
                repo.create_worktree(
                    "feature".to_string(),
                    PathBuf::from("/other-worktrees"),
                    None,
                    cx,
                )
            })
        })
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "creating a worktree with a duplicate branch name should fail"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("already exists"),
        "error should mention that the branch already exists, got: {err_msg}"
    );
    executor.run_until_parked();

    // Confirm there is still only one worktree.
    let worktrees = cx_a
        .update(|cx| repo.update(cx, |repo, _| repo.worktrees()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worktrees.len(),
        1,
        "should still have exactly one worktree after failed duplicate create"
    );
    assert_eq!(
        worktrees[0].path,
        PathBuf::from("/worktrees/feature"),
        "original worktree should be unaffected"
    );
}

#[gpui::test]
async fn test_repository_host_sees_worktrees_changed_on_remote_op(
    executor: BackgroundExecutor,
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let mut server = TestServer::start(executor.clone()).await;
    let (client_a, _client_b, project_a, repo_b) =
        setup_remote_git_project(&executor, &mut server, cx_a, cx_b).await;

    // Get the host's (client A's) Repository entity.
    let repo_a = cx_a.update(|cx| project_a.read(cx).active_repository(cx).unwrap());

    // Pre-populate a worktree on the host.
    client_a
        .fs()
        .create_dir(Path::new("/worktrees/host-branch"))
        .await
        .unwrap();
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state.worktrees.push(git::repository::Worktree {
                path: PathBuf::from("/worktrees/host-branch"),
                ref_name: "refs/heads/host-branch".into(),
                sha: "abc123".into(),
            });
        })
        .unwrap();
    executor.run_until_parked();

    // Subscribe to the HOST's Repository events.
    let host_events = Arc::new(Mutex::new(Vec::new()));
    let _host_subscription = cx_a.update(|cx| {
        let host_events = host_events.clone();
        cx.subscribe(&repo_a, move |_, event: &RepositoryEvent, _| {
            host_events.lock().push(event.clone());
        })
    });

    // Client B removes the worktree via the remote RPC path.
    cx_b.update(|cx| {
        repo_b.update(cx, |repo, cx| {
            repo.remove_worktree(PathBuf::from("/worktrees/host-branch"), false, cx)
        })
    })
    .await
    .unwrap()
    .unwrap();
    executor.run_until_parked();

    // The HOST's Repository should have emitted WorktreesChanged.
    assert!(
        host_events
            .lock()
            .iter()
            .any(|e| matches!(e, RepositoryEvent::WorktreesChanged)),
        "host Repository should emit WorktreesChanged when a remote client removes a worktree"
    );

    // Verify the host's state was actually updated.
    client_a
        .fs()
        .with_git_state(Path::new(path!("/project/.git")), false, |state| {
            assert!(
                state.worktrees.is_empty(),
                "host should have no worktrees after remote client removed it"
            );
        })
        .unwrap();
}
