use action_log::ActionLog;
use agent_client_protocol::{self as acp, ToolCallUpdateFields};
use anyhow::{Result, anyhow};
use futures::FutureExt as _;
use gpui::{App, Entity, SharedString, Task, WeakEntity};
use language::Point;
use language_model::LanguageModelToolResultContent;
use project::{AgentLocation, Project, WorktreeSettings};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::sync::Arc;
use util::markdown::MarkdownCodeBlock;

use crate::{
    AgentTool, Thread, ToolCallEventStream,
    edit_agent::streaming_fuzzy_matcher::StreamingFuzzyMatcher,
};

/// Point at a specific piece of code to walk the user through it.
///
/// This tool navigates the user's editor to the specified code and highlights
/// it. The tool pauses until the user is ready to continue, creating a guided
/// walkthrough experience.
///
/// Use this tool when you want to walk the user through a codebase, explain
/// an architecture, or trace a flow across files. Call it multiple times in
/// sequence with explanatory text before each call.
///
/// The user will see the highlighted code in their editor and your explanation
/// in the chat. They press Enter or click Continue when ready to proceed.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PointToolInput {
    /// The file path in the project to point at.
    path: String,

    /// The code to highlight.
    code: String,

    /// Optional line number hint (1-indexed) for disambiguation when the
    /// code snippet matches multiple locations in the file. Also used as
    /// a fallback location if the fuzzy match fails entirely.
    start_line: Option<u32>,
}

pub struct PointTool {
    project: Entity<Project>,
    #[allow(dead_code)]
    thread: WeakEntity<Thread>,
    action_log: Entity<ActionLog>,
}

impl PointTool {
    pub fn new(
        thread: WeakEntity<Thread>,
        project: Entity<Project>,
        action_log: Entity<ActionLog>,
    ) -> Self {
        Self {
            project,
            thread,
            action_log,
        }
    }

    fn with_thread(&self, new_thread: WeakEntity<Thread>) -> Self {
        Self {
            thread: new_thread,
            project: self.project.clone(),
            action_log: self.action_log.clone(),
        }
    }
}

impl AgentTool for PointTool {
    type Input = PointToolInput;
    type Output = LanguageModelToolResultContent;

    const NAME: &'static str = "point";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input
            && let Some(project_path) = self.project.read(cx).find_project_path(&input.path, cx)
            && let Some(path) = self
                .project
                .read(cx)
                .short_full_path_for_project_path(&project_path, cx)
        {
            format!("Point at `{path}`").into()
        } else {
            "Point at code".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<LanguageModelToolResultContent>> {
        let Some(project_path) = self.project.read(cx).find_project_path(&input.path, cx) else {
            return Task::ready(Err(anyhow!("Path {} not found in project", &input.path)));
        };
        let Some(abs_path) = self.project.read(cx).absolute_path(&project_path, cx) else {
            return Task::ready(Err(anyhow!(
                "Failed to convert {} to absolute path",
                &input.path
            )));
        };

        let global_settings = WorktreeSettings::get_global(cx);
        if global_settings.is_path_excluded(&project_path.path) {
            return Task::ready(Err(anyhow!(
                "Cannot read file because its path matches the global `file_scan_exclusions` setting: {}",
                &input.path
            )));
        }
        if global_settings.is_path_private(&project_path.path) {
            return Task::ready(Err(anyhow!(
                "Cannot read file because its path matches the global `private_files` setting: {}",
                &input.path
            )));
        }

        let worktree_settings = WorktreeSettings::get(Some((&project_path).into()), cx);
        if worktree_settings.is_path_excluded(&project_path.path) {
            return Task::ready(Err(anyhow!(
                "Cannot read file because its path matches the worktree `file_scan_exclusions` setting: {}",
                &input.path
            )));
        }
        if worktree_settings.is_path_private(&project_path.path) {
            return Task::ready(Err(anyhow!(
                "Cannot read file because its path matches the worktree `private_files` setting: {}",
                &input.path
            )));
        }

        event_stream.update_fields(ToolCallUpdateFields::new().locations(vec![
            acp::ToolCallLocation::new(&abs_path)
                .line(input.start_line.map(|line| line.saturating_sub(1))),
        ]));

        let project = self.project.clone();
        let action_log = self.action_log.clone();

        cx.spawn(async move |cx| {
            let open_buffer_task = cx.update(|cx| {
                project.update(cx, |project, cx| {
                    project.open_buffer(project_path.clone(), cx)
                })
            });

            let buffer = futures::select! {
                result = open_buffer_task.fuse() => result?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    anyhow::bail!("Point tool cancelled by user");
                }
            };

            if buffer.read_with(cx, |buffer, _| {
                buffer
                    .file()
                    .as_ref()
                    .is_none_or(|file| !file.disk_state().exists())
            }) {
                anyhow::bail!("{} not found", input.path);
            }

            action_log.update(cx, |log, cx| {
                log.buffer_read(buffer.clone(), cx);
            });

            let snapshot = buffer.read_with(cx, |buf, _| buf.snapshot());

            let mut matcher = StreamingFuzzyMatcher::new(snapshot.text.clone());
            matcher.push(&input.code, input.start_line.map(|l| l.saturating_sub(1)));
            let matches = matcher.finish();
            let best_match = matcher.select_best_match();

            let (matched_range, result_text, is_fallback) = if let Some(range) = best_match {
                let text: String = snapshot
                    .text_for_range(snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end))
                    .collect();
                (Some(range), text, false)
            } else if matches.len() == 1 {
                let range = matches.into_iter().next().expect("checked len");
                let text: String = snapshot
                    .text_for_range(snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end))
                    .collect();
                (Some(range), text, false)
            } else if let Some(start_line) = input.start_line {
                let row = start_line.saturating_sub(1);
                let max_row = snapshot.max_point().row;
                if row <= max_row {
                    let start_offset = snapshot.point_to_offset(Point::new(row, 0));
                    let end_row = (row + 1).min(max_row + 1);
                    let end_offset = if end_row > max_row {
                        snapshot.len()
                    } else {
                        snapshot.point_to_offset(Point::new(end_row, 0))
                    };
                    (
                        Some(start_offset..end_offset),
                        format!(
                            "Could not find the exact code snippet. Fell back to line {}.",
                            start_line
                        ),
                        true,
                    )
                } else {
                    return Err(anyhow!(
                        "Could not find the specified code in {}. Line {} is beyond the end of the file.",
                        input.path,
                        start_line
                    ));
                }
            } else {
                return Err(anyhow!(
                    "Could not find the specified code in {}.",
                    input.path
                ));
            };

            if let Some(range) = matched_range {
                let start_anchor = snapshot.anchor_after(range.start);
                let end_anchor = snapshot.anchor_before(range.end);

                project.update(cx, |project, cx| {
                    project.set_agent_location(
                        Some(AgentLocation {
                            buffer: buffer.downgrade(),
                            position: start_anchor,
                            selection_end: Some(end_anchor),
                        }),
                        cx,
                    );
                });

                if !is_fallback {
                    let markdown = MarkdownCodeBlock {
                        tag: &input.path,
                        text: &result_text,
                    }
                    .to_string();
                    event_stream.update_fields(ToolCallUpdateFields::new().content(vec![
                        acp::ToolCallContent::Content(acp::Content::new(markdown)),
                    ]));
                }
            }

            let title = format!("Point at `{}`", input.path);
            futures::select! {
                result = event_stream.request_continue(title).fuse() => {
                    result?;
                }
                _ = event_stream.cancelled_by_user().fuse() => {
                    anyhow::bail!("Point tool cancelled by user");
                }
            }

            Ok(result_text.into())
        })
    }

    fn rebind_thread(
        &self,
        new_thread: WeakEntity<Thread>,
    ) -> Option<Arc<dyn crate::AnyAgentTool>> {
        Some(self.with_thread(new_thread).erase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextServerRegistry, Templates, Thread, ToolCallEventStream};
    use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
    use indoc::indoc;
    use language_model::fake_provider::FakeLanguageModel;
    use project::{FakeFs, Project};
    use prompt_store::ProjectContext;
    use serde_json::json;
    use settings::SettingsStore;
    use text::ToPoint as _;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    struct PointToolTestSetup {
        tool: Arc<PointTool>,
        project: Entity<Project>,
    }

    /// Sets up a point tool with an example file containing deliberately duplicated
    /// lines for testing fuzzy match, fallback, and disambiguation.
    ///
    /// Lines (1-indexed):
    ///  1: fn first() {
    ///  2:     println!("hello world");
    ///  3: }
    ///  4: (empty)
    ///  5: fn second() {
    ///  6:     println!("goodbye world");
    ///  7: }
    ///  8: (empty)
    ///  9: fn third() {
    /// 10:     println!("hello world");
    /// 11: }
    async fn setup_point_tool_with_example_file(cx: &mut TestAppContext) -> PointToolTestSetup {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "example.rs": indoc! {"
                    fn first() {
                        println!(\"hello world\");
                    }

                    fn second() {
                        println!(\"goodbye world\");
                    }

                    fn third() {
                        println!(\"hello world\");
                    }
                "}
            }),
        )
        .await;
        setup_point_tool(fs, cx).await
    }

    async fn setup_point_tool(fs: Arc<FakeFs>, cx: &mut TestAppContext) -> PointToolTestSetup {
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let context_server_registry =
            cx.new(|cx| ContextServerRegistry::new(project.read(cx).context_server_store(), cx));
        let model = Arc::new(FakeLanguageModel::default());
        let thread = cx.new(|cx| {
            Thread::new(
                project.clone(),
                cx.new(|_cx| ProjectContext::default()),
                context_server_registry,
                Templates::new(),
                Some(model),
                cx,
            )
        });
        let tool = Arc::new(PointTool::new(
            thread.downgrade(),
            project.clone(),
            action_log,
        ));
        PointToolTestSetup { tool, project }
    }

    #[gpui::test]
    async fn test_point_tool_errors(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "visible.rs": "fn main() {}\n",
                "secret.key": "private data",
                ".hidden": {
                    "data.txt": "excluded data"
                }
            }),
        )
        .await;

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_exclusions =
                        Some(vec!["**/.hidden".to_string()]);
                    settings.project.worktree.private_files =
                        Some(vec!["**/*.key".to_string()].into());
                });
            });
        });

        let setup = setup_point_tool(fs, cx).await;

        // Case 1: Nonexistent file
        let result = cx
            .update(|cx| {
                setup.tool.clone().run(
                    PointToolInput {
                        path: "root/no_such_file.txt".to_string(),
                        code: "anything".to_string(),
                        start_line: None,
                    },
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;
        assert!(result.is_err());

        // Case 2: Excluded file
        let result = cx
            .update(|cx| {
                setup.tool.clone().run(
                    PointToolInput {
                        path: "root/.hidden/data.txt".to_string(),
                        code: "anything".to_string(),
                        start_line: None,
                    },
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;
        assert!(result.is_err());

        // Case 3: Private file
        let result = cx
            .update(|cx| {
                setup.tool.clone().run(
                    PointToolInput {
                        path: "root/secret.key".to_string(),
                        code: "anything".to_string(),
                        start_line: None,
                    },
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;
        assert!(result.is_err());

        // Case 4: No match and no fallback
        let result = cx
            .update(|cx| {
                setup.tool.clone().run(
                    PointToolInput {
                        path: "root/visible.rs".to_string(),
                        code: "nonexistent snippet that does not appear anywhere".to_string(),
                        start_line: None,
                    },
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;
        assert!(result.is_err());

        // Case 5: Line beyond EOF
        let result = cx
            .update(|cx| {
                setup.tool.clone().run(
                    PointToolInput {
                        path: "root/visible.rs".to_string(),
                        code: "nonexistent snippet that does not appear anywhere".to_string(),
                        start_line: Some(9999),
                    },
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;
        assert!(result.is_err());
    }

    #[gpui::test]
    async fn test_point_tool_unique_fuzzy_match(cx: &mut TestAppContext) {
        init_test(cx);
        let setup = setup_point_tool_with_example_file(cx).await;

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            setup.tool.clone().run(
                PointToolInput {
                    path: "root/example.rs".to_string(),
                    code: "println!(\"goodbye world\")".to_string(),
                    start_line: None,
                },
                event_stream,
                cx,
            )
        });

        cx.run_until_parked();

        // First event: locations update
        let fields = receiver.expect_update_fields().await;
        assert!(
            fields.locations.is_some(),
            "Expected locations in first update_fields"
        );

        // Second event: content update with markdown code block
        let fields = receiver.expect_update_fields().await;
        let content = fields.content.expect("Expected content in update_fields");
        assert!(!content.is_empty(), "Content should not be empty");
        let content_str = format!("{:?}", content);
        assert!(
            content_str.contains("goodbye world"),
            "Content should contain the matched code, got: {content_str}"
        );

        // Third event: continuation request
        let continuation = receiver.expect_continuation().await;
        continuation.response.send(()).ok();

        // Await the task
        let result = task.await;
        assert!(result.is_ok(), "Expected Ok result, got: {:?}", result);
        let result_content = result.unwrap();
        let result_text = result_content
            .to_str()
            .expect("Expected text result content");
        assert!(
            result_text.contains("goodbye world"),
            "Result should contain matched text"
        );

        // Verify AgentLocation was set
        let agent_location = setup
            .project
            .read_with(cx, |project, _| project.agent_location());
        assert!(
            agent_location.is_some(),
            "AgentLocation should be set after successful point"
        );
        let location = agent_location.unwrap();
        assert!(
            location.selection_end.is_some(),
            "selection_end should be set for range selection"
        );
    }

    #[gpui::test]
    async fn test_point_tool_line_fallback(cx: &mut TestAppContext) {
        init_test(cx);
        let setup = setup_point_tool_with_example_file(cx).await;

        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            setup.tool.clone().run(
                PointToolInput {
                    path: "root/example.rs".to_string(),
                    code: "this code does not exist anywhere in the file at all".to_string(),
                    start_line: Some(5),
                },
                event_stream,
                cx,
            )
        });

        cx.run_until_parked();

        // First event: locations update
        let fields = receiver.expect_update_fields().await;
        assert!(fields.locations.is_some());

        // No content event for fallback — next event is continuation
        let continuation = receiver.expect_continuation().await;
        continuation.response.send(()).ok();

        let result = task.await;
        assert!(result.is_ok(), "Expected Ok result, got: {:?}", result);
        let result_content = result.unwrap();
        let result_text = result_content
            .to_str()
            .expect("Expected text result content");
        assert!(
            result_text.contains("Fell back to line 5"),
            "Result should mention fallback, got: {result_text}"
        );

        // Verify AgentLocation points at line 5 (0-indexed row 4)
        let agent_location = setup
            .project
            .read_with(cx, |project, _| project.agent_location());
        assert!(agent_location.is_some());
        let location = agent_location.unwrap();
        let buffer = location.buffer.upgrade().expect("buffer should be alive");
        let position_point =
            buffer.read_with(cx, |buf, _| location.position.to_point(&buf.snapshot()));
        assert_eq!(
            position_point.row, 4,
            "Fallback should point at row 4 (line 5, 0-indexed)"
        );
    }

    #[gpui::test]
    async fn test_point_tool_line_hint_disambiguation(cx: &mut TestAppContext) {
        init_test(cx);
        let setup = setup_point_tool_with_example_file(cx).await;

        // "println!("hello world")" appears on lines 2 and 10.
        // With start_line=10, the matcher should pick the second occurrence.
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            setup.tool.clone().run(
                PointToolInput {
                    path: "root/example.rs".to_string(),
                    code: "println!(\"hello world\")".to_string(),
                    start_line: Some(10),
                },
                event_stream,
                cx,
            )
        });

        cx.run_until_parked();

        // First event: locations update
        let fields = receiver.expect_update_fields().await;
        assert!(fields.locations.is_some());

        // Second event: content update (fuzzy match succeeds)
        let fields = receiver.expect_update_fields().await;
        assert!(
            fields.content.is_some(),
            "Expected content for disambiguation case"
        );

        // Continuation
        let continuation = receiver.expect_continuation().await;
        continuation.response.send(()).ok();

        let result = task.await;
        assert!(result.is_ok(), "Expected Ok result, got: {:?}", result);

        // Verify AgentLocation points near line 10 (0-indexed row 9)
        let agent_location = setup
            .project
            .read_with(cx, |project, _| project.agent_location());
        assert!(agent_location.is_some());
        let location = agent_location.unwrap();
        let buffer = location.buffer.upgrade().expect("buffer should be alive");
        let position_point =
            buffer.read_with(cx, |buf, _| location.position.to_point(&buf.snapshot()));
        assert_eq!(
            position_point.row, 9,
            "Disambiguation with start_line=10 should pick the second occurrence at row 9"
        );
    }

    #[gpui::test]
    async fn test_point_tool_cancellation(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "file.rs": "fn main() {\n    println!(\"test\");\n}\n"
            }),
        )
        .await;

        let setup = setup_point_tool(fs, cx).await;

        let (event_stream, _receiver, mut cancellation_tx) =
            ToolCallEventStream::test_with_cancellation();

        let task = cx.update(|cx| {
            setup.tool.clone().run(
                PointToolInput {
                    path: "root/file.rs".to_string(),
                    code: "println!(\"test\")".to_string(),
                    start_line: None,
                },
                event_stream,
                cx,
            )
        });

        cx.run_until_parked();

        // Signal cancellation while the tool is waiting for continuation
        ToolCallEventStream::signal_cancellation_with_sender(&mut cancellation_tx);

        let result = task.await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cancelled by user"),
            "Expected cancellation error"
        );
    }
}
