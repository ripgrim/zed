use super::*;
use crate::udiff::apply_diff_to_string;
use client::UserStore;
use clock::FakeSystemClock;
use cloud_llm_client::{
    EditPredictionRejectReason,
    predict_edits_v3::{PredictEditsV3Request, PredictEditsV3Response},
};
use futures::{
    AsyncReadExt, StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::{
    Entity, TestAppContext,
    http_client::{FakeHttpClient, Response},
};
use indoc::indoc;
use language::Point;
use lsp::LanguageServerId;
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use std::sync::Arc;
use util::path;
use uuid::Uuid;

struct PredictReceiver(
    mpsc::UnboundedReceiver<(
        PredictEditsV3Request,
        oneshot::Sender<PredictEditsV3Response>,
    )>,
);

impl PredictReceiver {
    fn assert_no_request(&mut self) {
        assert!(
            self.0.try_next().is_err(),
            "expected no prediction request, but one was pending"
        );
    }

    async fn next_request(
        &mut self,
    ) -> (
        PredictEditsV3Request,
        oneshot::Sender<PredictEditsV3Response>,
    ) {
        self.0
            .next()
            .await
            .expect("prediction request channel closed unexpectedly")
    }
}

fn init_test(cx: &mut TestAppContext) -> (Entity<EditPredictionStore>, PredictReceiver) {
    cx.update(move |cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        zlog::init_test();

        let (predict_tx, predict_rx) = mpsc::unbounded();

        let http_client = FakeHttpClient::create({
            move |req| {
                let uri = req.uri().path().to_string();
                let mut body = req.into_body();
                let predict_tx = predict_tx.clone();
                async move {
                    let resp = match uri.as_str() {
                        "/client/llm_tokens" => {
                            serde_json::to_string(&json!({"token": "test"})).unwrap()
                        }
                        "/predict_edits/v3" => {
                            let mut buf = Vec::new();
                            body.read_to_end(&mut buf).await.ok();
                            let decompressed = zstd::decode_all(&buf[..]).unwrap();
                            let req = serde_json::from_slice(&decompressed).unwrap();
                            let (res_tx, res_rx) = oneshot::channel();
                            predict_tx.unbounded_send((req, res_tx)).unwrap();
                            serde_json::to_string(&res_rx.await?).unwrap()
                        }
                        "/predict_edits/reject" => "{}".to_string(),
                        _ => panic!("Unexpected path: {}", uri),
                    };
                    Ok(Response::builder().body(resp.into()).unwrap())
                }
            }
        });

        let client = client::Client::new(Arc::new(FakeSystemClock::new()), http_client, cx);
        client.cloud_client().set_credentials(1, "test".into());
        language_model::init(client.clone(), cx);

        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let ep_store = EditPredictionStore::global(&client, &user_store, cx);

        (ep_store, PredictReceiver(predict_rx))
    })
}

fn push_diagnostics(
    project: &Entity<Project>,
    file_path: &str,
    diagnostics: Vec<lsp::Diagnostic>,
    cx: &mut TestAppContext,
) {
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            lsp_store
                .update_diagnostics(
                    LanguageServerId(0),
                    lsp::PublishDiagnosticsParams {
                        uri: lsp::Uri::from_file_path(file_path).unwrap(),
                        diagnostics,
                        version: None,
                    },
                    None,
                    language::DiagnosticSourceKind::Pushed,
                    &[],
                    cx,
                )
                .unwrap();
        });
    });
}

fn make_error(line: u32, col_start: u32, col_end: u32, message: &str) -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: lsp::Range::new(
            lsp::Position::new(line, col_start),
            lsp::Position::new(line, col_end),
        ),
        severity: Some(lsp::DiagnosticSeverity::ERROR),
        message: message.to_string(),
        ..Default::default()
    }
}

fn model_response(request: &PredictEditsV3Request, diff: &str) -> PredictEditsV3Response {
    let excerpt =
        request.input.cursor_excerpt[request.input.editable_range_in_excerpt.clone()].to_string();
    let output = apply_diff_to_string(diff, &excerpt).unwrap();
    PredictEditsV3Response {
        request_id: Uuid::new_v4().to_string(),
        output,
    }
}

/// Tests deduplication of diagnostic-triggered edit predictions.
///
/// This tells the story of a user editing code while a noisy LSP
/// repeatedly publishes diagnostics. The system should avoid redundant
/// prediction requests when nothing meaningful has changed, while still
/// reacting when diagnostics, cursor position, or active file change.
///
/// The test follows the same setup pattern as `test_current_state`:
/// open a buffer, make a buffer prediction, reject it, then exercise
/// the diagnostic prediction path.
#[gpui::test]
async fn test_diagnostic_prediction_deduplication(cx: &mut TestAppContext) {
    let (ep_store, mut predictions) = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            "main.rs": "fn main() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n",
            "lib.rs":  "fn helper() {\n    todo!()\n}\n"
        }),
    )
    .await;
    let project = Project::test(fs, vec![path!("/root").as_ref()], cx).await;

    let buffer = project
        .update(cx, |project, cx| {
            let path = project
                .find_project_path(path!("/root/main.rs"), cx)
                .unwrap();
            project.set_active_path(Some(path.clone()), cx);
            project.open_buffer(path, cx)
        })
        .await
        .unwrap();

    let snapshot = buffer.read_with(cx, |buf, _| buf.snapshot());
    let position = snapshot.anchor_before(Point::new(1, 0));

    ep_store.update(cx, |ep_store, cx| {
        ep_store.register_project(&project, cx);
        ep_store.register_buffer(&buffer, &project, cx);
    });

    async fn await_prediction(
        predictions: &mut PredictReceiver,
        cx: &mut TestAppContext,
        diff: &str,
    ) {
        let (request, response_sender) = predictions.next_request().await;
        response_sender
            .send(model_response(&request, diff))
            .unwrap();
        cx.run_until_parked();
    }

    fn reject_current(
        ep_store: &Entity<EditPredictionStore>,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) {
        ep_store.update(cx, |ep_store, cx| {
            ep_store.reject_current_prediction(EditPredictionRejectReason::Discarded, project, cx);
        });
    }

    fn push_and_expect_no_request(
        project: &Entity<Project>,
        predictions: &mut PredictReceiver,
        file_path: &str,
        diagnostics: Vec<lsp::Diagnostic>,
        cx: &mut TestAppContext,
    ) {
        push_diagnostics(project, file_path, diagnostics, cx);
        cx.run_until_parked();
        predictions.assert_no_request();
    }

    let diff_change_x = indoc! {r"
        --- a/root/main.rs
        +++ b/root/main.rs
        @@ ... @@
         fn main() {
        -    let x = 1;
        +    let x = 42;
             let y = 2;
    "};

    let diff_change_y = indoc! {r"
        --- a/root/main.rs
        +++ b/root/main.rs
        @@ ... @@
         fn main() {
             let x = 1;
        -    let y = 2;
        +    let y = x + 1;
             let z = 3;
         }
    "};

    let diff_fix_z = indoc! {r"
        --- a/root/main.rs
        +++ b/root/main.rs
        @@ ... @@
         fn main() {
             let x = 1;
             let y = 2;
        -    let z = 3;
        +    let z = x + y;
         }
    "};

    let diff_rename_main = indoc! {r"
        --- a/root/main.rs
        +++ b/root/main.rs
        @@ ... @@
        -fn main() {
        +fn main_changed() {
             let x = 1;
             let y = 2;
    "};

    // ── Bootstrap: make one buffer prediction so the system is primed ───
    // buffer-initiated prediction and rejection it so the diagnostic path starts
    ep_store.update(cx, |ep_store, cx| {
        ep_store.refresh_prediction_from_buffer(project.clone(), buffer.clone(), position, cx);
    });
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 1: First diagnostic triggers a prediction ──────────────────
    // A fresh diagnostic on the active buffer should result in a request.
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(3, 8, 9, "unused variable `z`")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_fix_z).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 2: Identical diagnostics resent → deduped by hash ───────────
    // Resending the same diagnostics should not retrigger a request, and
    // empty diagnostics should also be ignored.
    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        vec![make_error(3, 8, 9, "unused variable `z`")],
        cx,
    );
    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        Vec::new(),
        cx,
    );

    // ── Step 3: Different diagnostics → new prediction ──────────────────
    // Changing the diagnostic content should produce a new request.
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(2, 8, 9, "unused variable `y`")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_y).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 4: Buffer refresh resets the cache ─────────────────────────
    // A buffer refresh should clear the dedup state, so a previously-seen
    // diagnostic should trigger again.
    ep_store.update(cx, |ep_store, cx| {
        ep_store.refresh_prediction_from_buffer(project.clone(), buffer.clone(), position, cx);
    });
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(2, 8, 9, "unused variable `y`")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_y).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 5: Active entry change resets the cache ─────────────────────
    // Switching files should clear the dedup state, so the same diagnostic
    // can trigger again when we return to the active file.
    let buffer2 = project
        .update(cx, |project, cx| {
            let path = project
                .find_project_path(path!("/root/lib.rs"), cx)
                .unwrap();
            project.set_active_path(Some(path.clone()), cx);
            project.open_buffer(path, cx)
        })
        .await
        .unwrap();
    ep_store.update(cx, |ep_store, cx| {
        ep_store.register_buffer(&buffer2, &project, cx);
    });

    reject_current(&ep_store, &project, cx);

    project.update(cx, |project, cx| {
        let path = project
            .find_project_path(path!("/root/main.rs"), cx)
            .unwrap();
        project.set_active_path(Some(path), cx);
    });
    cx.run_until_parked();

    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(2, 8, 9, "unused variable `y`")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_y).await;

    // ── Step 6: Skipped while a current prediction is showing ───────────
    // While a prediction is active, diagnostics should be ignored and
    // should not update the cached hash.
    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        vec![make_error(1, 8, 9, "completely new error")],
        cx,
    );

    reject_current(&ep_store, &project, cx);
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(1, 8, 9, "completely new error")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        vec![make_error(1, 8, 9, "completely new error")],
        cx,
    );

    // ── Step 7: Cursor move (via buffer refresh) resets the cache ───────
    // Moving the cursor should clear dedup state, so identical diagnostics
    // now trigger a new request.
    let new_position =
        buffer.read_with(cx, |buf, _| buf.snapshot().anchor_before(Point::new(0, 0)));
    ep_store.update(cx, |ep_store, cx| {
        ep_store.prediction_at(&buffer, Some(new_position), &project, cx);
        ep_store.refresh_prediction_from_buffer(project.clone(), buffer.clone(), new_position, cx);
    });
    await_prediction(&mut predictions, cx, diff_rename_main).await;
    reject_current(&ep_store, &project, cx);

    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(1, 8, 9, "completely new error")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 8: Jump target dedup skips repeated locations ──────────────
    // Even if the hash changes, the same jump target should be skipped.
    let jump_dedup_position =
        buffer.read_with(cx, |buf, _| buf.snapshot().anchor_before(Point::new(0, 1)));
    ep_store.update(cx, |ep_store, cx| {
        ep_store.prediction_at(&buffer, Some(jump_dedup_position), &project, cx);
    });
    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        vec![make_error(1, 8, 9, "completely new error")],
        cx,
    );

    // ── Step 9: Multiple diagnostics reordered → deduplicated ───────────
    // Reordering the same diagnostics should not emit a new request.
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![
            make_error(0, 0, 1, "first error"),
            make_error(2, 8, 9, "second error"),
        ],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    push_and_expect_no_request(
        &project,
        &mut predictions,
        path!("/root/main.rs"),
        vec![
            make_error(2, 8, 9, "second error"),
            make_error(0, 0, 1, "first error"),
        ],
        cx,
    );

    project.update(cx, |project, cx| {
        let path = project
            .find_project_path(path!("/root/lib.rs"), cx)
            .unwrap();
        project.set_active_path(Some(path), cx);
    });
    project.update(cx, |project, cx| {
        let path = project
            .find_project_path(path!("/root/main.rs"), cx)
            .unwrap();
        project.set_active_path(Some(path), cx);
    });
    cx.run_until_parked();

    // ── Step 10: Same message, different range → new prediction ─────────
    // Changing only the range should still produce a new request.
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(0, 0, 1, "range shift")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(1, 0, 1, "range shift")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    // ── Step 11: Same message/range, different severity → new prediction ─
    // Changing severity should produce a new request.
    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![make_error(2, 8, 9, "severity shift")],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
    reject_current(&ep_store, &project, cx);

    project.update(cx, |project, cx| {
        let path = project
            .find_project_path(path!("/root/lib.rs"), cx)
            .unwrap();
        project.set_active_path(Some(path), cx);
    });
    project.update(cx, |project, cx| {
        let path = project
            .find_project_path(path!("/root/main.rs"), cx)
            .unwrap();
        project.set_active_path(Some(path), cx);
    });
    cx.run_until_parked();

    let warning_diagnostic = lsp::Diagnostic {
        range: lsp::Range::new(lsp::Position::new(2, 8), lsp::Position::new(2, 9)),
        severity: Some(lsp::DiagnosticSeverity::WARNING),
        message: "severity shift".to_string(),
        ..Default::default()
    };

    push_diagnostics(
        &project,
        path!("/root/main.rs"),
        vec![warning_diagnostic],
        cx,
    );
    await_prediction(&mut predictions, cx, diff_change_x).await;
}
