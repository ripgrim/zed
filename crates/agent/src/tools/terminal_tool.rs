use agent_client_protocol as acp;
use anyhow::Result;
use futures::FutureExt as _;
use gpui::{App, AppContext, Entity, SharedString, Task};
use language_model::LanguageModelToolSchemaFormat;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use util::markdown::MarkdownInlineCode;

use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream};

const COMMAND_OUTPUT_LIMIT: u64 = 16 * 1024;

/// Executes a shell command or interacts with a running terminal process.
///
/// This tool can:
/// 1. Run a new command in a terminal (RunCmd)
/// 2. Send input to an already-running process (SendInput)
/// 3. Wait for a running process and check its status (Wait)
///
/// When a command times out or you use Wait, the process is NOT killed. Instead, you get
/// the current terminal output and can decide what to do next:
/// - Send input to interact with the process (e.g., "q" to quit less, Ctrl+C to interrupt)
/// - Use Wait to check on it again later
/// - Make a different tool call or respond with text (this will automatically kill the terminal)
///
/// Make sure you use the `cd` parameter to navigate to one of the root directories of the project.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TerminalToolInput {
    /// The action to perform: run a command, send input to a running process, or wait on a process.
    pub action: TerminalAction,
    /// Optional timeout in milliseconds. If the process hasn't exited by then, the tool returns
    /// with the current terminal state. The process is NOT killed - you can send more input or wait again.
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum TerminalAction {
    /// Executes a command in a terminal.
    /// For example, "git status" would run `git status`.
    /// Returns a terminal_id that can be used with SendInput or Wait.
    /// If the command doesn't exit within timeout_ms, returns the current output
    /// and the process keeps running - use SendInput to interact or Wait to check again.
    RunCmd {
        /// The one-liner command to execute.
        command: String,
        /// Working directory for the command. This must be one of the root directories of the project.
        cd: String,
    },
    /// Sends input to an already-running process.
    /// Use this to interact with interactive programs (e.g., send "q" to quit less).
    /// A newline is automatically appended to the input.
    SendInput {
        /// The ID of the terminal to send input to (from a previous RunCmd).
        terminal_id: String,
        /// The input string to send. A newline will be appended automatically.
        input: String,
    },
    /// Waits for a running process and returns its current state.
    /// Use this to check on a long-running process without sending input.
    Wait {
        /// The ID of the terminal to wait on (from a previous RunCmd).
        terminal_id: String,
        /// How long to wait (in milliseconds) before returning the current state.
        /// If the process exits before this duration, returns immediately with the exit status.
        duration_ms: u64,
    },
}

impl TerminalAction {
    /// Returns the user-facing label for this action type.
    pub fn ui_label(&self) -> &'static str {
        match self {
            TerminalAction::RunCmd { .. } => "Run Command",
            TerminalAction::SendInput { .. } => "Send Input to Process",
            TerminalAction::Wait { .. } => "Wait on Process",
        }
    }

    /// Parses the action from raw JSON input (e.g., from a tool call's raw_input field).
    /// Returns None if the JSON doesn't represent a valid TerminalToolInput.
    pub fn parse_from_json(json: &serde_json::Value) -> Option<Self> {
        serde_json::from_value::<TerminalToolInput>(json.clone())
            .ok()
            .map(|input| input.action)
    }
}

pub struct TerminalTool {
    project: Entity<Project>,
    environment: Rc<dyn ThreadEnvironment>,
}

impl TerminalTool {
    pub fn new(project: Entity<Project>, environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self {
            project,
            environment,
        }
    }
}

impl AgentTool for TerminalTool {
    type Input = TerminalToolInput;
    type Output = String;

    fn name() -> &'static str {
        "terminal"
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn input_schema(format: LanguageModelToolSchemaFormat) -> schemars::Schema {
        let schema = schemars::schema_for!(TerminalToolInput);
        eprintln!(
            "[INTERACTIVE-TERMINAL-DEBUG] Terminal tool schema (format {:?}):\n{}",
            format,
            serde_json::to_string_pretty(&schema).unwrap_or_else(|e| format!("Error: {}", e))
        );
        schema
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let text = match &input.action {
                TerminalAction::RunCmd { command, .. } => command.as_str(),
                TerminalAction::SendInput { input, .. } => input.as_str(),
                TerminalAction::Wait { terminal_id, .. } => terminal_id.as_str(),
            };
            let mut lines = text.lines();
            let first_line = lines.next().unwrap_or_default();
            let remaining_line_count = lines.count();
            match remaining_line_count {
                0 => MarkdownInlineCode(first_line).to_string().into(),
                1 => MarkdownInlineCode(&format!(
                    "{} - {} more line",
                    first_line, remaining_line_count
                ))
                .to_string()
                .into(),
                n => MarkdownInlineCode(&format!("{} - {} more lines", first_line, n))
                    .to_string()
                    .into(),
            }
        } else {
            "".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output>> {
        let timeout = input.timeout_ms.map(Duration::from_millis);

        eprintln!(
            "[INTERACTIVE-TERMINAL-DEBUG] Terminal tool run() called with action: {:?}, timeout: {:?}",
            input.action, timeout
        );

        match &input.action {
            TerminalAction::RunCmd { command, cd } => {
                let working_dir = match working_dir_from_cd(cd, &self.project, cx) {
                    Ok(dir) => dir,
                    Err(err) => return Task::ready(Err(err)),
                };
                let command = command.clone();

                let authorize =
                    event_stream.authorize(self.initial_title(Ok(input.clone()), cx), cx);
                cx.spawn(async move |cx| {
                    authorize.await?;

                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] RunCmd authorized, creating terminal for command: {}",
                        command
                    );

                    let terminal = self
                        .environment
                        .create_terminal(
                            command.clone(),
                            working_dir,
                            Some(COMMAND_OUTPUT_LIMIT),
                            cx,
                        )
                        .await?;

                    let terminal_id = terminal.id(cx)?;
                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] Terminal created with ID: {:?}",
                        terminal_id
                    );
                    event_stream.update_fields(acp::ToolCallUpdateFields::new().content(vec![
                        acp::ToolCallContent::Terminal(acp::Terminal::new(terminal_id.clone())),
                    ]));

                    let (exited, exit_status) = match timeout {
                        Some(timeout) => {
                            let wait_for_exit = terminal.wait_for_exit(cx)?;
                            let timeout_task = cx.background_spawn(async move {
                                smol::Timer::after(timeout).await;
                            });

                            futures::select! {
                                status = wait_for_exit.clone().fuse() => {
                                    eprintln!(
                                        "[INTERACTIVE-TERMINAL-DEBUG] RunCmd: process exited with status: {:?}",
                                        status
                                    );
                                    (true, status)
                                },
                                _ = timeout_task.fuse() => {
                                    eprintln!(
                                        "[INTERACTIVE-TERMINAL-DEBUG] RunCmd: timeout reached ({:?}), process still running",
                                        timeout
                                    );
                                    (false, acp::TerminalExitStatus::new())
                                }
                            }
                        }
                        None => {
                            let status = terminal.wait_for_exit(cx)?.await;
                            (true, status)
                        }
                    };

                    let output = terminal.current_output(cx)?;
                    let terminal_id_str = terminal_id.0.to_string();

                    Ok(process_run_cmd_result(
                        output,
                        &command,
                        &terminal_id_str,
                        exited,
                        exit_status,
                        timeout,
                    ))
                })
            }
            TerminalAction::SendInput { terminal_id, input } => {
                let terminal_id = acp::TerminalId::new(terminal_id.clone());
                let input = input.clone();

                let title: SharedString =
                    MarkdownInlineCode(&format!("Send input {:?} to current process", input))
                        .to_string()
                        .into();
                let authorize = event_stream.authorize(title, cx);

                cx.spawn(async move |cx| {
                    authorize.await?;

                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] SendInput authorized, looking up terminal: {:?}",
                        terminal_id
                    );

                    let terminal = self.environment.get_terminal(&terminal_id, cx)?;

                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] Sending input to terminal: {:?}",
                        input
                    );
                    terminal.send_input(&input, cx)?;
                    eprintln!("[INTERACTIVE-TERMINAL-DEBUG] Input sent successfully");

                    let timeout = timeout.unwrap_or(Duration::from_millis(1000));
                    let (exited, exit_status) = {
                        let wait_for_exit = terminal.wait_for_exit(cx)?;
                        let timeout_task = cx.background_spawn(async move {
                            smol::Timer::after(timeout).await;
                        });

                        futures::select! {
                            status = wait_for_exit.clone().fuse() => {
                                eprintln!(
                                    "[INTERACTIVE-TERMINAL-DEBUG] Terminal exited with status: {:?}",
                                    status
                                );
                                (true, status)
                            },
                            _ = timeout_task.fuse() => {
                                eprintln!(
                                    "[INTERACTIVE-TERMINAL-DEBUG] Timeout reached ({:?}), terminal still running",
                                    timeout
                                );
                                (false, acp::TerminalExitStatus::new())
                            }
                        }
                    };

                    let output = terminal.current_output(cx)?;
                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] Current output length: {}, truncated: {}, exited: {}",
                        output.output.len(),
                        output.truncated,
                        exited
                    );
                    Ok(process_send_input_result(
                        output,
                        &input,
                        exited,
                        exit_status,
                        timeout,
                    ))
                })
            }
            TerminalAction::Wait {
                terminal_id,
                duration_ms,
            } => {
                let terminal_id = acp::TerminalId::new(terminal_id.clone());
                let duration = Duration::from_millis(*duration_ms);

                let title: SharedString =
                    MarkdownInlineCode(&format!("wait {}ms: {}", duration_ms, terminal_id.0))
                        .to_string()
                        .into();
                let authorize = event_stream.authorize(title, cx);

                cx.spawn(async move |cx| {
                    authorize.await?;

                    eprintln!(
                        "[INTERACTIVE-TERMINAL-DEBUG] Wait: looking up terminal: {:?}, duration: {:?}",
                        terminal_id, duration
                    );

                    let terminal = self.environment.get_terminal(&terminal_id, cx)?;

                    let wait_duration = duration;
                    let (exited, exit_status) = {
                        let wait_for_exit = terminal.wait_for_exit(cx)?;
                        let wait_task = cx.background_spawn(async move {
                            smol::Timer::after(wait_duration).await;
                        });

                        futures::select! {
                            status = wait_for_exit.clone().fuse() => {
                                eprintln!(
                                    "[INTERACTIVE-TERMINAL-DEBUG] Wait: process exited with status: {:?}",
                                    status
                                );
                                (true, status)
                            },
                            _ = wait_task.fuse() => {
                                eprintln!(
                                    "[INTERACTIVE-TERMINAL-DEBUG] Wait: duration reached ({:?}), process still running",
                                    wait_duration
                                );
                                (false, acp::TerminalExitStatus::new())
                            }
                        }
                    };

                    let output = terminal.current_output(cx)?;
                    let terminal_id_str = terminal_id.0.to_string();

                    Ok(process_wait_result(
                        output,
                        &terminal_id_str,
                        exited,
                        exit_status,
                        wait_duration,
                    ))
                })
            }
        }
    }
}

fn process_run_cmd_result(
    output: acp::TerminalOutputResponse,
    command: &str,
    terminal_id: &str,
    exited: bool,
    exit_status: acp::TerminalExitStatus,
    timeout: Option<Duration>,
) -> String {
    let content = output.output.trim();
    let content_block = if content.is_empty() {
        String::new()
    } else if output.truncated {
        format!(
            "Output truncated. The first {} bytes:\n\n```\n{}\n```",
            content.len(),
            content
        )
    } else {
        format!("```\n{}\n```", content)
    };

    if exited {
        match exit_status.exit_code {
            Some(0) => {
                if content_block.is_empty() {
                    "Command executed successfully.".to_string()
                } else {
                    content_block
                }
            }
            Some(code) => {
                if content_block.is_empty() {
                    format!("Command \"{}\" failed with exit code {}.", command, code)
                } else {
                    format!(
                        "Command \"{}\" failed with exit code {}.\n\n{}",
                        command, code, content_block
                    )
                }
            }
            None => {
                if content_block.is_empty() {
                    format!("Command \"{}\" was interrupted.", command)
                } else {
                    format!(
                        "Command \"{}\" was interrupted.\n\n{}",
                        command, content_block
                    )
                }
            }
        }
    } else {
        let timeout_ms = timeout.map(|t| t.as_millis()).unwrap_or(0);
        let still_running_msg = format!(
            "The command is still running after {} ms. Terminal ID: {}\n\n\
            You can:\n\
            - Use SendInput with terminal_id \"{}\" to send input (e.g., \"q\" to quit, or Ctrl+C as \"\\x03\")\n\
            - Use Wait with terminal_id \"{}\" to check on it again\n\
            - Make a different tool call or respond with text (this will kill the process)",
            timeout_ms, terminal_id, terminal_id, terminal_id
        );
        if content_block.is_empty() {
            still_running_msg
        } else {
            format!(
                "{}\n\nCurrent terminal output:\n\n{}",
                still_running_msg, content_block
            )
        }
    }
}

fn process_wait_result(
    output: acp::TerminalOutputResponse,
    terminal_id: &str,
    exited: bool,
    exit_status: acp::TerminalExitStatus,
    timeout: Duration,
) -> String {
    let content = output.output.trim();
    let content_block = if content.is_empty() {
        String::new()
    } else if output.truncated {
        format!(
            "Output truncated. The first {} bytes:\n\n```\n{}\n```",
            content.len(),
            content
        )
    } else {
        format!("```\n{}\n```", content)
    };

    if exited {
        match exit_status.exit_code {
            Some(0) => {
                if content_block.is_empty() {
                    "The process exited successfully.".to_string()
                } else {
                    format!("The process exited successfully.\n\n{}", content_block)
                }
            }
            Some(code) => {
                if content_block.is_empty() {
                    format!("The process exited with code {}.", code)
                } else {
                    format!(
                        "The process exited with code {}.\n\n{}",
                        code, content_block
                    )
                }
            }
            None => {
                if content_block.is_empty() {
                    "The process was interrupted.".to_string()
                } else {
                    format!("The process was interrupted.\n\n{}", content_block)
                }
            }
        }
    } else {
        let timeout_ms = timeout.as_millis();
        let still_running_msg = format!(
            "The process is still running after {} ms.\n\n\
            You can:\n\
            - Use SendInput with terminal_id \"{}\" to send input (e.g., \"q\" to quit, or Ctrl+C as \"\\x03\")\n\
            - Use Wait with terminal_id \"{}\" to check on it again\n\
            - Make a different tool call or respond with text (this will kill the process)",
            timeout_ms, terminal_id, terminal_id
        );
        if content_block.is_empty() {
            still_running_msg
        } else {
            format!(
                "{}\n\nCurrent terminal output:\n\n{}",
                still_running_msg, content_block
            )
        }
    }
}

fn process_send_input_result(
    output: acp::TerminalOutputResponse,
    input: &str,
    exited: bool,
    exit_status: acp::TerminalExitStatus,
    timeout: Duration,
) -> String {
    let content = output.output.trim();
    let content_block = if content.is_empty() {
        String::new()
    } else if output.truncated {
        format!(
            "Output truncated. The first {} bytes:\n\n```\n{}\n```",
            content.len(),
            content
        )
    } else {
        format!("```\n{}\n```", content)
    };

    if exited {
        match exit_status.exit_code {
            Some(0) => {
                if content_block.is_empty() {
                    format!(
                        "Input \"{}\" was sent. The process exited successfully.",
                        input
                    )
                } else {
                    format!(
                        "Input \"{}\" was sent. The process exited successfully.\n\n{}",
                        input, content_block
                    )
                }
            }
            Some(code) => {
                if content_block.is_empty() {
                    format!(
                        "Input \"{}\" was sent. The process exited with code {}.",
                        input, code
                    )
                } else {
                    format!(
                        "Input \"{}\" was sent. The process exited with code {}.\n\n{}",
                        input, code, content_block
                    )
                }
            }
            None => {
                if content_block.is_empty() {
                    format!("Input \"{}\" was sent. The process was interrupted.", input)
                } else {
                    format!(
                        "Input \"{}\" was sent. The process was interrupted.\n\n{}",
                        input, content_block
                    )
                }
            }
        }
    } else {
        let timeout_ms = timeout.as_millis();
        if content_block.is_empty() {
            format!(
                "Input \"{}\" was sent. The process has not exited after {} ms.",
                input, timeout_ms
            )
        } else {
            format!(
                "Input \"{}\" was sent. The process has not exited after {} ms. Current terminal state:\n\n{}",
                input, timeout_ms, content_block
            )
        }
    }
}

fn working_dir_from_cd(
    cd: &str,
    project: &Entity<Project>,
    cx: &mut App,
) -> Result<Option<PathBuf>> {
    let project = project.read(cx);

    if cd == "." || cd.is_empty() {
        // Accept "." or "" as meaning "the one worktree" if we only have one worktree.
        let mut worktrees = project.worktrees(cx);

        match worktrees.next() {
            Some(worktree) => {
                anyhow::ensure!(
                    worktrees.next().is_none(),
                    "'.' is ambiguous in multi-root workspaces. Please specify a root directory explicitly.",
                );
                Ok(Some(worktree.read(cx).abs_path().to_path_buf()))
            }
            None => Ok(None),
        }
    } else {
        let input_path = Path::new(cd);

        if input_path.is_absolute() {
            // Absolute paths are allowed, but only if they're in one of the project's worktrees.
            if project
                .worktrees(cx)
                .any(|worktree| input_path.starts_with(&worktree.read(cx).abs_path()))
            {
                return Ok(Some(input_path.into()));
            }
        } else if let Some(worktree) = project.worktree_for_root_name(cd, cx) {
            return Ok(Some(worktree.read(cx).abs_path().to_path_buf()));
        }

        anyhow::bail!("`cd` directory {cd:?} was not in any of the project's worktrees.");
    }
}
