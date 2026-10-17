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
        DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DocumentChanges, ExecuteCommandParams, FileChangeType,
        FileEvent, HoverParams, InitializeParams, InitializeResult, InitializedParams, MessageType,
        OneOf, PartialResultParams, Position, PublishDiagnosticsParams, Range, ShowMessageParams,
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        TextDocumentPositionParams, TextEdit, Uri, VersionedTextDocumentIdentifier,
        WorkDoneProgressParams, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
        WorkspaceFolder, WorkspaceFoldersChangeEvent,
    },
};

use crate::{backend::LspBackend, diagnostics::DIAGNOSTIC_SOURCE, test_utils};

fn initialize_request(
    id: i64,
    workspace_root: Option<&str>,
    options: Option<serde_json::Value>,
) -> Request {
    #[allow(deprecated)]
    let params = InitializeParams {
        initialization_options: options,
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
                    relative_pattern_support: Some(true),
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

fn did_change_watched_files_notification(uri: &Uri) -> Request {
    let params = DidChangeWatchedFilesParams {
        changes: vec![FileEvent {
            uri: uri.clone(),
            typ: FileChangeType::CHANGED,
        }],
    };
    Request::build("workspace/didChangeWatchedFiles")
        .params(json!(params))
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
    messages: Vec<ShowMessageParams>,
    requests: Vec<(String, serde_json::Value)>,
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
        Self::with_user_config(None)
    }

    fn with_user_config(user_config: Option<std::path::PathBuf>) -> Self {
        let (service, socket) =
            LspService::new(move |client| LspBackend::with_user_config(client, user_config));
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
        self.initialize_with(workspace_root, None).await
    }

    async fn initialize_with(
        &mut self,
        workspace_root: Option<&str>,
        options: Option<serde_json::Value>,
    ) -> InitializeResult {
        let resp = self
            .request(initialize_request(1, workspace_root, options))
            .await;
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

    async fn wait_until(
        &self,
        timeout: Duration,
        done: impl Fn(&CollectedMessages) -> bool + Send + Sync,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if done(&*self.collected.lock().await) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        done(&*self.collected.lock().await)
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
            let params = parsed.get("params").cloned().unwrap_or_default();
            collected
                .lock()
                .await
                .requests
                .push((method.to_owned(), params));
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

        if method == "window/showMessage"
            && let Some(params) = parsed.get("params")
            && let Ok(message) = serde_json::from_value::<ShowMessageParams>(params.clone())
        {
            collected.lock().await.messages.push(message);
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

#[tokio::test]
async fn invalid_config_pauses_diagnostics_until_reload_succeeds()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[policy]\nbump = \"sideways\"\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await;

    let config_path = config_file.display().to_string();
    let shown = harness
        .wait_until(Duration::from_secs(2), |c| {
            c.messages.iter().any(|m| {
                m.typ == MessageType::ERROR
                    && m.message.contains(&config_path)
                    && m.message.contains("sideways")
            })
        })
        .await;
    assert!(
        shown,
        "expected an error showMessage naming the config file"
    );

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: gitlab.com/owner/repo@v1\n";
    harness.notify(did_open_notification(&uri, text, 1)).await;

    let paused = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    assert_ne!(
        paused,
        [],
        "expected an empty publish for the open document"
    );
    assert!(
        paused.iter().all(|d| d.diagnostics.is_empty()),
        "invalid config must suppress diagnostics: {paused:?}"
    );

    let position = Position {
        line: 3,
        character: 20,
    };
    let resp = harness
        .request(hover_request(40, &uri, position))
        .await
        .ok_or("hover returned no response")?;
    let (_, body) = resp.into_parts();
    assert_eq!(body?, serde_json::Value::Null);

    harness.collected.lock().await.messages.clear();
    harness
        .request(execute_command_request(
            41,
            "actup-lsp/fixAllInWorkspace",
            vec![],
        ))
        .await;
    let refused = harness
        .wait_until(Duration::from_secs(2), |c| {
            c.messages
                .iter()
                .any(|m| m.typ == MessageType::ERROR && m.message.contains("sideways"))
        })
        .await;
    assert!(refused, "fix-all must report the invalid config");

    harness.clear_diagnostics().await;
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let config_uri = Uri::from_file_path(&config_file).ok_or("bad config uri")?;
    harness
        .notify(did_change_watched_files_notification(&config_uri))
        .await;

    let resumed = harness
        .wait_until(Duration::from_secs(5), |c| {
            c.diagnostics
                .iter()
                .any(|d| d.uri == uri && !d.diagnostics.is_empty())
        })
        .await;
    assert!(resumed, "diagnostics must resume after the config parses");

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn config_turning_invalid_clears_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await;

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: gitlab.com/owner/repo@v1\n";
    harness.notify(did_open_notification(&uri, text, 1)).await;
    let loaded = harness
        .wait_until(Duration::from_secs(5), |c| {
            c.diagnostics
                .iter()
                .any(|d| d.uri == uri && !d.diagnostics.is_empty())
        })
        .await;
    assert!(loaded, "a valid config must produce diagnostics");

    harness.clear_diagnostics().await;
    std::fs::write(&config_file, "[policy]\nbump = \"sideways\"\n")?;
    let config_uri = Uri::from_file_path(&config_file).ok_or("bad config uri")?;
    harness
        .notify(did_change_watched_files_notification(&config_uri))
        .await;

    let shown = harness
        .wait_until(Duration::from_secs(2), |c| {
            c.messages
                .iter()
                .any(|m| m.typ == MessageType::ERROR && m.message.contains("sideways"))
        })
        .await;
    assert!(shown, "expected an error showMessage for the broken config");

    let cleared = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    assert_ne!(cleared, [], "expected a publish clearing old diagnostics");
    assert!(
        cleared.iter().all(|d| d.diagnostics.is_empty()),
        "invalid config must clear diagnostics: {cleared:?}"
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn config_reload_rejects_renamed_key() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await;

    std::fs::write(&config_file, "update_policy = \"patch\"\n")?;
    let config_uri = Uri::from_file_path(&config_file).ok_or("bad config uri")?;
    harness
        .notify(did_change_watched_files_notification(&config_uri))
        .await;

    let config_path = config_file.display().to_string();
    let shown = harness
        .wait_until(Duration::from_secs(2), |c| {
            c.messages.iter().any(|m| {
                m.typ == MessageType::ERROR
                    && m.message.contains(&config_path)
                    && m.message
                        .contains("`update_policy` is no longer supported; use `policy.bump`")
            })
        })
        .await;
    assert!(shown, "expected an error showMessage naming the new key");

    harness.shutdown().await;
    Ok(())
}

const UNRESOLVED_WORKFLOW: &str =
    "jobs:\n  build:\n    steps:\n      - uses: gitlab.com/owner/repo@v1\n";

impl TestHarness {
    async fn error_shown(&self, needle: &str) -> bool {
        self.wait_until(Duration::from_secs(2), |c| {
            c.messages
                .iter()
                .any(|m| m.typ == MessageType::ERROR && m.message.contains(needle))
        })
        .await
    }

    async fn diagnostics_published(&self, uri: &Uri) -> bool {
        self.wait_until(Duration::from_secs(5), |c| {
            c.diagnostics
                .iter()
                .any(|d| d.uri == *uri && !d.diagnostics.is_empty())
        })
        .await
    }

    async fn assert_paused(&self, uri: &Uri) {
        let paused = self.wait_for_diagnostics(uri, Duration::from_secs(2)).await;
        assert_ne!(
            paused,
            [],
            "expected an empty publish for the open document"
        );
        assert!(
            paused.iter().all(|d| d.diagnostics.is_empty()),
            "invalid config must suppress diagnostics: {paused:?}"
        );
    }

    async fn notify_changed(
        &mut self,
        file: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let uri = Uri::from_file_path(file).ok_or("bad config uri")?;
        self.notify(did_change_watched_files_notification(&uri))
            .await;
        Ok(())
    }
}

#[tokio::test]
async fn invalid_github_yaml_config_reloads_on_change() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join(".github"))?;
    let config_file = temp.path().join(".github/actup.yaml");
    std::fs::write(&config_file, "policy:\n  bump: sideways\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await;
    assert!(
        harness
            .error_shown(&config_file.display().to_string())
            .await,
        "expected an error showMessage naming .github/actup.yaml"
    );

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1))
        .await;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&config_file, "cache:\n  enabled: false\n")?;
    harness.notify_changed(&config_file).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after .github/actup.yaml parses"
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_user_config_reloads_on_change() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo)?;
    std::fs::write(repo.join("actup.toml"), "[cache]\nenabled = false\n")?;
    let user = temp.path().join("config.toml");
    std::fs::write(&user, "[policy\n")?;
    let root = repo.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::with_user_config(Some(user.clone()));
    harness.initialize(Some(root)).await;
    assert!(
        harness.error_shown(&user.display().to_string()).await,
        "expected an error showMessage naming the user config"
    );

    let uri = Uri::from_file_path(repo.join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1))
        .await;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&user, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&user).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after the user config parses"
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_config_path_beside_valid_repo_config_is_invalid()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo)?;
    std::fs::write(repo.join("actup.toml"), "[cache]\nenabled = false\n")?;
    let explicit = temp.path().join("custom.toml");
    std::fs::write(&explicit, "[policy]\nbump = \"sideways\"\n")?;
    let root = repo.to_str().ok_or("temp path is not UTF-8")?;
    let explicit_str = explicit.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness
        .initialize_with(Some(root), Some(json!({ "configPath": explicit_str })))
        .await;
    assert!(
        harness.error_shown(explicit_str).await,
        "expected an error showMessage naming configPath"
    );

    let uri = Uri::from_file_path(repo.join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1))
        .await;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&explicit, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&explicit).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after configPath parses"
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn relative_config_path_resolves_against_workspace_root()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo)?;
    let explicit = repo.join("custom.toml");
    std::fs::write(&explicit, "[policy]\nbump = \"sideways\"\n")?;
    let root = repo.to_str().ok_or("temp path is not UTF-8")?;
    let explicit_str = explicit.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness
        .initialize_with(Some(root), Some(json!({ "configPath": "custom.toml" })))
        .await;
    assert!(
        harness.error_shown(explicit_str).await,
        "expected an error showMessage naming the resolved configPath"
    );

    let repo_uri = Uri::from_file_path(&repo).ok_or("bad uri")?;
    let patterns = registered_relative_patterns(&harness.collected.lock().await.requests);
    assert!(
        patterns.contains(&(repo_uri.as_str().to_owned(), "custom.toml".to_owned())),
        "missing workspace-relative configPath watcher in {patterns:?}"
    );

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_config_path_pauses_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("actup.toml"), "[cache]\nenabled = false\n")?;
    let explicit = temp.path().join("deleted.toml");
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;
    let explicit_str = explicit.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness
        .initialize_with(Some(root), Some(json!({ "configPath": explicit_str })))
        .await;
    assert!(
        harness.error_shown(explicit_str).await,
        "expected an error showMessage naming the missing configPath"
    );

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1))
        .await;
    harness.assert_paused(&uri).await;

    harness.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unrelated_file_change_does_not_reload() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await;

    std::fs::write(&config_file, "[policy]\nbump = \"sideways\"\n")?;
    harness
        .notify_changed(&temp.path().join("nested/actup.toml"))
        .await?;
    assert!(
        !harness.error_shown("sideways").await,
        "a non-candidate file must not trigger a reload"
    );

    harness.notify_changed(&config_file).await?;
    assert!(harness.error_shown("sideways").await);

    harness.shutdown().await;
    Ok(())
}

fn registered_relative_patterns(requests: &[(String, serde_json::Value)]) -> Vec<(String, String)> {
    requests
        .iter()
        .filter(|(method, _)| method == "client/registerCapability")
        .flat_map(|(_, params)| {
            params["registrations"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|r| {
            r["registerOptions"]["watchers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|w| {
            let glob = &w["globPattern"];
            Some((
                glob["baseUri"].as_str()?.to_owned(),
                glob["pattern"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

#[tokio::test]
async fn config_watchers_follow_workspace_root() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    std::fs::create_dir(&first)?;
    std::fs::create_dir(&second)?;
    let user = temp.path().join("user/config.toml");
    let explicit = temp.path().join("custom.toml");
    let explicit_str = explicit.to_str().ok_or("temp path is not UTF-8")?;
    let root = first.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::with_user_config(Some(user.clone()));
    harness
        .initialize_with(Some(root), Some(json!({ "configPath": explicit_str })))
        .await;

    let uri = |path: &std::path::Path| {
        Uri::from_file_path(path)
            .map(|u| u.as_str().to_owned())
            .ok_or("bad uri")
    };
    let patterns = registered_relative_patterns(&harness.collected.lock().await.requests);
    for expected in [
        (
            uri(&first.join(".github"))?,
            "{actup.toml,actup.json,actup.jsonc,actup.yaml,actup.yml}",
        ),
        (uri(temp.path())?, "custom.toml"),
        (uri(&temp.path().join("user"))?, "config.toml"),
    ] {
        assert!(
            patterns.contains(&(expected.0.clone(), expected.1.to_owned())),
            "missing {expected:?} in {patterns:?}"
        );
    }

    harness.collected.lock().await.requests.clear();
    let change = DidChangeWorkspaceFoldersParams {
        event: WorkspaceFoldersChangeEvent {
            added: vec![WorkspaceFolder {
                uri: Uri::from_file_path(&second).ok_or("bad uri")?,
                name: "second".to_owned(),
            }],
            removed: vec![WorkspaceFolder {
                uri: Uri::from_file_path(&first).ok_or("bad uri")?,
                name: "first".to_owned(),
            }],
        },
    };
    harness
        .notify(
            Request::build("workspace/didChangeWorkspaceFolders")
                .params(json!(change))
                .finish(),
        )
        .await;
    let switched = harness
        .wait_until(Duration::from_secs(2), |c| {
            c.requests
                .iter()
                .any(|(method, _)| method == "client/registerCapability")
        })
        .await;
    assert!(switched, "expected a new watcher registration");

    let requests = harness.collected.lock().await.requests.clone();
    let methods: Vec<_> = requests.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        ["client/unregisterCapability", "client/registerCapability"]
    );
    let patterns = registered_relative_patterns(&requests);
    let second_github = uri(&second.join(".github"))?;
    let first = uri(&first)?;
    assert!(patterns.iter().any(|(base, _)| *base == second_github));
    assert!(!patterns.iter().any(|(base, _)| *base == first));

    harness.shutdown().await;
    Ok(())
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
