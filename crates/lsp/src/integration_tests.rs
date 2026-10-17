//! Integration tests for the LSP server.
//!
//! These tests exercise the full LSP protocol flow by constructing an
//! [`LspService`] from the real [`LspBackend`], sending JSON-RPC requests
//! through the `tower::Service` interface, and reading server→client
//! notifications from the [`ClientSocket`] stream.
//!
//! No network calls are made: the provider registry is built from config
//! during `initialize`, and if `GITHUB_TOKEN` is absent, the provider
//! gracefully degrades (returning errors that surface as Information
//! diagnostics). This is intentional — integration tests validate the
//! protocol flow, not version resolution correctness (unit tests cover that).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::sync::Mutex;
use tower::Service as _;
use tower_lsp_server::{
    ClientSocket, LspService,
    jsonrpc::{Id, Request, Response},
    ls_types::{
        ClientCapabilities, CodeActionContext, CodeActionKind, CodeActionOrCommand,
        CodeActionParams, CompletionContext, CompletionParams, CompletionTriggerKind,
        DidChangeTextDocumentParams, DidChangeWatchedFilesClientCapabilities,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentChanges,
        ExecuteCommandParams, HoverParams, InitializeParams, InitializeResult, InitializedParams,
        OneOf, PartialResultParams, Position, PublishDiagnosticsParams, Range,
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, TextEdit, Uri, VersionedTextDocumentIdentifier,
        WorkDoneProgressParams, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
        WorkspaceFolder,
    },
};

use crate::{backend::LspBackend, diagnostics::DIAGNOSTIC_SOURCE, test_utils};

fn initialize_request(id: i64, workspace_root: Option<&str>) -> Request {
    #[allow(deprecated)]
    let params = InitializeParams {
        workspace_folders: workspace_root.map(|root| {
            vec![WorkspaceFolder {
                uri: Uri::from_file_path(root).unwrap(),
                name: "test".to_owned(),
            }]
        }),
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                    dynamic_registration: Some(true),
                    ..Default::default()
                }),
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    Request::build("initialize")
        .id(id)
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn initialized_notification() -> Request {
    Request::build("initialized")
        .params(serde_json::to_value(InitializedParams {}).unwrap())
        .finish()
}

fn did_open_notification(uri: &Uri, text: &str, version: i32) -> Request {
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "yaml".to_owned(),
            version,
            text: text.to_owned(),
        },
    };
    Request::build("textDocument/didOpen")
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn code_action_request(
    id: i64,
    uri: &Uri,
    range: Range,
    diagnostics: Vec<tower_lsp_server::ls_types::Diagnostic>,
) -> Request {
    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        range,
        context: CodeActionContext {
            diagnostics,
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    Request::build("textDocument/codeAction")
        .id(id)
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn did_close_notification(uri: &Uri) -> Request {
    let params = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
    };
    Request::build("textDocument/didClose")
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn shutdown_request(id: i64) -> Request {
    Request::build("shutdown").id(id).finish()
}

fn exit_notification() -> Request {
    Request::build("exit").finish()
}

fn hover_request(id: i64, uri: &Uri, position: Position) -> Request {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    Request::build("textDocument/hover")
        .id(id)
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn completion_request(
    id: i64,
    uri: &Uri,
    position: Position,
    trigger_char: Option<&str>,
) -> Request {
    let context = trigger_char.map(|ch| CompletionContext {
        trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
        trigger_character: Some(ch.to_owned()),
    });
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context,
    };
    Request::build("textDocument/completion")
        .id(id)
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

fn execute_command_request(id: i64, command: &str, args: Vec<serde_json::Value>) -> Request {
    let params = ExecuteCommandParams {
        command: command.to_owned(),
        arguments: args,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    Request::build("workspace/executeCommand")
        .id(id)
        .params(serde_json::to_value(params).unwrap())
        .finish()
}

/// Collected server→client messages.
#[derive(Default)]
struct CollectedMessages {
    diagnostics: Vec<PublishDiagnosticsParams>,
}

/// Integration test harness.
///
/// Spawns a background task to consume the `ClientSocket` stream
/// (draining server→client messages and auto-responding to requests).
/// This prevents deadlocks from the channel filling up when the server
/// sends notifications while processing our requests.
struct TestHarness {
    service: LspService<LspBackend>,
    collected: Arc<Mutex<CollectedMessages>>,
    socket_task: tokio::task::JoinHandle<()>,
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Abort the background socket drain task to prevent it from lingering
        // after the test completes. Without this, the task runs until the
        // socket stream closes naturally, which may outlive the test runtime.
        self.socket_task.abort();
    }
}

impl TestHarness {
    // TODO: TestHarness uses the real LspBackend, so CI-provided GITHUB_TOKEN
    // can cause live network calls during didOpen → diagnostics. Refactor to
    // inject a mock provider registry or clear auth env vars before construction.
    fn new() -> Self {
        let (service, socket) = LspService::new(LspBackend::new);
        let collected = Arc::new(Mutex::new(CollectedMessages::default()));
        let collected_clone = Arc::clone(&collected);

        // Spawn a background task to drain the client socket.
        let socket_task = tokio::spawn(async move {
            drain_socket(socket, collected_clone).await;
        });

        Self {
            service,
            collected,
            socket_task,
        }
    }

    async fn request(&mut self, req: Request) -> Option<Response> {
        self.service.call(req).await.unwrap()
    }

    async fn notify(&mut self, req: Request) {
        let _ = self.service.call(req).await.unwrap();
    }

    async fn initialize(&mut self, workspace_root: Option<&str>) -> InitializeResult {
        let resp = self.request(initialize_request(1, workspace_root)).await;
        let resp = resp.expect("initialize should return a response");
        let (_, body) = resp.into_parts();
        let value = body.unwrap();
        let result: InitializeResult = serde_json::from_value(value).unwrap();

        self.notify(initialized_notification()).await;

        // Give background tasks time to process setup messages.
        tokio::time::sleep(Duration::from_millis(100)).await;

        result
    }

    /// Wait for diagnostics to appear for a given URI, with timeout.
    async fn wait_for_diagnostics(
        &self,
        uri: &Uri,
        timeout: Duration,
    ) -> Vec<PublishDiagnosticsParams> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            let found: Vec<_> = {
                let collected = self.collected.lock().await;
                collected
                    .diagnostics
                    .iter()
                    .filter(|d| d.uri == *uri)
                    .cloned()
                    .collect()
            };
            if !found.is_empty() {
                return found;
            }

            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }

        // Return whatever we have (possibly empty).
        let collected = self.collected.lock().await;
        collected
            .diagnostics
            .iter()
            .filter(|d| d.uri == *uri)
            .cloned()
            .collect()
    }

    /// Clear collected diagnostics.
    async fn clear_diagnostics(&self) {
        self.collected.lock().await.diagnostics.clear();
    }

    async fn shutdown(mut self) {
        let _ = self.request(shutdown_request(999)).await;
        self.notify(exit_notification()).await;
        // The socket task will end when the socket stream closes.
    }
}

/// Background task: drain the `ClientSocket`, auto-respond to requests,
/// and collect `publishDiagnostics` notifications.
async fn drain_socket(mut socket: ClientSocket, collected: Arc<Mutex<CollectedMessages>>) {
    while let Some(req) = socket.next().await {
        let parsed: serde_json::Value = serde_json::to_value(&req).unwrap();
        let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Auto-respond to server requests that have an id.
        if let Some(id_val) = parsed.get("id")
            && let Ok(id) = serde_json::from_value::<Id>(id_val.clone())
        {
            let resp = Response::from_ok(id, json!(null));
            if socket.send(resp).await.is_err() {
                break;
            }
        }

        // Collect publishDiagnostics notifications.
        if method == "textDocument/publishDiagnostics"
            && let Some(params) = parsed.get("params")
            && let Ok(diag_params) =
                serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
        {
            collected.lock().await.diagnostics.push(diag_params);
        }
    }
}

// Protocol tests

#[tokio::test]
async fn initialize_returns_capabilities() {
    let mut harness = TestHarness::new();
    let result = harness.initialize(None).await;

    let info = result.server_info.unwrap();
    assert_eq!(info.name, "actup-lsp");
    assert!(info.version.is_some());

    let caps = result.capabilities;
    assert!(caps.text_document_sync.is_some());
    assert!(caps.code_action_provider.is_some());
    assert!(caps.hover_provider.is_some());

    let completion = caps.completion_provider.unwrap();
    assert!(
        completion
            .trigger_characters
            .as_ref()
            .is_some_and(|tc| tc.contains(&"@".to_owned()))
    );

    let exec = caps.execute_command_provider.unwrap();
    assert!(
        exec.commands
            .contains(&"actup-lsp/fixAllInWorkspace".to_owned())
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn did_open_no_uses_publishes_empty_diagnostics() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/no-uses.yml").unwrap();
    let text = "name: CI\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;

    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;

    assert!(
        diags.iter().any(|d| d.diagnostics.is_empty()),
        "expected empty diagnostics for file without uses:"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn did_open_with_uses_publishes_diagnostics() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/with-uses.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;

    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(5))
        .await;

    assert!(
        !diags.is_empty(),
        "expected publishDiagnostics for opened document"
    );

    // All diagnostics from actup should have the correct source.
    for d in &diags {
        for diag in &d.diagnostics {
            assert_eq!(diag.source.as_deref(), Some(DIAGNOSTIC_SOURCE));
        }
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn did_close_clears_diagnostics() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/close-test.yml").unwrap();
    let text =
        "name: CI\non: push\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(3))
        .await;

    // Clear collected diagnostics before close.
    harness.clear_diagnostics().await;

    harness.notify(did_close_notification(&uri)).await;

    // Wait for the close-triggered empty diagnostics.
    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    let last = diags.last();

    assert!(
        last.is_some_and(|d| d.diagnostics.is_empty()),
        "didClose should publish empty diagnostics"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn code_action_returns_quickfix_for_outdated() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/codeaction.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v3\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Build an outdated diagnostic as if the client received it.
    let diag = test_utils::make_outdated_diagnostic(
        test_utils::make_range(6, 31, 33),
        "actions/checkout@v3",
        "v4.2.0",
    );

    let req = code_action_request(10, &uri, diag.range, vec![diag]);
    let resp = harness.request(req).await;

    let resp = resp.expect("codeAction should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    // The response must be non-null: our outdated diagnostic should produce
    // at least one quickfix code action. A null response means the diagnostic
    // data wasn't round-tripped correctly.
    assert!(
        !value.is_null(),
        "expected non-null code action response for outdated diagnostic"
    );

    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(value).unwrap();
    assert_ne!(actions, []);

    let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected CodeAction, got Command");
    };
    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    assert!(action.title.contains("v4.2.0"));

    let edit = action.edit.as_ref().unwrap();
    // Edits must use versioned documentChanges, not unversioned changes.
    assert!(edit.changes.is_none());
    let doc_changes = edit.document_changes.as_ref().unwrap();
    let DocumentChanges::Edits(doc_edits) = doc_changes else {
        panic!("expected DocumentChanges::Edits");
    };
    assert_eq!(doc_edits.len(), 1);
    assert_eq!(&doc_edits[0].text_document.uri, &uri);
    assert_eq!(doc_edits[0].text_document.version, Some(1));
    let text_edits: Vec<_> = doc_edits[0]
        .edits
        .iter()
        .map(|e| match e {
            OneOf::Left(te) => te,
            OneOf::Right(_) => panic!("expected TextEdit"),
        })
        .collect();
    assert_eq!(text_edits.len(), 1);
    assert_eq!(text_edits[0].new_text, "v4.2.0");

    harness.shutdown().await;
}

#[tokio::test]
async fn code_action_empty_context_returns_none() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/empty.yml").unwrap();
    harness
        .notify(did_open_notification(&uri, "name: CI\non: push\n", 1))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let range = test_utils::make_range(0, 0, 10);
    let req = code_action_request(20, &uri, range, vec![]);
    let resp = harness.request(req).await;

    let resp = resp.expect("codeAction should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    assert!(
        value.is_null() || value.as_array().is_some_and(Vec::is_empty),
        "expected no code actions, got: {value}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn shutdown_returns_ok() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let resp = harness.request(shutdown_request(100)).await;
    let resp = resp.expect("shutdown should return a response");
    let (_, body) = resp.into_parts();
    assert!(body.is_ok());

    harness.notify(exit_notification()).await;
}

#[tokio::test]
async fn full_lifecycle_open_change_close() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/lifecycle.yml").unwrap();

    // Open.
    let text_v1 =
        "name: CI\non: push\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v3\n";
    harness
        .notify(did_open_notification(&uri, text_v1, 1))
        .await;
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(3))
        .await;

    // Change.
    harness.clear_diagnostics().await;
    let text_v2 =
        "name: CI\non: push\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v2\n";
    let change_params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version: 2,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text_v2.to_owned(),
        }],
    };
    let change_req = Request::build("textDocument/didChange")
        .params(serde_json::to_value(change_params).unwrap())
        .finish();
    harness.notify(change_req).await;

    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(5))
        .await;
    assert!(!diags.is_empty(), "expected diagnostics after change");

    // Close.
    harness.clear_diagnostics().await;
    harness.notify(did_close_notification(&uri)).await;

    let close_diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    let last = close_diags.last();
    assert!(
        last.is_some_and(|d| d.diagnostics.is_empty()),
        "diagnostics should be cleared on close"
    );

    harness.shutdown().await;
}

// Hover protocol tests

#[tokio::test]
async fn hover_on_uses_line_returns_response() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/hover.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    // Wait for diagnostics to ensure the document is fully processed.
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(3))
        .await;

    // Hover on the `uses:` value (line 6, middle of "actions/checkout@v4").
    let position = Position {
        line: 6,
        character: 20,
    };
    let resp = harness.request(hover_request(10, &uri, position)).await;
    let resp = resp.expect("hover should return a response");
    let (_, body) = resp.into_parts();

    // The response should be Ok — either Some(Hover) or null.
    // We just validate the protocol works; the actual content depends on
    // whether the provider can resolve (no token = likely error/null).
    assert!(body.is_ok(), "hover should not return a JSON-RPC error");

    harness.shutdown().await;
}

#[tokio::test]
async fn hover_on_non_uses_line_returns_null() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/hover-null.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Hover on a non-uses line (line 0 = "name: CI").
    let position = Position {
        line: 0,
        character: 3,
    };
    let resp = harness.request(hover_request(11, &uri, position)).await;
    let resp = resp.expect("hover should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    assert!(
        value.is_null(),
        "hover on non-uses line should return null, got: {value}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn hover_without_open_document_returns_null() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    // Request hover on a document that was never opened.
    let uri = Uri::from_file_path("/test/not-opened.yml").unwrap();
    let position = Position {
        line: 0,
        character: 0,
    };
    let resp = harness.request(hover_request(12, &uri, position)).await;
    let resp = resp.expect("hover should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    assert!(
        value.is_null(),
        "hover on unopened document should return null"
    );

    harness.shutdown().await;
}

// Completion protocol tests

#[tokio::test]
async fn completion_on_uses_line_returns_response() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/completion.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger completion after "@" (line 6, after "actions/checkout@").
    let position = Position {
        line: 6,
        character: 34,
    };
    let resp = harness
        .request(completion_request(20, &uri, position, Some("@")))
        .await;
    let resp = resp.expect("completion should return a response");
    let (_, body) = resp.into_parts();

    // Protocol validation: the response should be Ok, not a JSON-RPC error.
    assert!(
        body.is_ok(),
        "completion should not return a JSON-RPC error"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn completion_on_non_uses_line_returns_null() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/completion-null.yml").unwrap();
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n";

    harness.notify(did_open_notification(&uri, text, 1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger completion on a non-uses line.
    let position = Position {
        line: 0,
        character: 5,
    };
    let resp = harness
        .request(completion_request(21, &uri, position, None))
        .await;
    let resp = resp.expect("completion should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    assert!(
        value.is_null(),
        "completion on non-uses line should return null, got: {value}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn completion_without_open_document_returns_null() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let uri = Uri::from_file_path("/test/completion-missing.yml").unwrap();
    let position = Position {
        line: 0,
        character: 0,
    };
    let resp = harness
        .request(completion_request(22, &uri, position, None))
        .await;
    let resp = resp.expect("completion should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    assert!(
        value.is_null(),
        "completion on unopened document should return null"
    );

    harness.shutdown().await;
}

// executeCommand protocol tests

#[tokio::test]
async fn execute_command_fix_all_returns_response() {
    let mut harness = TestHarness::new();

    // Use a temp dir as workspace root to avoid touching real files.
    let temp = tempfile::tempdir().unwrap();
    let workspace_root = temp.path().to_str().unwrap();

    harness.initialize(Some(workspace_root)).await;

    let resp = harness
        .request(execute_command_request(
            30,
            "actup-lsp/fixAllInWorkspace",
            vec![],
        ))
        .await;
    let resp = resp.expect("executeCommand should return a response");
    let (_, body) = resp.into_parts();

    // Protocol validation: the handler should not produce a JSON-RPC error.
    assert!(
        body.is_ok(),
        "executeCommand should not return a JSON-RPC error"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn execute_command_unknown_command_returns_null() {
    let mut harness = TestHarness::new();
    harness.initialize(None).await;

    let resp = harness
        .request(execute_command_request(
            31,
            "actup.nonExistentCommand",
            vec![],
        ))
        .await;
    let resp = resp.expect("executeCommand should return a response");
    let (_, body) = resp.into_parts();
    let value = body.unwrap();

    // Unknown commands return Ok(None) → JSON null (logged as warning).
    assert!(
        value.is_null(),
        "unknown command should return null, got: {value}"
    );

    harness.shutdown().await;
}

// Snapshot tests: YAML before/after edits

#[test]
fn snapshot_yaml_edit_preserves_formatting() {
    let original = "      - uses: actions/checkout@v3\n";
    let expected = "      - uses: actions/checkout@v4.2.0\n";

    let edit = TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 31,
            },
            end: Position {
                line: 0,
                character: 33,
            },
        },
        new_text: "v4.2.0".to_owned(),
    };
    assert_eq!(apply_text_edit(original, &edit), expected);
}

#[test]
fn snapshot_multi_edit_preserves_formatting() {
    let original = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions/setup-node@v3
      - run: npm test
";
    let expected = "\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4.2.0
      - uses: actions/setup-node@v4.0.0
      - run: npm test
";

    let mut edits = vec![
        TextEdit {
            range: Range {
                start: Position {
                    line: 6,
                    character: 31,
                },
                end: Position {
                    line: 6,
                    character: 33,
                },
            },
            new_text: "v4.2.0".to_owned(),
        },
        TextEdit {
            range: Range {
                start: Position {
                    line: 7,
                    character: 33,
                },
                end: Position {
                    line: 7,
                    character: 35,
                },
            },
            new_text: "v4.0.0".to_owned(),
        },
    ];
    // Apply in reverse to avoid offset shifts.
    edits.sort_by_key(|e| std::cmp::Reverse(e.range.start.line));
    let mut result = original.to_owned();
    for edit in &edits {
        result = apply_text_edit(&result, edit);
    }
    assert_eq!(result, expected);
}

#[test]
fn snapshot_sha_pin_edit() {
    let original = "      - uses: actions/checkout@v4\n";
    let sha = "abc1234567890abcdef1234567890abcdef12345";
    let expected = format!("      - uses: actions/checkout@{sha}\n");

    let edit = TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 31,
            },
            end: Position {
                line: 0,
                character: 33,
            },
        },
        new_text: sha.to_owned(),
    };
    assert_eq!(apply_text_edit(original, &edit), expected);
}

#[test]
fn snapshot_quoted_uses_edit() {
    let original = "      - uses: \"actions/checkout@v3\"\n";
    let expected = "      - uses: \"actions/checkout@v4.2.0\"\n";

    let edit = TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 32,
            },
            end: Position {
                line: 0,
                character: 34,
            },
        },
        new_text: "v4.2.0".to_owned(),
    };
    assert_eq!(apply_text_edit(original, &edit), expected);
}

/// Apply a single-line `TextEdit` to text (simulates LSP client behavior).
fn apply_text_edit(text: &str, edit: &TextEdit) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let i_u32 = i as u32;
        if i_u32 == edit.range.start.line && i_u32 == edit.range.end.line {
            let start = edit.range.start.character as usize;
            let end = edit.range.end.character as usize;
            result.push_str(&line[..start]);
            result.push_str(&edit.new_text);
            result.push_str(&line[end..]);
        } else {
            result.push_str(line);
        }
        if i < lines.len() - 1 {
            result.push('\n');
        }
    }
    result
}
