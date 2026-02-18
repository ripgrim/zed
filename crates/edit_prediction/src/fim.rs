use crate::{
    EditPredictionId, EditPredictionModelInput, cursor_excerpt, ollama,
    prediction::EditPredictionResult,
};
use anyhow::{Context as _, Result, anyhow};
use cloud_llm_client::predict_edits_v3::{RawCompletionRequest, RawCompletionResponse};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Entity, Task, http_client};
use language::{
    Anchor, Buffer, BufferSnapshot, OffsetRangeExt as _, ToOffset, ToPoint as _,
    language_settings::all_language_settings,
};
use settings::{EditPredictionPromptFormat, EditPredictionProvider};
use std::{path::Path, sync::Arc, time::Instant};
use zeta_prompt::ZetaPromptInput;

const FIM_CONTEXT_TOKENS: usize = 512;

struct FimRequestOutput {
    request_id: String,
    edits: Vec<(std::ops::Range<Anchor>, Arc<str>)>,
    snapshot: BufferSnapshot,
    response_received_at: Instant,
    inputs: ZetaPromptInput,
    buffer: Entity<Buffer>,
    buffer_snapshotted_at: Instant,
}

pub fn request_prediction(
    EditPredictionModelInput {
        buffer,
        snapshot,
        position,
        events,
        ..
    }: EditPredictionModelInput,
    prompt_format: EditPredictionPromptFormat,
    cx: &mut App,
) -> Task<Result<Option<EditPredictionResult>>> {
    let settings = &all_language_settings(None, cx).edit_predictions;

    let full_path: Arc<Path> = snapshot
        .file()
        .map(|file| file.full_path(cx))
        .unwrap_or_else(|| "untitled".into())
        .into();

    let http_client = cx.http_client();
    let cursor_point = position.to_point(&snapshot);
    let buffer_snapshotted_at = Instant::now();

    // Zeta generates more tokens than FIM models. Ideally, we'd use MAX_REWRITE_TOKENS,
    // but this might be too slow for local deployments. So we make it configurable,
    // but we also have this hardcoded multiplier for now.
    let provider = settings.provider;
    let Some(settings) = (match provider {
        EditPredictionProvider::Ollama => settings.ollama.clone(),
        EditPredictionProvider::OpenAiCompatibleApi => settings.open_ai_compatible_api.clone(),
        _ => None,
    }) else {
        return Task::ready(Err(anyhow!("Unsupported edit prediction provider")));
    };

    let result = cx.background_spawn(async move {
        // For zeta models, use the dedicated zeta1 functions which handle their own
        // range computation with the correct token limits.
        let (excerpt_range, _) = cursor_excerpt::editable_and_context_ranges_for_cursor_position(
            cursor_point,
            &snapshot,
            FIM_CONTEXT_TOKENS,
            0,
        );
        let excerpt_offset_range = excerpt_range.to_offset(&snapshot);
        let cursor_offset = cursor_point.to_offset(&snapshot);

        let inputs = ZetaPromptInput {
            events,
            related_files: Vec::new(),
            cursor_offset_in_excerpt: cursor_offset - excerpt_offset_range.start,
            editable_range_in_excerpt: cursor_offset - excerpt_offset_range.start
                ..cursor_offset - excerpt_offset_range.start,
            cursor_path: full_path.clone(),
            excerpt_start_row: Some(excerpt_range.start.row),
            cursor_excerpt: snapshot
                .text_for_range(excerpt_range)
                .collect::<String>()
                .into(),
            excerpt_ranges: None,
            preferred_model: None,
            in_open_source_repo: false,
            can_collect_data: false,
        };

        let prefix = inputs.cursor_excerpt[..inputs.cursor_offset_in_excerpt].to_string();
        let suffix = inputs.cursor_excerpt[inputs.cursor_offset_in_excerpt..].to_string();
        let prompt = format_fim_prompt(prompt_format, &prefix, &suffix);
        let stop_tokens = get_fim_stop_tokens();

        let (response_text, request_id) = match provider {
            EditPredictionProvider::Ollama => {
                let response =
                    ollama::make_request(settings, prompt, stop_tokens, http_client).await?;
                (response.response, response.created_at)
            }
            EditPredictionProvider::OpenAiCompatibleApi => {
                let request = RawCompletionRequest {
                    model: settings.model.clone(),
                    prompt,
                    max_tokens: Some(settings.max_output_tokens),
                    temperature: None,
                    stop: stop_tokens
                        .into_iter()
                        .map(std::borrow::Cow::Owned)
                        .collect(),
                    environment: None,
                };

                let request_body = serde_json::to_string(&request)?;
                let http_request = http_client::Request::builder()
                    .method(http_client::Method::POST)
                    .uri(settings.api_url.as_ref())
                    .header("Content-Type", "application/json")
                    .body(http_client::AsyncBody::from(request_body))?;

                let mut response = http_client.send(http_request).await?;
                let status = response.status();

                log::debug!("fim: response status: {}", status);

                if !status.is_success() {
                    let mut body = String::new();
                    response.body_mut().read_to_string(&mut body).await?;
                    return Err(anyhow::anyhow!("fim API error: {} - {}", status, body));
                }

                let mut body = String::new();
                response.body_mut().read_to_string(&mut body).await?;

                let fim_response: RawCompletionResponse =
                    serde_json::from_str(&body).context("Failed to parse fim response")?;
                let completion = fim_response
                    .choices
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("fim response is missing completion"))?
                    .text
                    .clone();
                (completion, fim_response.id)
            }
            _ => {
                unreachable!()
            }
        };

        let response_received_at = Instant::now();

        log::debug!(
            "fim: completion received ({:.2}s)",
            (response_received_at - buffer_snapshotted_at).as_secs_f64()
        );

        let completion: Arc<str> = clean_fim_completion(&response_text).into();
        let edits = if completion.is_empty() {
            vec![]
        } else {
            let cursor_offset = cursor_point.to_offset(&snapshot);
            let anchor = snapshot.anchor_after(cursor_offset);
            vec![(anchor..anchor, completion)]
        };

        anyhow::Ok(FimRequestOutput {
            request_id,
            edits,
            snapshot,
            response_received_at,
            inputs,
            buffer,
            buffer_snapshotted_at,
        })
    });

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        let output = result.await.context("fim edit prediction failed")?;
        anyhow::Ok(Some(
            EditPredictionResult::new(
                EditPredictionId(output.request_id.into()),
                &output.buffer,
                &output.snapshot,
                output.edits.into(),
                None,
                output.buffer_snapshotted_at,
                output.response_received_at,
                output.inputs,
                cx,
            )
            .await,
        ))
    })
}

fn format_fim_prompt(
    prompt_format: EditPredictionPromptFormat,
    prefix: &str,
    suffix: &str,
) -> String {
    match prompt_format {
        EditPredictionPromptFormat::CodeLlama => {
            format!("<PRE> {prefix} <SUF>{suffix} <MID>")
        }
        EditPredictionPromptFormat::StarCoder => {
            format!("<fim_prefix>{prefix}<fim_suffix>{suffix}<fim_middle>")
        }
        EditPredictionPromptFormat::DeepseekCoder => {
            format!("<｜fim▁begin｜>{prefix}<｜fim▁hole｜>{suffix}<｜fim▁end｜>")
        }
        EditPredictionPromptFormat::Qwen | EditPredictionPromptFormat::CodeGemma => {
            format!("<|fim_prefix|>{prefix}<|fim_suffix|>{suffix}<|fim_middle|>")
        }
        EditPredictionPromptFormat::CodeStral => {
            format!("[SUFFIX]{suffix}[PREFIX]{prefix}")
        }
        EditPredictionPromptFormat::Glm => {
            format!("<|code_prefix|>{prefix}<|code_suffix|>{suffix}<|code_middle|>")
        }
        _ => {
            format!("<fim_prefix>{prefix}<fim_suffix>{suffix}<fim_middle>")
        }
    }
}

fn get_fim_stop_tokens() -> Vec<String> {
    vec![
        "<|endoftext|>".to_string(),
        "<|file_separator|>".to_string(),
        "<|fim_pad|>".to_string(),
        "<|fim_prefix|>".to_string(),
        "<|fim_middle|>".to_string(),
        "<|fim_suffix|>".to_string(),
        "<fim_prefix>".to_string(),
        "<fim_middle>".to_string(),
        "<fim_suffix>".to_string(),
        "<PRE>".to_string(),
        "<SUF>".to_string(),
        "<MID>".to_string(),
        "[PREFIX]".to_string(),
        "[SUFFIX]".to_string(),
    ]
}

fn clean_fim_completion(response: &str) -> String {
    let mut result = response.to_string();

    let end_tokens = [
        "<|endoftext|>",
        "<|file_separator|>",
        "<|fim_pad|>",
        "<|fim_prefix|>",
        "<|fim_middle|>",
        "<|fim_suffix|>",
        "<fim_prefix>",
        "<fim_middle>",
        "<fim_suffix>",
        "<PRE>",
        "<SUF>",
        "<MID>",
        "[PREFIX]",
        "[SUFFIX]",
    ];

    for token in &end_tokens {
        if let Some(pos) = result.find(token) {
            result.truncate(pos);
        }
    }

    result
}
