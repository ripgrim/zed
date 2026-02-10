use crate::PredictionProvider;
use crate::paths::WORKTREES_DIR;
use crate::qa::QaResult;
use anyhow::{Context as _, Result};
use collections::HashMap;
use edit_prediction::example_spec::ExampleSpec;
use edit_prediction::udiff::OpenedBuffers;
use gpui::Entity;
use http_client::Url;
use language::{Anchor, Buffer};
use project::Project;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use zeta_prompt::RelatedFile;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Example {
    #[serde(flatten)]
    pub spec: ExampleSpec,

    /// The full content of the file where an edit is being predicted, and the
    /// actual cursor offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_inputs: Option<ExamplePromptInputs>,

    /// The input and expected output from the edit prediction model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<ExamplePrompt>,

    /// The actual predictions from the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predictions: Vec<ExamplePrediction>,

    /// The scores, for how well the actual predictions match the expected
    /// predictions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub score: Vec<ExampleScore>,

    /// QA evaluation results for each prediction (indexed parallel to `predictions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qa: Vec<Option<QaResult>>,

    /// The application state used to process this example.
    #[serde(skip)]
    pub state: Option<ExampleState>,
}

#[derive(Clone, Debug)]
pub struct ExampleState {
    pub project: Entity<Project>,
    pub buffer: Entity<Buffer>,
    pub cursor_position: Anchor,
    pub _open_buffers: OpenedBuffers,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExamplePromptInputs {
    pub content: String,
    pub cursor_row: u32,
    pub cursor_column: u32,
    pub cursor_offset: usize,
    /// The start offset of the selection. If `None`, the selection is empty
    /// (cursor only), meaning the selection start equals `cursor_offset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_start_offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt_start_row: Option<u32>,
    pub edit_history: Vec<Arc<zeta_prompt::Event>>,
    pub related_files: Option<Vec<RelatedFile>>,
}

impl ExamplePromptInputs {
    /// Returns the selection range. For an empty selection (cursor only),
    /// returns a range where start == end == cursor_offset.
    pub fn selection_range(&self) -> std::ops::Range<usize> {
        let start = self.selection_start_offset.unwrap_or(self.cursor_offset);
        start..self.cursor_offset
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExamplePrompt {
    pub input: String,
    pub expected_output: String,
    pub rejected_output: Option<String>, // For DPO
    pub provider: PredictionProvider,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExamplePrediction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_patch: Option<String>,
    #[serde(deserialize_with = "deserialize_null_as_empty_string")]
    pub actual_output: String,
    #[serde(
        default,
        alias = "actual_cursor",
        deserialize_with = "deserialize_single_or_vec_or_null",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub actual_cursors: Vec<ActualCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub provider: PredictionProvider,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActualCursor {
    pub path: String,
    pub row: u32,
    pub column: u32,
    pub offset: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editable_region_offset: Option<usize>,
    /// The start offset of the selection (global byte offset).
    /// If `None`, the selection is empty (cursor only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_start_offset: Option<usize>,
    /// The start offset of the selection within the editable region.
    /// If `None`, the selection is empty (cursor only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_start_editable_region_offset: Option<usize>,
}

impl ActualCursor {
    /// Construct an `ActualCursor` from a selection range within the new editable region.
    ///
    /// - `path`: file path the cursor is in
    /// - `editable_region_selection`: byte offset range of the selection within the new editable region text.
    ///   For an empty selection (cursor only), use a range where start == end.
    /// - `new_editable_region`: the full new editable region text (after marker removal)
    /// - `content`: the full file content (before the edit)
    /// - `editable_region_byte_offset`: byte offset where the editable region starts in `content`
    /// - `editable_region_start_line`: 0-based line number where the editable region starts in `content`
    pub fn from_editable_region(
        path: &std::path::Path,
        editable_region_selection: std::ops::Range<usize>,
        new_editable_region: &str,
        content: &str,
        editable_region_byte_offset: usize,
        editable_region_start_line: usize,
    ) -> Self {
        let editable_region_cursor_offset = editable_region_selection.end;
        let global_offset = editable_region_byte_offset + editable_region_cursor_offset;
        let new_region_prefix = &new_editable_region[..editable_region_cursor_offset];
        let row = (editable_region_start_line + new_region_prefix.matches('\n').count()) as u32;
        let column = match new_region_prefix.rfind('\n') {
            Some(pos) => (editable_region_cursor_offset - pos - 1) as u32,
            None => {
                let content_prefix = &content[..editable_region_byte_offset];
                let content_column = match content_prefix.rfind('\n') {
                    Some(pos) => editable_region_byte_offset - pos - 1,
                    None => editable_region_byte_offset,
                };
                (content_column + editable_region_cursor_offset) as u32
            }
        };

        let is_empty_selection = editable_region_selection.start == editable_region_selection.end;
        let (selection_start_offset, selection_start_editable_region_offset) = if is_empty_selection
        {
            (None, None)
        } else {
            let start_global = editable_region_byte_offset + editable_region_selection.start;
            (Some(start_global), Some(editable_region_selection.start))
        };

        ActualCursor {
            path: path.to_string_lossy().to_string(),
            row,
            column,
            offset: global_offset,
            editable_region_offset: Some(editable_region_cursor_offset),
            selection_start_offset,
            selection_start_editable_region_offset,
        }
    }

    /// Returns the selection range within the editable region.
    /// For an empty selection (cursor only), returns a range where start == end.
    pub fn editable_region_selection(&self) -> Option<std::ops::Range<usize>> {
        let cursor_offset = self.editable_region_offset?;
        let start = self
            .selection_start_editable_region_offset
            .unwrap_or(cursor_offset);
        Some(start..cursor_offset)
    }
}

fn deserialize_null_as_empty_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

fn deserialize_single_or_vec_or_null<'de, D>(deserializer: D) -> Result<Vec<ActualCursor>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SingleOrVec {
        Vec(Vec<ActualCursor>),
        Single(ActualCursor),
    }

    let opt = Option::<SingleOrVec>::deserialize(deserializer)?;
    Ok(match opt {
        None => vec![],
        Some(SingleOrVec::Vec(v)) => v,
        Some(SingleOrVec::Single(c)) => vec![c],
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExampleScore {
    pub delta_chr_f: f32,
    pub braces_disbalance: usize,
    #[serde(default)]
    pub exact_lines_tp: usize,
    #[serde(default)]
    pub exact_lines_fp: usize,
    #[serde(default)]
    pub exact_lines_fn: usize,
    #[serde(default)]
    pub reversal_ratio: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_distance: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_exact_match: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_start_distance: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_exact_match: Option<bool>,
    pub wrong_editable_region: Option<bool>,
    #[serde(default)]
    pub has_isolated_whitespace_changes: bool,
}

impl Example {
    pub fn repo_name(&self) -> Result<RepoName<'_>> {
        // git@github.com:owner/repo.git
        if self.spec.repository_url.contains('@') {
            let (owner, repo) = self
                .spec
                .repository_url
                .split_once(':')
                .context("expected : in git url")?
                .1
                .split_once('/')
                .context("expected / in git url")?;
            Ok(RepoName {
                owner: Cow::Borrowed(owner),
                name: Cow::Borrowed(repo.trim_end_matches(".git")),
            })
        // http://github.com/owner/repo.git
        } else {
            let url = Url::parse(&self.spec.repository_url)?;
            let mut segments = url.path_segments().context("empty http url")?;
            let owner = segments
                .next()
                .context("expected owner path segment")?
                .to_string();
            let repo = segments
                .next()
                .context("expected repo path segment")?
                .trim_end_matches(".git")
                .to_string();
            assert!(segments.next().is_none());

            Ok(RepoName {
                owner: Cow::Owned(owner),
                name: Cow::Owned(repo),
            })
        }
    }
}

pub struct RepoName<'a> {
    pub owner: Cow<'a, str>,
    pub name: Cow<'a, str>,
}

impl RepoName<'_> {
    pub fn worktree_path(&self) -> PathBuf {
        WORKTREES_DIR
            .join(self.owner.as_ref())
            .join(self.name.as_ref())
    }
}

pub fn read_example_files(inputs: &[PathBuf]) -> Vec<Example> {
    let mut examples = Vec::new();

    for path in inputs {
        let is_stdin = path.as_path() == Path::new("-");
        let content = if is_stdin {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .expect("Failed to read from stdin");
            buffer
        } else {
            std::fs::read_to_string(path)
                .unwrap_or_else(|_| panic!("Failed to read path: {:?}", &path))
        };
        let filename = path.file_stem().unwrap().to_string_lossy().to_string();
        let ext = if !is_stdin {
            path.extension()
                .map(|ext| ext.to_string_lossy().to_string())
                .unwrap_or_else(|| panic!("{} should have an extension", path.display()))
        } else {
            "jsonl".to_string()
        };

        match ext.as_ref() {
            "json" => {
                let mut example =
                    serde_json::from_str::<Example>(&content).unwrap_or_else(|error| {
                        panic!("Failed to parse example file: {}\n{error}", path.display())
                    });
                if example.spec.name.is_empty() {
                    example.spec.name = filename;
                }
                examples.push(example);
            }
            "jsonl" => examples.extend(
                content
                    .lines()
                    .enumerate()
                    .map(|(line_ix, line)| {
                        let mut example =
                            serde_json::from_str::<Example>(line).unwrap_or_else(|error| {
                                panic!(
                                    "Failed to parse example on {}:{}\n{error}",
                                    path.display(),
                                    line_ix + 1
                                )
                            });
                        if example.spec.name.is_empty() {
                            example.spec.name = format!("{filename}-{line_ix}")
                        }
                        example
                    })
                    .collect::<Vec<Example>>(),
            ),
            "md" => {
                let mut example = parse_markdown_example(&content).unwrap();
                if example.spec.name.is_empty() {
                    example.spec.name = filename;
                }
                examples.push(example);
            }
            ext => {
                panic!("{} has invalid example extension `{ext}`", path.display())
            }
        }
    }

    examples
}

pub fn sort_examples_by_repo_and_rev(examples: &mut [Example]) {
    examples.sort_by(|a, b| {
        a.spec
            .repository_url
            .cmp(&b.spec.repository_url)
            .then(b.spec.revision.cmp(&a.spec.revision))
    });
}

pub fn group_examples_by_repo(examples: Vec<Example>) -> VecDeque<Vec<Example>> {
    let mut examples_by_repo: HashMap<String, Vec<Example>> = HashMap::default();
    let mut ungrouped = Vec::new();
    for example in examples {
        if example.spec.repository_url.is_empty() {
            ungrouped.push(example);
        } else {
            examples_by_repo
                .entry(example.spec.repository_url.clone())
                .or_insert_with(Vec::new)
                .push(example);
        }
    }
    let mut result: VecDeque<Vec<Example>> = examples_by_repo.into_values().collect();
    for example in ungrouped {
        result.push_back(vec![example]);
    }
    result
}

fn parse_markdown_example(input: &str) -> Result<Example> {
    let spec = ExampleSpec::from_markdown(input)?;
    Ok(Example {
        spec,
        prompt_inputs: None,
        prompt: None,
        predictions: Vec::new(),
        score: Vec::new(),
        qa: Vec::new(),
        state: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(path: &str, row: u32, column: u32, offset: usize) -> ActualCursor {
        ActualCursor {
            path: path.to_string(),
            row,
            column,
            offset,
            editable_region_offset: None,
            selection_start_offset: None,
            selection_start_editable_region_offset: None,
        }
    }

    fn cursor_json(path: &str, row: u32, column: u32, offset: usize) -> String {
        format!(r#"{{"path": "{path}", "row": {row}, "column": {column}, "offset": {offset}}}"#)
    }

    fn prediction_json(cursor_field: &str) -> String {
        format!(r#"{{"actual_output": "output", {cursor_field}"provider": "Sweep"}}"#)
    }

    #[test]
    fn test_actual_cursors_deserialization() {
        let c1 = cursor_json("src/a.rs", 1, 0, 10);
        let c2 = cursor_json("src/b.rs", 2, 5, 20);

        let cases: Vec<(&str, String, Vec<ActualCursor>)> = vec![
            (
                "legacy single object (actual_cursor)",
                prediction_json(&format!(r#""actual_cursor": {c1}, "#)),
                vec![cursor("src/a.rs", 1, 0, 10)],
            ),
            (
                "legacy null (actual_cursor: null)",
                prediction_json(r#""actual_cursor": null, "#),
                vec![],
            ),
            ("missing field", prediction_json(""), vec![]),
            (
                "new array format (actual_cursors)",
                prediction_json(&format!(r#""actual_cursors": [{c1}, {c2}], "#)),
                vec![cursor("src/a.rs", 1, 0, 10), cursor("src/b.rs", 2, 5, 20)],
            ),
            (
                "new null (actual_cursors: null)",
                prediction_json(r#""actual_cursors": null, "#),
                vec![],
            ),
            (
                "empty array",
                prediction_json(r#""actual_cursors": [], "#),
                vec![],
            ),
            (
                "legacy field name with array value",
                prediction_json(&format!(r#""actual_cursor": [{c1}], "#)),
                vec![cursor("src/a.rs", 1, 0, 10)],
            ),
        ];

        for (name, json, expected) in cases {
            let prediction: ExamplePrediction =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(prediction.actual_cursors, expected, "{name}");
        }
    }

    #[test]
    fn test_actual_cursors_roundtrip() {
        let empty_prediction = ExamplePrediction {
            actual_patch: None,
            actual_output: "output".to_string(),
            actual_cursors: vec![],
            error: None,
            provider: PredictionProvider::Sweep,
        };
        let empty_json = serde_json::to_value(&empty_prediction).unwrap();
        assert!(empty_json.get("actual_cursors").is_none());
        assert!(empty_json.get("actual_cursor").is_none());

        let cursors = vec![
            ActualCursor {
                editable_region_offset: Some(5),
                selection_start_offset: Some(8),
                selection_start_editable_region_offset: Some(3),
                ..cursor("src/a.rs", 1, 0, 10)
            },
            ActualCursor {
                editable_region_offset: Some(45),
                ..cursor("src/a.rs", 3, 4, 50)
            },
        ];

        let prediction = ExamplePrediction {
            actual_patch: Some("patch".to_string()),
            actual_output: "output".to_string(),
            actual_cursors: cursors.clone(),
            error: None,
            provider: PredictionProvider::Sweep,
        };

        let json_str = serde_json::to_string(&prediction).unwrap();
        let roundtripped: ExamplePrediction = serde_json::from_str(&json_str).unwrap();
        assert_eq!(roundtripped.actual_cursors, cursors);
    }
}
