//! Integration tests for the LSP server.
//!
//! These tests exercise the full LSP protocol flow by constructing an
//! [`LspService`] from the real [`LspBackend`], sending JSON-RPC requests
//! through the `tower::Service` interface, and reading server→client
//! notifications from the [`ClientSocket`] stream.
//!
//! The backend builds its providers with [`test_utils::MOCK_PROVIDERS`]: every
//! provider is a [`test_utils::MockProvider`] and the file cache is disabled,
//! so no test reaches the network or the user's cache.

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
        WorkDoneProgressCancelParams, WorkDoneProgressParams, WorkspaceClientCapabilities,
        WorkspaceEditClientCapabilities, WorkspaceFolder, WorkspaceFoldersChangeEvent,
    },
};

use crate::{backend::LspBackend, diagnostics::DIAGNOSTIC_SOURCE, test_utils};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn initialize_request(
    id: i64,
    workspace_root: Option<&str>,
    options: Option<serde_json::Value>,
) -> TestResult<Request> {
    let workspace_uri = workspace_root
        .map(|root| Uri::from_file_path(root).ok_or("invalid workspace root"))
        .transpose()?;
    let params = InitializeParams {
        initialization_options: options,
        workspace_folders: workspace_uri.map(|uri| {
            vec![WorkspaceFolder {
                uri,
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
    Ok(Request::build("initialize")
        .id(id)
        .params(serde_json::to_value(params)?)
        .finish())
}

fn initialized_notification() -> TestResult<Request> {
    Ok(Request::build("initialized")
        .params(serde_json::to_value(InitializedParams {})?)
        .finish())
}

fn did_open_notification(uri: &Uri, text: &str, version: i32) -> TestResult<Request> {
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri: uri.clone(),
            language_id: "yaml".to_owned(),
            version,
            text: text.to_owned(),
        },
    };
    Ok(Request::build("textDocument/didOpen")
        .params(serde_json::to_value(params)?)
        .finish())
}

fn code_action_request(
    id: i64,
    uri: &Uri,
    range: Range,
    diagnostics: Vec<tower_lsp_server::ls_types::Diagnostic>,
) -> TestResult<Request> {
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
    Ok(Request::build("textDocument/codeAction")
        .id(id)
        .params(serde_json::to_value(params)?)
        .finish())
}

fn did_close_notification(uri: &Uri) -> TestResult<Request> {
    let params = DidCloseTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
    };
    Ok(Request::build("textDocument/didClose")
        .params(serde_json::to_value(params)?)
        .finish())
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

fn hover_request(id: i64, uri: &Uri, position: Position) -> TestResult<Request> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    Ok(Request::build("textDocument/hover")
        .id(id)
        .params(serde_json::to_value(params)?)
        .finish())
}

fn completion_request(
    id: i64,
    uri: &Uri,
    position: Position,
    trigger_char: Option<&str>,
) -> TestResult<Request> {
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
    Ok(Request::build("textDocument/completion")
        .id(id)
        .params(serde_json::to_value(params)?)
        .finish())
}

fn execute_command_request(
    id: i64,
    command: &str,
    args: Vec<serde_json::Value>,
) -> TestResult<Request> {
    let params = ExecuteCommandParams {
        command: command.to_owned(),
        arguments: args,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    Ok(Request::build("workspace/executeCommand")
        .id(id)
        .params(serde_json::to_value(params)?)
        .finish())
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
    fn new() -> Self {
        Self::with_user_config(None)
    }

    fn with_user_config(user_config: Option<std::path::PathBuf>) -> Self {
        let (service, socket) = crate::backend::service(move |client| {
            LspBackend::with_providers(client, user_config, test_utils::MOCK_PROVIDERS)
        });
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

    async fn request(&mut self, req: Request) -> TestResult<Option<Response>> {
        Ok(self.service.call(req).await?)
    }

    async fn notify(&mut self, req: Request) -> TestResult {
        self.service.call(req).await?;
        Ok(())
    }

    async fn initialize(&mut self, workspace_root: Option<&str>) -> TestResult<InitializeResult> {
        self.initialize_with(workspace_root, None).await
    }

    async fn initialize_with(
        &mut self,
        workspace_root: Option<&str>,
        options: Option<serde_json::Value>,
    ) -> TestResult<InitializeResult> {
        let resp = self
            .request(initialize_request(1, workspace_root, options)?)
            .await?
            .ok_or("initialize should return a response")?;
        let (_, body) = resp.into_parts();
        let result: InitializeResult = serde_json::from_value(body?)?;

        self.notify(initialized_notification()?).await?;

        // Give background tasks time to process setup messages.
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(result)
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

    async fn shutdown(mut self) -> TestResult {
        self.request(shutdown_request(999)).await?;
        self.notify(exit_notification()).await
    }
}

/// Background task: drain the `ClientSocket`, auto-respond to requests,
/// and collect `publishDiagnostics` notifications.
async fn drain_socket(mut socket: ClientSocket, collected: Arc<Mutex<CollectedMessages>>) {
    while let Some(req) = socket.next().await {
        let parsed = match serde_json::to_value(&req) {
            Ok(parsed) => parsed,
            Err(error) => panic!("server message must serialize: {error}"),
        };
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
async fn initialize_returns_capabilities() -> TestResult {
    let mut harness = TestHarness::new();
    let result = harness.initialize(None).await?;

    let info = result.server_info.ok_or("no server info")?;
    assert_eq!(info.name, "actup-lsp");
    assert!(info.version.is_some());

    let caps = result.capabilities;
    assert!(caps.text_document_sync.is_some());
    assert!(caps.code_action_provider.is_some());
    assert!(caps.hover_provider.is_some());

    let completion = caps.completion_provider.ok_or("no completion provider")?;
    assert!(
        completion
            .trigger_characters
            .as_ref()
            .is_some_and(|tc| tc.contains(&"@".to_owned()))
    );

    let exec = caps
        .execute_command_provider
        .ok_or("no executeCommand provider")?;
    assert!(
        exec.commands
            .contains(&"actup-lsp/fixAllInWorkspace".to_owned())
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn did_open_no_uses_publishes_empty_diagnostics() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/no-uses.yml").ok_or("invalid file path")?;
    let text = "name: CI\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;

    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;

    assert!(
        diags.iter().any(|d| d.diagnostics.is_empty()),
        "expected empty diagnostics for file without uses:"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn did_open_with_uses_publishes_diagnostics() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/with-uses.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;

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

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn harness_serves_versions_from_the_mock_provider() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/mocked.yml").ok_or("no uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v3\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    let published = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(5))
        .await;

    let messages: Vec<&str> = published
        .iter()
        .flat_map(|params| &params.diagnostics)
        .map(|diag| diag.message.as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("v4.2.0")
                && message.contains(&test_utils::DUMMY_SHA[..7])),
        "{messages:?}"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn did_close_clears_diagnostics() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/close-test.yml").ok_or("invalid file path")?;
    let text =
        "name: CI\non: push\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(3))
        .await;

    // Clear collected diagnostics before close.
    harness.clear_diagnostics().await;

    harness.notify(did_close_notification(&uri)?).await?;

    // Wait for the close-triggered empty diagnostics.
    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    let last = diags.last();

    assert!(
        last.is_some_and(|d| d.diagnostics.is_empty()),
        "didClose should publish empty diagnostics"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_returns_quickfix_for_outdated() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/codeaction.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v3\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Build an outdated diagnostic as if the client received it.
    let diag = test_utils::make_outdated_diagnostic(
        test_utils::make_range(6, 31, 33),
        "actions/checkout@v3",
        "v4.2.0",
    );

    let req = code_action_request(10, &uri, diag.range, vec![diag])?;
    let resp = harness.request(req).await?;

    let resp = resp.ok_or("codeAction should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    // The response must be non-null: our outdated diagnostic should produce
    // at least one quickfix code action. A null response means the diagnostic
    // data wasn't round-tripped correctly.
    assert!(
        !value.is_null(),
        "expected non-null code action response for outdated diagnostic"
    );

    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(value)?;
    assert_ne!(actions, []);

    let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected CodeAction, got Command");
    };
    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    assert!(action.title.contains("v4.2.0"));

    let edit = action.edit.as_ref().ok_or("action has no edit")?;
    // Edits must use versioned documentChanges, not unversioned changes.
    assert!(edit.changes.is_none());
    let doc_changes = edit
        .document_changes
        .as_ref()
        .ok_or("edit has no document changes")?;
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

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_returns_quickfix_in_a_markdown_file() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/README.md").ok_or("bad uri")?;
    let text = "# Usage\n\nRun it:\n\n```yaml\nsteps:\n  - uses: actions/checkout@v3\n```\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let diag = test_utils::make_outdated_diagnostic(
        test_utils::make_range(6, 27, 29),
        "actions/checkout@v3",
        "v4.2.0",
    );
    let value = harness
        .request(code_action_request(14, &uri, diag.range, vec![diag])?)
        .await?
        .ok_or("codeAction should return a response")?
        .into_parts()
        .1?;

    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(value)?;
    let Some(CodeActionOrCommand::CodeAction(action)) = actions.first() else {
        panic!("expected a code action, got {actions:?}");
    };
    assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
    let Some(DocumentChanges::Edits(doc_edits)) = action
        .edit
        .as_ref()
        .and_then(|edit| edit.document_changes.as_ref())
    else {
        panic!("expected document edits, got {:?}", action.edit);
    };
    let edits: Vec<(Range, &str)> = doc_edits
        .iter()
        .flat_map(|doc| &doc.edits)
        .filter_map(|edit| match edit {
            OneOf::Left(te) => Some((te.range, te.new_text.as_str())),
            OneOf::Right(_) => None,
        })
        .collect();
    assert_eq!(edits, [(test_utils::make_range(6, 27, 29), "v4.2.0")]);
    assert_eq!(doc_edits[0].text_document.uri, uri);

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pin_action_edits_the_ref_in_a_markdown_file() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/USAGE.md").ok_or("bad uri")?;
    let text = "# Usage\n\n```yaml\nsteps:\n  - uses: actions/checkout@v4.2.0\n```\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(5))
        .await;

    let at_ref = test_utils::make_range(4, 30, 30);
    let value = harness
        .request(code_action_request(15, &uri, at_ref, vec![])?)
        .await?
        .ok_or("codeAction should return a response")?
        .into_parts()
        .1?;

    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(value)?;
    let pin = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.kind == Some(CodeActionKind::REFACTOR) =>
            {
                Some(action)
            }
            _ => None,
        })
        .ok_or_else(|| format!("no refactor action in {actions:?}"))?;
    let Some(DocumentChanges::Edits(doc_edits)) = pin
        .edit
        .as_ref()
        .and_then(|edit| edit.document_changes.as_ref())
    else {
        panic!("expected document edits, got {:?}", pin.edit);
    };
    let edits: Vec<(Position, &str)> = doc_edits
        .iter()
        .flat_map(|doc| &doc.edits)
        .filter_map(|edit| match edit {
            OneOf::Left(te) => Some((te.range.start, te.new_text.as_str())),
            OneOf::Right(_) => None,
        })
        .collect();
    let [(start, new_text)] = edits.as_slice() else {
        panic!("expected one edit, got {edits:?}");
    };
    assert_eq!(*start, Position::new(4, 27));
    assert!(new_text.starts_with(test_utils::DUMMY_SHA), "{new_text}");

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_leaves_out_an_edit_that_breaks_the_yaml() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/corrupting.yml").ok_or("no uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v3\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let range = test_utils::make_range(3, 31, 33);
    let good = test_utils::make_outdated_diagnostic(range, "actions/checkout@v3", "v4.2.0");
    let corrupting =
        test_utils::make_outdated_diagnostic(range, "actions/checkout@v3", "v4\n      - [");
    let resp = harness
        .request(code_action_request(
            12,
            &uri,
            range,
            vec![corrupting, good],
        )?)
        .await?
        .ok_or("codeAction should return a response")?;
    let (_, body) = resp.into_parts();
    let actions: Vec<CodeActionOrCommand> = serde_json::from_value(body?)?;
    let quickfixes: Vec<&str> = actions
        .iter()
        .filter_map(|action| match action {
            CodeActionOrCommand::CodeAction(c) if c.kind == Some(CodeActionKind::QUICKFIX) => {
                Some(c.title.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(quickfixes, ["Update to v4.2.0"]);

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_offers_nothing_for_an_unopened_document() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/never-opened.yml").ok_or("no uri")?;
    let range = test_utils::make_range(3, 31, 33);
    let diag = test_utils::make_outdated_diagnostic(range, "actions/checkout@v3", "v4.2.0");
    let resp = harness
        .request(code_action_request(13, &uri, range, vec![diag])?)
        .await?
        .ok_or("codeAction should return a response")?;
    let (_, body) = resp.into_parts();
    assert_eq!(body?, serde_json::Value::Null);

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_expands_short_sha() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/shortsha.yml").ok_or("bad uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@abc1234\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut diag =
        test_utils::make_unresolvable_diagnostic(test_utils::make_range(3, 31, 38), None);
    diag.data = serde_json::to_value(crate::diagnostics::DiagnosticData::ExpandSha {
        target_ref: test_utils::DUMMY_SHA.to_string(),
        repin_comment: Some(" # v4.2.0".to_string()),
        replace_end_col: None,
    })
    .ok();

    let resp = harness
        .request(code_action_request(11, &uri, diag.range, vec![diag])?)
        .await?
        .ok_or("codeAction should return a response")?;
    let (_, body) = resp.into_parts();
    let actions: Vec<CodeActionOrCommand> =
        serde_json::from_value(body.map_err(|e| e.to_string())?)?;
    let Some(CodeActionOrCommand::CodeAction(action)) = actions
        .iter()
        .find(|a| matches!(a, CodeActionOrCommand::CodeAction(c) if c.kind == Some(CodeActionKind::QUICKFIX)))
    else {
        return Err(format!("expected a quickfix, got {actions:?}").into());
    };
    assert_eq!(action.title, "Expand to full SHA");
    let Some(DocumentChanges::Edits(doc_edits)) = action
        .edit
        .as_ref()
        .and_then(|e| e.document_changes.as_ref())
    else {
        return Err("expected document changes".into());
    };
    let [OneOf::Left(edit)] = doc_edits[0].edits.as_slice() else {
        return Err("expected one text edit".into());
    };
    assert_eq!(
        apply_text_edit(text, edit),
        format!(
            "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@{} # v4.2.0\n",
            test_utils::DUMMY_SHA
        )
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn code_action_empty_context_returns_none() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/empty.yml").ok_or("invalid file path")?;
    harness
        .notify(did_open_notification(&uri, "name: CI\non: push\n", 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let range = test_utils::make_range(0, 0, 10);
    let req = code_action_request(20, &uri, range, vec![])?;
    let resp = harness.request(req).await?;

    let resp = resp.ok_or("codeAction should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    assert!(
        value.is_null() || value.as_array().is_some_and(Vec::is_empty),
        "expected no code actions, got: {value}"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shutdown_returns_ok() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let resp = harness.request(shutdown_request(100)).await?;
    let resp = resp.ok_or("shutdown should return a response")?;
    let (_, body) = resp.into_parts();
    assert!(body.is_ok());

    harness.notify(exit_notification()).await?;
    Ok(())
}

#[tokio::test]
async fn full_lifecycle_open_change_close() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/lifecycle.yml").ok_or("invalid file path")?;

    // Open.
    let text_v1 =
        "name: CI\non: push\njobs:\n  build:\n    steps:\n      - uses: actions/checkout@v3\n";
    harness
        .notify(did_open_notification(&uri, text_v1, 1)?)
        .await?;
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
        .params(serde_json::to_value(change_params)?)
        .finish();
    harness.notify(change_req).await?;

    let diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(5))
        .await;
    assert!(!diags.is_empty(), "expected diagnostics after change");

    // Close.
    harness.clear_diagnostics().await;
    harness.notify(did_close_notification(&uri)?).await?;

    let close_diags = harness
        .wait_for_diagnostics(&uri, Duration::from_secs(2))
        .await;
    let last = close_diags.last();
    assert!(
        last.is_some_and(|d| d.diagnostics.is_empty()),
        "diagnostics should be cleared on close"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_config_pauses_diagnostics_until_reload_succeeds() -> TestResult {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[policy]\nbump = \"sideways\"\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await?;

    let config_path = actup_core::canonical_path(temp.path())
        .join("actup.toml")
        .display()
        .to_string();
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
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;

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
        .request(hover_request(40, &uri, position)?)
        .await?
        .ok_or("hover returned no response")?;
    let (_, body) = resp.into_parts();
    assert_eq!(body?, serde_json::Value::Null);

    harness.collected.lock().await.messages.clear();
    harness
        .request(execute_command_request(
            41,
            "actup-lsp/fixAllInWorkspace",
            vec![],
        )?)
        .await?;
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
        .await?;

    let resumed = harness
        .wait_until(Duration::from_secs(5), |c| {
            c.diagnostics
                .iter()
                .any(|d| d.uri == uri && !d.diagnostics.is_empty())
        })
        .await;
    assert!(resumed, "diagnostics must resume after the config parses");

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn config_turning_invalid_clears_diagnostics() -> TestResult {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await?;

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    let text = "jobs:\n  build:\n    steps:\n      - uses: gitlab.com/owner/repo@v1\n";
    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
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
        .await?;

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

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn config_reload_rejects_renamed_key() -> TestResult {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await?;

    std::fs::write(&config_file, "update_policy = \"patch\"\n")?;
    let config_uri = Uri::from_file_path(&config_file).ok_or("bad config uri")?;
    harness
        .notify(did_change_watched_files_notification(&config_uri))
        .await?;

    let config_path = actup_core::canonical_path(temp.path())
        .join("actup.toml")
        .display()
        .to_string();
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

    harness.shutdown().await?;
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

    async fn notify_changed(&mut self, file: &std::path::Path) -> TestResult {
        let uri = Uri::from_file_path(file).ok_or("bad config uri")?;
        self.notify(did_change_watched_files_notification(&uri))
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn invalid_github_yaml_config_reloads_on_change() -> TestResult {
    let temp = tempfile::tempdir()?;
    std::fs::create_dir(temp.path().join(".github"))?;
    let config_file = temp.path().join(".github/actup.yaml");
    std::fs::write(&config_file, "policy:\n  bump: sideways\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await?;
    assert!(
        harness
            .error_shown(
                &actup_core::canonical_path(temp.path())
                    .join(".github")
                    .join("actup.yaml")
                    .display()
                    .to_string()
            )
            .await,
        "expected an error showMessage naming .github/actup.yaml"
    );

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1)?)
        .await?;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&config_file, "cache:\n  enabled: false\n")?;
    harness.notify_changed(&config_file).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after .github/actup.yaml parses"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_user_config_reloads_on_change() -> TestResult {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo)?;
    std::fs::write(repo.join("actup.toml"), "[cache]\nenabled = false\n")?;
    let user = temp.path().join("config.toml");
    std::fs::write(&user, "[policy\n")?;
    let root = repo.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::with_user_config(Some(user.clone()));
    harness.initialize(Some(root)).await?;
    assert!(
        harness.error_shown(&user.display().to_string()).await,
        "expected an error showMessage naming the user config"
    );

    let uri = Uri::from_file_path(repo.join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1)?)
        .await?;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&user, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&user).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after the user config parses"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_config_path_beside_valid_repo_config_is_invalid() -> TestResult {
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
        .await?;
    assert!(
        harness.error_shown(explicit_str).await,
        "expected an error showMessage naming configPath"
    );

    let uri = Uri::from_file_path(repo.join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1)?)
        .await?;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&explicit, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&explicit).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after configPath parses"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn relative_config_path_resolves_against_workspace_root() -> TestResult {
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
        .await?;
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

    let uri = Uri::from_file_path(repo.join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1)?)
        .await?;
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&explicit, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&explicit).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after the relative configPath parses"
    );

    let second = temp.path().join("second");
    std::fs::create_dir(&second)?;
    let second_explicit = second.join("custom.toml");
    std::fs::write(&second_explicit, "[policy]\nbump = \"backwards\"\n")?;
    let second_explicit_str = second_explicit.to_str().ok_or("temp path is not UTF-8")?;
    let change = DidChangeWorkspaceFoldersParams {
        event: WorkspaceFoldersChangeEvent {
            added: vec![WorkspaceFolder {
                uri: Uri::from_file_path(&second).ok_or("bad uri")?,
                name: "second".to_owned(),
            }],
            removed: vec![WorkspaceFolder {
                uri: repo_uri,
                name: "repo".to_owned(),
            }],
        },
    };
    harness.clear_diagnostics().await;
    harness
        .notify(
            Request::build("workspace/didChangeWorkspaceFolders")
                .params(json!(change))
                .finish(),
        )
        .await?;
    assert!(
        harness.error_shown(second_explicit_str).await,
        "expected the new root's configPath to load"
    );
    harness.assert_paused(&uri).await;

    harness.clear_diagnostics().await;
    std::fs::write(&second_explicit, "[cache]\nenabled = false\n")?;
    harness.notify_changed(&second_explicit).await?;
    assert!(
        harness.diagnostics_published(&uri).await,
        "diagnostics must resume after the new root's configPath parses"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn missing_config_path_pauses_diagnostics() -> TestResult {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("actup.toml"), "[cache]\nenabled = false\n")?;
    let explicit = temp.path().join("deleted.toml");
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;
    let explicit_str = explicit.to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness
        .initialize_with(Some(root), Some(json!({ "configPath": explicit_str })))
        .await?;
    assert!(
        harness.error_shown(explicit_str).await,
        "expected an error showMessage naming the missing configPath"
    );

    let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
    harness
        .notify(did_open_notification(&uri, UNRESOLVED_WORKFLOW, 1)?)
        .await?;
    harness.assert_paused(&uri).await;

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn blank_config_path_falls_back_to_discovery() -> TestResult {
    let temp = tempfile::tempdir()?;
    std::fs::write(
        temp.path().join("actup.toml"),
        "[policy]\nbump = \"sideways\"\n",
    )?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    for blank in ["", "  "] {
        let mut harness = TestHarness::new();
        harness
            .initialize_with(Some(root), Some(json!({ "configPath": blank })))
            .await?;
        assert!(
            harness.error_shown("sideways").await,
            "configPath {blank:?} must leave discovery to find the repo config"
        );
        harness.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unrelated_file_change_does_not_reload() -> TestResult {
    let temp = tempfile::tempdir()?;
    let config_file = temp.path().join("actup.toml");
    std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
    let root = temp.path().to_str().ok_or("temp path is not UTF-8")?;

    let mut harness = TestHarness::new();
    harness.initialize(Some(root)).await?;

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

    harness.shutdown().await?;
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
async fn config_watchers_follow_workspace_root() -> TestResult {
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
        .await?;

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
        .await?;
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

    harness.shutdown().await?;
    Ok(())
}

// Hover protocol tests

#[tokio::test]
async fn hover_on_uses_line_returns_response() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/hover.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    // Wait for diagnostics to ensure the document is fully processed.
    harness
        .wait_for_diagnostics(&uri, Duration::from_secs(3))
        .await;

    // Hover on the `uses:` value (line 6, middle of "actions/checkout@v4").
    let position = Position {
        line: 6,
        character: 20,
    };
    let resp = harness.request(hover_request(10, &uri, position)?).await?;
    let resp = resp.ok_or("hover should return a response")?;
    let (_, body) = resp.into_parts();

    // The response should be Ok — either Some(Hover) or null.
    // We just validate the protocol works; the actual content depends on
    // whether the provider can resolve (no token = likely error/null).
    assert!(body.is_ok(), "hover should not return a JSON-RPC error");

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn hover_reads_markdown_and_plain_text_documents() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let cases = [
        (
            "/test/USAGE.md",
            "# Usage\n\n```yaml\nsteps:\n  - uses: actions/checkout@v3\n```\n",
            Position::new(4, 20),
            Range::new(Position::new(4, 10), Position::new(4, 29)),
        ),
        (
            "/test/steps.ts",
            "export const step = { uses: \"actions/checkout@v3\" };\n",
            Position::new(0, 35),
            Range::new(Position::new(0, 29), Position::new(0, 48)),
        ),
    ];
    for (id, (path, text, position, range)) in (30..).zip(cases) {
        let uri = Uri::from_file_path(path).ok_or("bad uri")?;
        harness
            .notify(did_open_notification(&uri, text, 1)?)
            .await?;
        harness
            .wait_for_diagnostics(&uri, Duration::from_secs(5))
            .await;
        let value = harness
            .request(hover_request(id, &uri, position)?)
            .await?
            .ok_or("hover should return a response")?
            .into_parts()
            .1?;
        let hover: tower_lsp_server::ls_types::Hover = serde_json::from_value(value)?;
        assert_eq!(hover.range, Some(range), "{path}");
        let tower_lsp_server::ls_types::HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover in {path}");
        };
        assert!(markup.value.contains("actions/checkout"), "{path}");
        assert!(markup.value.contains("v4.2.0"), "{path}: {}", markup.value);
    }

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn hover_on_non_uses_line_returns_null() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/hover-null.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Hover on a non-uses line (line 0 = "name: CI").
    let position = Position {
        line: 0,
        character: 3,
    };
    let resp = harness.request(hover_request(11, &uri, position)?).await?;
    let resp = resp.ok_or("hover should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    assert!(
        value.is_null(),
        "hover on non-uses line should return null, got: {value}"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn hover_without_open_document_returns_null() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    // Request hover on a document that was never opened.
    let uri = Uri::from_file_path("/test/not-opened.yml").ok_or("invalid file path")?;
    let position = Position {
        line: 0,
        character: 0,
    };
    let resp = harness.request(hover_request(12, &uri, position)?).await?;
    let resp = resp.ok_or("hover should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    assert!(
        value.is_null(),
        "hover on unopened document should return null"
    );

    harness.shutdown().await?;
    Ok(())
}

// Completion protocol tests

#[tokio::test]
async fn completion_on_uses_line_returns_response() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/completion.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger completion after "@" (line 6, after "actions/checkout@").
    let position = Position {
        line: 6,
        character: 31,
    };
    let resp = harness
        .request(completion_request(20, &uri, position, Some("@"))?)
        .await?;
    let resp = resp.ok_or("completion should return a response")?;
    let (_, body) = resp.into_parts();

    // Protocol validation: the response should be Ok, not a JSON-RPC error.
    assert!(
        body.is_ok(),
        "completion should not return a JSON-RPC error"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completion_on_non_uses_line_returns_null() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/completion-null.yml").ok_or("invalid file path")?;
    let text = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n";

    harness
        .notify(did_open_notification(&uri, text, 1)?)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger completion on a non-uses line.
    let position = Position {
        line: 0,
        character: 5,
    };
    let resp = harness
        .request(completion_request(21, &uri, position, None)?)
        .await?;
    let resp = resp.ok_or("completion should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    assert!(
        value.is_null(),
        "completion on non-uses line should return null, got: {value}"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completion_without_open_document_returns_null() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let uri = Uri::from_file_path("/test/completion-missing.yml").ok_or("invalid file path")?;
    let position = Position {
        line: 0,
        character: 0,
    };
    let resp = harness
        .request(completion_request(22, &uri, position, None)?)
        .await?;
    let resp = resp.ok_or("completion should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    assert!(
        value.is_null(),
        "completion on unopened document should return null"
    );

    harness.shutdown().await?;
    Ok(())
}

// executeCommand protocol tests

#[tokio::test]
async fn execute_command_fix_all_returns_response() -> TestResult {
    let mut harness = TestHarness::new();

    // Use a temp dir as workspace root to avoid touching real files.
    let temp = tempfile::tempdir()?;
    let workspace_root = temp.path().to_str().ok_or("temp dir path is not UTF-8")?;

    harness.initialize(Some(workspace_root)).await?;

    let resp = harness
        .request(execute_command_request(
            30,
            "actup-lsp/fixAllInWorkspace",
            vec![],
        )?)
        .await?;
    let resp = resp.ok_or("executeCommand should return a response")?;
    let (_, body) = resp.into_parts();

    // Protocol validation: the handler should not produce a JSON-RPC error.
    assert!(
        body.is_ok(),
        "executeCommand should not return a JSON-RPC error"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn execute_command_unknown_command_returns_null() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;

    let resp = harness
        .request(execute_command_request(
            31,
            "actup.nonExistentCommand",
            vec![],
        )?)
        .await?;
    let resp = resp.ok_or("executeCommand should return a response")?;
    let (_, body) = resp.into_parts();
    let value = body?;

    // Unknown commands return Ok(None) → JSON null (logged as warning).
    assert!(
        value.is_null(),
        "unknown command should return null, got: {value}"
    );

    harness.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn work_done_progress_cancel_cancels_the_running_command() -> TestResult {
    let mut harness = TestHarness::new();
    harness.initialize(None).await?;
    let running = harness.service.inner().progress.start();
    let other = harness.service.inner().progress.start();

    harness
        .notify(
            Request::build("window/workDoneProgress/cancel")
                .params(serde_json::to_value(WorkDoneProgressCancelParams {
                    token: running.token.clone(),
                })?)
                .finish(),
        )
        .await?;

    assert!(running.cancelled.is_cancelled());
    assert!(!other.cancelled.is_cancelled());
    harness.shutdown().await?;
    Ok(())
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
        let on_edit_line = u32::try_from(i)
            .is_ok_and(|line| line == edit.range.start.line && line == edit.range.end.line);
        if on_edit_line {
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
