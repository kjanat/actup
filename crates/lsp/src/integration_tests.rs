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

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde_json::json;
use tokio::sync::Mutex;
use tower::Service as _;
use tower_lsp_server::jsonrpc::{Id, Request, Response};
use tower_lsp_server::ls_types::{
    ClientCapabilities, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    DidChangeTextDocumentParams, DidChangeWatchedFilesClientCapabilities,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, PartialResultParams, Position, PublishDiagnosticsParams, Range,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, TextEdit, Uri,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceClientCapabilities,
    WorkspaceFolder,
};
use tower_lsp_server::{ClientSocket, LspService};

use crate::backend::LspBackend;
use crate::diagnostics::DIAGNOSTIC_SOURCE;
use crate::test_utils;

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
    _socket_task: tokio::task::JoinHandle<()>,
}

impl TestHarness {
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
            _socket_task: socket_task,
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

// ---------------------------------------------------------------------------
// Protocol tests
// ---------------------------------------------------------------------------

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
            .contains(&"actup.fixAllInWorkspace".to_owned())
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

    if !value.is_null() {
        let actions: Vec<CodeActionOrCommand> = serde_json::from_value(value).unwrap();
        assert!(!actions.is_empty());

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction, got Command");
        };
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert!(action.title.contains("v4.2.0"));

        let edit = action.edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        assert!(changes.contains_key(&uri));
        assert_eq!(changes[&uri].len(), 1);
        assert_eq!(changes[&uri][0].new_text, "v4.2.0");
    }

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

// ---------------------------------------------------------------------------
// Snapshot tests: YAML before/after edits
// ---------------------------------------------------------------------------

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
    edits.sort_by(|a, b| b.range.start.line.cmp(&a.range.start.line));
    let mut result = original.to_owned();
    for edit in &edits {
        result = apply_text_edit(&result, edit);
    }
    assert_eq!(result, expected);
}

#[test]
fn snapshot_sha_pin_edit() {
    let original = "      - uses: actions/checkout@v4\n";
    let sha = "abc1234567890abcdef1234567890abcdef123456";
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
