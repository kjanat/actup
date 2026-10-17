//! actup-mcp: a Model Context Protocol server for checking and updating `uses:` refs.
//!
//! [`Server`] answers the read tools `actionGetReadme`, `actionGetActionYml`,
//! `getVersions` and `getLatest`, and the mutators `updateFile` and
//! `updateAllUsesToVersion`. Mutators only touch files inside a [`Root`].

#![warn(missing_docs)]

mod args;
mod confine;
mod diff;
mod read;
mod report;
mod write;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use actup_core::{Clock, Config, FileCache, Registration, clock, provider::Registry};
pub use confine::{Refusal, Root};
use rust_mcp_sdk::{
    McpServer, StdioTransport, ToMcpServerHandler, TransportOptions,
    error::SdkResult,
    mcp_server::{McpServerOptions, ServerHandler, server_runtime},
    schema::{
        CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities,
        ServerCapabilitiesTools, Tool, ToolAnnotations, schema_utils::CallToolError,
    },
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

use crate::{args::input_schema, report::Reply};

/// The provider registrations the server builds its registry from.
pub static REGISTRATIONS: [&Registration; 3] = [
    &actup_provider_github::REGISTRATION,
    &actup_provider_gitlab::REGISTRATION,
    &actup_provider_gitea::REGISTRATION,
];

/// Instructions the server sends to clients at initialization.
pub const INSTRUCTIONS: &str = "\
actup checks and updates GitHub-compatible Actions `uses:` refs in workflows and composite actions.

Read tools (no side effects, results reused for `cache.ttl` seconds):
- getVersions: semver tags, the latest tag, and with includeBranches the branches of a repository.
- getLatest: the version that the configured policy (or policyOverride) moves currentRef to. \
refKind names what currentRef is (tag, branch or sha); sameCommit names a target on the commit currentRef already points at.
- actionGetReadme / actionGetActionYml: README and action.yml at a ref (default branch when omitted).

Mutators (write files; pass dryRun: true to plan without writing):
- updateFile: replace the ref of named `uses:` values in one file. Every edit applies or none does, \
and every edit names an action on the same host. \
A trailing label comment names a version or the current ref (`@main # main note`). \
A new ref that is a version token counts as a tag, and any other name except a SHA as a branch. \
The label names a new tag written over a tag or branch and keeps its note; \
a new branch, a new SHA, or a tag written over a SHA pin drops the label and keeps the note. \
Any other trailing comment stays as written. \
It also replaces an invalid ref when the rewritten value parses, and refuses a value without `@`.
- updateAllUsesToVersion: move every reference to one action under workspaceRoot to targetRef. \
SHA-pinned references stay pinned: the provider resolves targetRef to its current commit on every call \
and the line keeps a `# <ref>` comment. \
A matching `uses:` value that is not a valid reference stays unchanged and is listed under unresolvable with its error.

Every path must resolve inside the server root (its working directory or --root). \
Paths that escape it, including through symlinks, are refused.

Results carry provider, host and status: upToDate, outdated, unknown (network or auth failure, \
not a user error), unresolvable (missing repository or ref) or rateLimited. \
Read tools report upToDate when the provider answered. Mutators report outdated when they changed \
(or in a dry run would change) a ref and upToDate when every ref already matched, so a repeated call changes nothing.

Mutator results list each change with path, 1-based line, range (0-based byte offsets of the replaced text \
in the original line), before and after lines, and a unified diff. reversal.diff is the reverse patch and \
reversal.lines maps each changed line back to its original text.";

const README: &str = "actionGetReadme";
const ACTION_YML: &str = "actionGetActionYml";
const VERSIONS: &str = "getVersions";
const LATEST: &str = "getLatest";
const UPDATE_FILE: &str = "updateFile";
const UPDATE_ALL: &str = "updateAllUsesToVersion";

/// The actup MCP tool handler.
pub struct Server {
    config: Config,
    registry: Registry,
    root: Root,
    file_cache: FileCache,
    clock: Arc<dyn Clock>,
    memo: Mutex<HashMap<String, (Reply, SystemTime)>>,
}

impl Server {
    /// Serve `registry` under `config` with version sets read through `file_cache`, confining every path to `root`.
    #[must_use]
    pub fn new(config: Config, registry: Registry, root: Root, file_cache: FileCache) -> Self {
        Self::with_clock(config, registry, root, file_cache, Arc::new(clock::System))
    }

    fn with_clock(
        config: Config,
        registry: Registry,
        root: Root,
        file_cache: FileCache,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            registry,
            root,
            file_cache,
            clock,
            memo: Mutex::new(HashMap::new()),
        }
    }

    /// The tools this server offers.
    #[must_use]
    pub fn tools() -> Vec<Tool> {
        let read = |title: &str| ToolAnnotations {
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(true),
            read_only_hint: Some(true),
            title: Some(title.to_string()),
        };
        let write = |title: &str, open_world: bool| ToolAnnotations {
            destructive_hint: Some(true),
            idempotent_hint: Some(true),
            open_world_hint: Some(open_world),
            read_only_hint: Some(false),
            title: Some(title.to_string()),
        };
        vec![
            tool::<args::Readme>(
                README,
                "Get the README of an action repository at a ref.",
                read("Get action README"),
            ),
            tool::<args::ActionYml>(
                ACTION_YML,
                "Get the raw and parsed action.yml of an action at a ref.",
                read("Get action.yml"),
            ),
            tool::<args::Versions>(
                VERSIONS,
                "List the semver tags, the latest tag and optionally the branches of an action repository.",
                read("List versions"),
            ),
            tool::<args::Latest>(
                LATEST,
                "Check a ref against the configured update policy and return the version it may move to.",
                read("Get latest allowed version"),
            ),
            tool::<args::UpdateFile>(
                UPDATE_FILE,
                "Replace the refs of named `uses:` values in one file inside the server root. \
                 Returns changed ranges, a unified diff and a reversal plan.",
                write("Update refs in a file", false),
            ),
            tool::<args::UpdateAll>(
                UPDATE_ALL,
                "Update every reference to one action under a directory inside the server root to a target ref. \
                 Returns changed ranges, a unified diff and a reversal plan.",
                write("Update all references to an action", true),
            ),
        ]
    }

    /// Run the tool `name` with `arguments`.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown tool, invalid arguments, or a refused path.
    pub async fn call(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<CallToolResult, CallToolError> {
        let arguments = Value::Object(arguments.unwrap_or_default());
        let reply = match name {
            README => {
                let args: args::Readme = parse(name, arguments)?;
                self.cached(name, &args, read::readme(name, &self.registry, &args))
                    .await?
            }
            ACTION_YML => {
                let args: args::ActionYml = parse(name, arguments)?;
                self.cached(name, &args, read::action_yml(name, &self.registry, &args))
                    .await?
            }
            VERSIONS => {
                let args: args::Versions = parse(name, arguments)?;
                self.cached(
                    name,
                    &args,
                    read::versions(name, &self.registry, &self.file_cache, &args),
                )
                .await?
            }
            LATEST => {
                let args: args::Latest = parse(name, arguments)?;
                self.cached(
                    name,
                    &args,
                    read::latest(name, &self.registry, &self.file_cache, &self.config, &args),
                )
                .await?
            }
            UPDATE_FILE => {
                let args: args::UpdateFile = parse(name, arguments)?;
                write::update_file(name, &self.registry, &self.root, &args)?
            }
            UPDATE_ALL => {
                let args: args::UpdateAll = parse(name, arguments)?;
                write::update_all(name, &self.registry, &self.config, &self.root, &args).await?
            }
            _ => return Err(CallToolError::unknown_tool(name)),
        };
        Ok(reply.into_result())
    }

    async fn cached(
        &self,
        name: &str,
        args: &(impl Serialize + Sync),
        run: impl Future<Output = Result<Reply, CallToolError>>,
    ) -> Result<Reply, CallToolError> {
        let key = format!(
            "{name}:{}",
            serde_json::to_string(args).map_err(CallToolError::new)?
        );
        let ttl = self.config.cache.ttl;
        if let Some(hit) = self.memo.lock().ok().and_then(|memo| {
            memo.get(&key)
                .filter(|(_, at)| {
                    self.clock
                        .now()
                        .duration_since(*at)
                        .is_ok_and(|age| age <= ttl)
                })
                .map(|(reply, _)| reply.clone())
        }) {
            return Ok(hit);
        }
        let reply = run.await?;
        if reply.succeeded()
            && let Ok(mut memo) = self.memo.lock()
        {
            memo.insert(key, (reply.clone(), self.clock.now()));
        }
        Ok(reply)
    }
}

fn tool<T: JsonSchema>(name: &str, description: &str, annotations: ToolAnnotations) -> Tool {
    Tool {
        title: annotations.title.clone(),
        annotations: Some(annotations),
        description: Some(description.to_string()),
        execution: None,
        icons: Vec::new(),
        input_schema: input_schema::<T>(),
        meta: None,
        name: name.to_string(),
        output_schema: None,
    }
}

fn parse<T: DeserializeOwned>(tool: &str, arguments: Value) -> Result<T, CallToolError> {
    serde_json::from_value(arguments)
        .map_err(|e| CallToolError::invalid_arguments(tool, Some(e.to_string())))
}

#[async_trait::async_trait]
impl ServerHandler for Server {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: Self::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        self.call(&params.name, params.arguments).await
    }
}

/// The initialization result this server sends.
#[must_use]
pub fn server_details() -> InitializeResult {
    InitializeResult {
        server_info: Implementation {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: Some("actup".to_string()),
            description: Some(env!("CARGO_PKG_DESCRIPTION").to_string()),
            icons: Vec::new(),
            website_url: Some(env!("CARGO_PKG_HOMEPAGE").to_string()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..ServerCapabilities::default()
        },
        protocol_version: ProtocolVersion::V2025_11_25.into(),
        instructions: Some(INSTRUCTIONS.to_string()),
        meta: None,
    }
}

/// Serve `server` over the process's stdin and stdout until the client disconnects.
///
/// # Errors
///
/// Returns an error if the transport cannot start or fails.
pub async fn serve_stdio(server: Server) -> SdkResult<()> {
    let transport = StdioTransport::new(TransportOptions::default())?;
    let runtime = server_runtime::create_server(McpServerOptions {
        server_details: server_details(),
        transport,
        handler: server.to_mcp_server_handler(),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });
    runtime.start().await
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use actup_core::{
        ActionRepoId, BoxFuture, CachingProvider, Retention,
        provider::{ActionYml, Kind, Provider, Sha40},
        version,
    };

    use super::*;

    #[derive(Debug)]
    struct StepClock(Mutex<SystemTime>);

    impl StepClock {
        fn advance(&self, by: Duration) {
            if let Ok(mut now) = self.0.lock()
                && let Some(moved) = now.checked_add(by)
            {
                *now = moved;
            }
        }
    }

    impl Clock for StepClock {
        fn now(&self) -> SystemTime {
            self.0.lock().map_or(SystemTime::UNIX_EPOCH, |now| *now)
        }
    }

    #[test]
    fn registrations_cover_every_provider() {
        let kinds: Vec<actup_core::HostKind> = REGISTRATIONS.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                actup_core::HostKind::GitHub,
                actup_core::HostKind::GitLab,
                actup_core::HostKind::Gitea
            ]
        );
    }

    struct CountingProvider(Arc<AtomicUsize>, AtomicUsize);

    impl Provider for CountingProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let resolves = self.1.fetch_add(1, Ordering::SeqCst);
            let result = ["b", "c"]
                .get(resolves)
                .and_then(|c| Sha40::new(c.repeat(40)))
                .ok_or_else(|| actup_core::Error::RefNotFound {
                    owner: String::new(),
                    repo: String::new(),
                    ref_name: ref_name.to_string(),
                    host: None,
                });
            Box::pin(async move { result })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            let calls = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move { Ok(Some(format!("call {calls}"))) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn server() -> Result<(tempfile::TempDir, Server), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = Root::new(dir.path())?;
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        Ok((
            dir,
            Server::new(Config::default(), registry, root, FileCache::disabled()),
        ))
    }

    #[tokio::test]
    async fn read_tools_answer_repeated_calls_from_the_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(CountingProvider(
            Arc::clone(&calls),
            AtomicUsize::new(0),
        )));
        let clock = Arc::new(StepClock(Mutex::new(SystemTime::UNIX_EPOCH)));
        let dyn_clock: Arc<dyn Clock> = clock.clone();
        let config = Config::default();
        let ttl = config.cache.ttl;
        let server = Server::with_clock(
            config,
            registry,
            Root::new(dir.path())?,
            FileCache::disabled(),
            dyn_clock,
        );
        let args = |reference: &str| {
            let Value::Object(args) =
                serde_json::json!({ "owner": "a", "repo": "b", "ref": reference })
            else {
                unreachable!("object literal");
            };
            Some(args)
        };
        let readme = |result: CallToolResult| {
            result
                .structured_content
                .and_then(|body| body.get("readme").cloned())
        };
        let first = readme(server.call(README, args("v1")).await?);
        let second = readme(server.call(README, args("v1")).await?);
        let other = readme(server.call(README, args("v2")).await?);
        assert_eq!(first, Some(Value::from("call 1")));
        assert_eq!(second, first);
        assert_eq!(other, Some(Value::from("call 2")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        clock.advance(ttl);
        let held = readme(server.call(README, args("v1")).await?);
        assert_eq!(held, Some(Value::from("call 1")));
        clock.advance(Duration::from_secs(1));
        let expired = readme(server.call(README, args("v1")).await?);
        assert_eq!(expired, Some(Value::from("call 3")));
        Ok(())
    }

    #[tokio::test]
    async fn read_tools_share_version_sets_through_the_file_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cache_dir = tempfile::tempdir()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let call = |tool: &str, args: Value| {
            let Value::Object(args) = args else {
                unreachable!("object literal");
            };
            (tool.to_owned(), Some(args))
        };
        for _ in 0..2 {
            let mut registry = Registry::new();
            registry.set_default_host("github.com");
            registry.register(Box::new(CountingProvider(
                Arc::clone(&calls),
                AtomicUsize::new(0),
            )));
            let config = Config::default();
            let file_cache = FileCache::with_path(cache_dir.path().to_path_buf(), config.cache.ttl);
            let server = Server::new(config, registry, Root::new(dir.path())?, file_cache);
            for (tool, args) in [
                call(VERSIONS, serde_json::json!({ "owner": "a", "repo": "b" })),
                call(
                    LATEST,
                    serde_json::json!({ "owner": "a", "repo": "b", "currentRef": "v1" }),
                ),
            ] {
                let result = server.call(&tool, args).await?;
                let provider = result
                    .structured_content
                    .and_then(|body| body.get("provider").cloned());
                assert_eq!(provider, Some(Value::from("github")), "{tool}");
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_dir(cache_dir.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn update_all_resolves_a_fresh_sha_on_every_call()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflow = dir.path().join(".github/workflows/ci.yml");
        std::fs::create_dir_all(dir.path().join(".github/workflows"))?;
        std::fs::write(
            &workflow,
            format!("steps:\n  - uses: a/b@{} # main\n", "a".repeat(40)),
        )?;
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(CachingProvider::with_retention(
            CountingProvider(Arc::new(AtomicUsize::new(0)), AtomicUsize::new(0)),
            Retention::expiring(Config::default().cache.ttl),
        )));
        let server = Server::new(
            Config::default(),
            registry,
            Root::new(dir.path())?,
            FileCache::disabled(),
        );
        let Value::Object(args) = serde_json::json!({
            "workspaceRoot": ".",
            "match": { "owner": "a", "repo": "b" },
            "targetRef": "main",
        }) else {
            unreachable!("object literal");
        };
        for sha in ["b", "c"] {
            let result = server.call(UPDATE_ALL, Some(args.clone())).await?;
            assert_eq!(result.is_error, None, "{result:?}");
            assert_eq!(
                std::fs::read_to_string(&workflow)?,
                format!("steps:\n  - uses: a/b@{} # main\n", sha.repeat(40))
            );
        }
        Ok(())
    }

    struct StaleProvider(Arc<AtomicUsize>);

    impl Provider for StaleProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let latest = Arc::new(version::Info::new(
                version::Version::parse("v4.2.0"),
                "b".repeat(40),
            ));
            let set = version::Set::build(
                vec![Arc::clone(&latest)],
                std::collections::HashMap::from([(4, Arc::clone(&latest))]),
                std::collections::HashMap::new(),
                Some(latest),
            );
            let set = match actup_core::cache::Staleness::after(
                &actup_core::Error::RateLimited {
                    provider: "github".to_string(),
                    retry_after: Some(std::time::Duration::from_mins(12)),
                },
                std::time::Duration::from_hours(26),
            ) {
                Some(stale) => set.into_stale(stale),
                None => set,
            };
            Box::pin(async move { Ok(set) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let err = actup_core::Error::RefNotFound {
                owner: String::new(),
                repo: String::new(),
                ref_name: ref_name.to_string(),
                host: None,
            };
            Box::pin(async move { Err(err) })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn read_tools_report_stale_versions_and_leave_them_out_of_the_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cache_dir = tempfile::tempdir()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(StaleProvider(Arc::clone(&calls))));
        let config = Config::default();
        let file_cache = FileCache::with_path(cache_dir.path().to_path_buf(), config.cache.ttl);
        let server = Server::new(config, registry, Root::new(dir.path())?, file_cache);
        let args = |json: Value| match json {
            Value::Object(args) => Some(args),
            _ => None,
        };
        let stale = serde_json::json!({
            "age_secs": 93_600,
            "reason": "rate_limited",
            "retry_after_secs": 720,
        });

        for _ in 0..2 {
            let latest = server
                .call(
                    LATEST,
                    args(serde_json::json!({ "owner": "a", "repo": "b", "currentRef": "v4.0.0" })),
                )
                .await?;
            assert_eq!(latest.is_error, None);
            let body = latest.structured_content.ok_or("no body")?;
            assert_eq!(body.get("status"), Some(&Value::from("outdated")));
            assert_eq!(body.get("check"), Some(&Value::from("outdated")));
            assert_eq!(body.get("stale"), Some(&stale));
        }

        let versions = server
            .call(
                VERSIONS,
                args(serde_json::json!({ "owner": "a", "repo": "b" })),
            )
            .await?
            .structured_content
            .ok_or("no body")?;
        assert_eq!(versions.get("status"), Some(&Value::from("upToDate")));
        assert_eq!(versions.get("stale"), Some(&stale));

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(server.memo.lock().map_err(|e| e.to_string())?.is_empty());
        assert_eq!(std::fs::read_dir(cache_dir.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn tools_mark_mutators_destructive_and_reads_read_only() {
        let tools = Server::tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                README,
                ACTION_YML,
                VERSIONS,
                LATEST,
                UPDATE_FILE,
                UPDATE_ALL
            ]
        );
        for tool in &tools {
            let Some(hints) = &tool.annotations else {
                panic!("{} has no annotations", tool.name);
            };
            let mutates = tool.name.starts_with("update");
            assert_eq!(hints.read_only_hint, Some(!mutates), "{}", tool.name);
            assert_eq!(hints.destructive_hint, Some(mutates), "{}", tool.name);
            assert_eq!(hints.idempotent_hint, Some(true), "{}", tool.name);
        }
    }

    #[test]
    fn server_details_send_the_instructions_and_tool_capability() {
        let details = server_details();
        assert_eq!(details.instructions.as_deref(), Some(INSTRUCTIONS));
        assert!(details.capabilities.tools.is_some());
        assert_eq!(details.server_info.name, "actup-mcp");
    }

    #[tokio::test]
    async fn call_rejects_unknown_tools_and_invalid_arguments()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, server) = server()?;
        let Err(unknown) = server.call("nope", None).await else {
            panic!("unknown tool must fail");
        };
        assert!(unknown.to_string().contains("nope"), "{unknown}");
        let Err(invalid) = server.call(VERSIONS, None).await else {
            panic!("missing owner must fail");
        };
        assert!(invalid.to_string().contains("owner"), "{invalid}");
        Ok(())
    }

    #[tokio::test]
    async fn call_reports_an_unserved_host_without_caching_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, server) = server()?;
        let args = serde_json::json!({ "owner": "actions", "repo": "checkout" });
        let Value::Object(args) = args else {
            panic!("object");
        };
        let result = server.call(VERSIONS, Some(args)).await?;
        assert_eq!(result.is_error, Some(true));
        let Some(body) = result.structured_content else {
            panic!("structured content");
        };
        assert_eq!(body.get("provider"), Some(&Value::from("unknown")));
        assert_eq!(body.get("host"), Some(&Value::from("github.com")));
        assert_eq!(body.get("status"), Some(&Value::from("unknown")));
        assert!(server.memo.lock().map_err(|e| e.to_string())?.is_empty());
        Ok(())
    }
}
