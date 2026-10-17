//! Contract tests that drive `actup-mcp` over stdio with a mock provider.
//!
//! The test binary re-executes itself with `MCP_CONTRACT_ROOT` set to serve
//! one server on its own stdio, and the parent talks JSON-RPC to it.

use std::{
    collections::HashMap,
    error::Error,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitCode, Stdio},
    sync::{Arc, mpsc},
    time::Duration,
};

use actup_core::{
    ActionRepoId, BoxFuture, Config, Error as CoreError,
    provider::{ActionYml, Kind, Provider, Registry, Sha40},
    version,
};
use actup_mcp::{Root, Server, serve_stdio};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

const CHILD_ENV: &str = "MCP_CONTRACT_ROOT";

const TRACK_ENV: &str = "MCP_CONTRACT_TRACK_EXACT";

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

struct MockProvider;

impl MockProvider {
    fn failure(repo: &ActionRepoId) -> Option<CoreError> {
        match repo.owner() {
            "ghost" => Some(CoreError::NotFound {
                owner: repo.owner().into(),
                repo: repo.repo().into(),
                host: None,
            }),
            "busy" => Some(CoreError::RateLimited {
                provider: "github".into(),
                retry_after: None,
            }),
            "offline" => Some(CoreError::Network {
                operation: "ls-refs".into(),
                message: "connection refused".into(),
                source: None,
            }),
            _ => None,
        }
    }

    fn twin_versions() -> version::Set {
        let info =
            |tag: &str, c: char| Arc::new(version::Info::new(version::Version::parse(tag), sha(c)));
        let major = info("v1", 'f');
        let old = info("v1.0.4", 'g');
        let new = info("v1.0.5", 'f');
        version::Set::build(
            vec![Arc::clone(&major), old, Arc::clone(&new)],
            HashMap::from([(1, Arc::clone(&new))]),
            HashMap::from([(1, major)]),
            Some(new),
        )
    }

    fn versions() -> version::Set {
        let info =
            |tag: &str, c: char| Arc::new(version::Info::new(version::Version::parse(tag), sha(c)));
        let old = info("v4.0.0", 'a');
        let new = info("v4.2.0", 'b');
        let rc = info("v5.0.0-rc.1", 'e');
        let mut set = version::Set::build(
            vec![Arc::clone(&old), Arc::clone(&new), Arc::clone(&rc)],
            HashMap::from([(4, Arc::clone(&new)), (5, Arc::clone(&rc))]),
            HashMap::new(),
            Some(new),
        );
        set.set_default_branch(version::BranchInfo::new("main", sha('c')));
        set.set_branches(HashMap::from([
            ("main".to_string(), sha('c')),
            ("dev".to_string(), sha('d')),
        ]));
        set
    }
}

impl Provider for MockProvider {
    fn kind(&self) -> Kind {
        Kind::GitHub
    }

    fn host(&self) -> &'static str {
        "github.com"
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<version::Set>> {
        let result = match Self::failure(repo) {
            Some(e) => Err(e),
            None if repo.owner() == "twins" => Ok(Self::twin_versions()),
            None => Ok(Self::versions()),
        };
        Box::pin(async move { result })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
        let found = match ref_name {
            "v4.0.0" => Sha40::new(sha('a')),
            "v4.2.0" | "v4" => Sha40::new(sha('b')),
            "main" => Sha40::new(sha('c')),
            _ => None,
        };
        let result = found.ok_or_else(|| CoreError::RefNotFound {
            owner: repo.owner().into(),
            repo: repo.repo().into(),
            ref_name: ref_name.into(),
            host: None,
        });
        Box::pin(async move { result })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<String>> {
        let result = Self::failure(repo).map_or_else(|| Ok("main".to_string()), Err);
        Box::pin(async move { result })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
        let result = Self::failure(repo).map_or_else(
            || {
                Ok(Some(format!(
                    "# {} at {}\n",
                    repo.repo(),
                    ref_name.unwrap_or("HEAD")
                )))
            },
            Err,
        );
        Box::pin(async move { result })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
        let name = format!("{} {}", repo.subpath().unwrap_or("root"), ref_name);
        Box::pin(async move { Ok(ActionYml::new(format!("name: {name}\n")).with_name(name)) })
    }
}

fn serve_child(root: &Path) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let root = match Root::new(root) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("root: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut registry = Registry::new();
    registry.set_default_host("github.com");
    registry.register(Box::new(MockProvider));
    let mut config = Config::default();
    config.scan.extra_paths = match ["docs/*.md", "infra/*.tf"]
        .map(str::parse)
        .into_iter()
        .collect()
    {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("extra paths: {e}");
            return ExitCode::FAILURE;
        }
    };
    if std::env::var_os(TRACK_ENV).is_some() {
        config.policy.track = actup_core::policy::Track::Exact;
    }
    let file_cache = actup_core::FileCache::disabled();
    match runtime.block_on(serve_stdio(Server::new(config, registry, root, file_cache))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Client {
    fn start(root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut command = Command::new(std::env::current_exe()?);
        command.env(CHILD_ENV, root);
        Self::spawn(command)
    }

    fn start_exact(root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut command = Command::new(std::env::current_exe()?);
        command.env(CHILD_ENV, root).env(TRACK_ENV, "1");
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Result<Self, Box<dyn Error>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "contract", "version": "0" },
            }),
        )?;
        if init["instructions"]
            .as_str()
            .is_none_or(|s| !s.contains("server root"))
        {
            return Err(format!("initialize returned no instructions: {init}").into());
        }
        client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(client)
    }

    fn send(&mut self, message: &Value) -> TestResult {
        writeln!(self.stdin, "{message}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        message["params"] = params;
        self.send(&message)?;
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(20))?;
            let message: Value = serde_json::from_str(&line)?;
            if message["id"] != json!(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("{method} failed: {error}").into());
            }
            return Ok(message["result"].clone());
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, Box<dyn Error>> {
        let mut params = json!({ "name": tool });
        params["arguments"] = arguments;
        self.request("tools/call", params)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn body(result: &Value) -> &Value {
    &result["structuredContent"]
}

fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

fn error_text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn expect(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

fn expect_eq(actual: &Value, expected: &Value, what: &str) -> TestResult {
    expect(actual == expected, || {
        format!("{what}: expected {expected}, got {actual}")
    })
}

const WORKFLOW: &str = "\
name: ci
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4.0.0
      - uses: actions/setup-node@v4
      - run: echo done
";

struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("repo/.github/workflows"))?;
        fs::create_dir_all(dir.path().join("repo/.github/actions/setup"))?;
        fs::write(dir.path().join("repo/.github/workflows/ci.yml"), WORKFLOW)?;
        fs::write(dir.path().join("outside.yml"), WORKFLOW)?;
        Ok(Self { dir })
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    fn read(&self, relative: &str) -> Result<String, Box<dyn Error>> {
        Ok(fs::read_to_string(self.path(relative))?)
    }

    fn write(&self, relative: &str, content: &str) -> TestResult {
        Ok(fs::write(self.path(relative), content)?)
    }
}

fn tools_list_marks_mutators_destructive(client: &mut Client, _ws: &Workspace) -> TestResult {
    let result = client.request("tools/list", json!({}))?;
    let tools = result["tools"].as_array().ok_or("tools array")?;
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    expect(
        names
            == [
                "actionGetReadme",
                "actionGetActionYml",
                "getVersions",
                "getLatest",
                "updateFile",
                "updateAllUsesToVersion",
            ],
        || format!("tool names: {names:?}"),
    )?;
    for tool in tools {
        let mutates = tool["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("update"));
        expect_eq(
            &tool["annotations"]["destructiveHint"],
            &json!(mutates),
            "destructiveHint",
        )?;
        expect_eq(
            &tool["annotations"]["readOnlyHint"],
            &json!(!mutates),
            "readOnlyHint",
        )?;
        expect_eq(
            &tool["inputSchema"]["type"],
            &json!("object"),
            "inputSchema.type",
        )?;
    }
    let update_all = &tools[5]["inputSchema"];
    expect_eq(
        &update_all["required"],
        &json!(["workspaceRoot", "match", "targetRef"]),
        "updateAllUsesToVersion required",
    )
}

fn read_tools_report_provider_host_and_status(client: &mut Client, _ws: &Workspace) -> TestResult {
    let versions = client.call(
        "getVersions",
        json!({ "owner": "actions", "repo": "checkout", "includeBranches": true }),
    )?;
    expect(!is_error(&versions), || {
        format!("getVersions failed: {versions}")
    })?;
    let v = body(&versions);
    expect_eq(&v["provider"], &json!("github"), "provider")?;
    expect_eq(&v["host"], &json!("github.com"), "host")?;
    expect_eq(&v["status"], &json!("upToDate"), "status")?;
    expect_eq(
        &v["tags"],
        &json!([{ "version": "v4.0.0", "sha": sha('a') }, { "version": "v4.2.0", "sha": sha('b') }]),
        "stable tags",
    )?;
    expect_eq(&v["latest"]["version"], &json!("v4.2.0"), "latest")?;
    expect_eq(&v["defaultBranch"]["name"], &json!("main"), "defaultBranch")?;
    expect_eq(
        &v["branches"],
        &json!([{ "name": "dev", "sha": sha('d') }, { "name": "main", "sha": sha('c') }]),
        "branches",
    )?;

    let with_rc = client.call(
        "getVersions",
        json!({ "owner": "actions", "repo": "checkout", "includePrerelease": true }),
    )?;
    expect_eq(
        &body(&with_rc)["latest"]["version"],
        &json!("v5.0.0-rc.1"),
        "latest prerelease",
    )?;
    expect(body(&with_rc).get("branches").is_none(), || {
        "branches without includeBranches".into()
    })?;

    let latest = client.call(
        "getLatest",
        json!({ "owner": "actions", "repo": "checkout", "currentRef": "v4.0.0" }),
    )?;
    let l = body(&latest);
    expect_eq(&l["status"], &json!("outdated"), "getLatest status")?;
    expect_eq(&l["check"], &json!("outdated"), "getLatest check")?;
    expect_eq(
        &l["target"],
        &json!({ "version": "v4.2.0", "sha": sha('b') }),
        "getLatest target",
    )?;
    expect_eq(&l["provider"], &json!("github"), "getLatest provider")?;

    let current = client.call(
        "getLatest",
        json!({ "owner": "actions", "repo": "checkout", "currentRef": "v4.2.0", "policyOverride": "patch" }),
    )?;
    expect_eq(
        &body(&current)["status"],
        &json!("upToDate"),
        "current getLatest status",
    )?;

    let readme = client.call(
        "actionGetReadme",
        json!({ "host": "github.com", "owner": "actions", "repo": "checkout", "ref": "v4" }),
    )?;
    expect_eq(
        &body(&readme)["readme"],
        &json!("# checkout at v4\n"),
        "readme",
    )?;
    expect_eq(&body(&readme)["host"], &json!("github.com"), "readme host")?;

    let yml = client.call(
        "actionGetActionYml",
        json!({ "owner": "actions", "repo": "cache", "path": "restore" }),
    )?;
    expect_eq(&body(&yml)["ref"], &json!("main"), "action.yml default ref")?;
    expect_eq(
        &body(&yml)["actionYml"]["name"],
        &json!("restore main"),
        "action.yml name",
    )?;
    Ok(())
}

fn latest_names_ref_kind_and_same_commit_target(ws: &Workspace) -> TestResult {
    let mut client = Client::start_exact(&ws.root())?;
    let latest = client.call(
        "getLatest",
        json!({ "owner": "twins", "repo": "r", "currentRef": "v1" }),
    )?;
    let l = body(&latest);
    expect_eq(&l["status"], &json!("outdated"), "same-commit status")?;
    expect_eq(
        &l["target"],
        &json!({ "version": "v1.0.5", "sha": sha('f') }),
        "same-commit target",
    )?;
    expect_eq(&l["refKind"], &json!("tag"), "same-commit refKind")?;
    expect_eq(&l["sameCommit"], &json!("v1.0.5"), "sameCommit")?;

    let behind = client.call(
        "getLatest",
        json!({ "owner": "twins", "repo": "r", "currentRef": "v1.0.4" }),
    )?;
    let b = body(&behind);
    expect_eq(&b["status"], &json!("outdated"), "real bump status")?;
    expect_eq(&b["refKind"], &json!("tag"), "real bump refKind")?;
    expect(b.get("sameCommit").is_none(), || {
        format!("a real bump has no sameCommit: {b}")
    })?;

    let unlisted = client.call(
        "getLatest",
        json!({ "owner": "twins", "repo": "r", "currentRef": "v9" }),
    )?;
    expect(body(&unlisted).get("refKind").is_none(), || {
        format!("an unlisted ref has no refKind: {}", body(&unlisted))
    })?;
    Ok(())
}

fn read_tools_classify_provider_failures(client: &mut Client, _ws: &Workspace) -> TestResult {
    for (owner, status) in [
        ("ghost", "unresolvable"),
        ("busy", "rateLimited"),
        ("offline", "unknown"),
    ] {
        let result = client.call("getVersions", json!({ "owner": owner, "repo": "x" }))?;
        expect(is_error(&result), || {
            format!("{owner}: expected isError: {result}")
        })?;
        expect_eq(&body(&result)["status"], &json!(status), owner)?;
        expect_eq(&body(&result)["provider"], &json!("github"), owner)?;
        expect(body(&result)["error"].is_string(), || {
            format!("{owner}: no error message")
        })?;
    }
    let readme = client.call("actionGetReadme", json!({ "owner": "ghost", "repo": "x" }))?;
    expect_eq(
        &body(&readme)["status"],
        &json!("unresolvable"),
        "readme of a missing repo",
    )?;
    let yml = client.call(
        "actionGetActionYml",
        json!({ "owner": "offline", "repo": "x" }),
    )?;
    expect(is_error(&yml), || {
        format!("default branch failure must fail: {yml}")
    })?;
    expect_eq(
        &body(&yml)["status"],
        &json!("unknown"),
        "action.yml without a default branch",
    )?;
    expect_eq(
        &body(&yml)["provider"],
        &json!("github"),
        "action.yml provider",
    )?;
    let missing_ref = client.call(
        "getLatest",
        json!({ "owner": "actions", "repo": "checkout", "currentRef": "v9.9.9" }),
    )?;
    expect_eq(
        &body(&missing_ref)["status"],
        &json!("unresolvable"),
        "getLatest of a missing tag",
    )?;
    let unserved = client.call(
        "getVersions",
        json!({ "host": "ghe.corp.com", "owner": "a", "repo": "b" }),
    )?;
    expect_eq(
        &body(&unserved)["provider"],
        &json!("unknown"),
        "unserved provider",
    )?;
    expect_eq(
        &body(&unserved)["host"],
        &json!("ghe.corp.com"),
        "unserved host",
    )
}

fn invalid_arguments_are_tool_errors(client: &mut Client, _ws: &Workspace) -> TestResult {
    let cases = [
        (
            "getLatest",
            json!({ "owner": "actions", "repo": "checkout" }),
            "currentRef",
        ),
        (
            "getVersions",
            json!({ "owner": "actions", "repo": "checkout", "extra": 1 }),
            "extra",
        ),
        (
            "getVersions",
            json!({ "owner": "a/b", "repo": "c" }),
            "single names",
        ),
        (
            "getLatest",
            json!({ "owner": "a", "repo": "b", "currentRef": "v1 x" }),
            "currentRef",
        ),
        (
            "updateFile",
            json!({ "path": ".github/workflows/ci.yml", "edits": [] }),
            "edits must not be empty",
        ),
        (
            "updateFile",
            json!({ "path": ".github/workflows/ci.yml", "edits": [{ "line": 0, "action": "a/b", "ref": "v1" }] }),
            "nonzero",
        ),
        (
            "updateFile",
            json!({ "path": ".github/workflows/ci.yml", "edits": [{ "line": 7, "action": "actions/checkout", "ref": "abc1234" }] }),
            "abbreviated",
        ),
        (
            "updateAllUsesToVersion",
            json!({ "workspaceRoot": ".", "match": { "owner": "actions", "repo": "checkout" }, "targetRef": sha('b') }),
            "commit SHA",
        ),
        ("noSuchTool", json!({}), "noSuchTool"),
    ];
    for (tool, arguments, needle) in cases {
        let result = client.call(tool, arguments)?;
        expect(is_error(&result), || {
            format!("{tool}: expected isError: {result}")
        })?;
        let text = error_text(&result);
        expect(text.contains(needle), || {
            format!("{tool}: `{needle}` not in {text:?}")
        })?;
    }
    Ok(())
}

fn update_file(client: &mut Client, arguments: Value) -> Result<Value, Box<dyn Error>> {
    client.call("updateFile", arguments)
}

fn update_file_returns_ranges_diff_and_reversal(client: &mut Client, ws: &Workspace) -> TestResult {
    let args = json!({
        "path": ".github/workflows/ci.yml",
        "edits": [{ "line": 7, "action": "actions/checkout", "ref": "v4.2.0" }],
    });
    let result = update_file(client, args.clone())?;
    expect(!is_error(&result), || {
        format!("updateFile failed: {result}")
    })?;
    let b = body(&result);
    expect_eq(&b["status"], &json!("outdated"), "status")?;
    expect_eq(&b["provider"], &json!("github"), "provider")?;
    expect_eq(&b["host"], &json!("github.com"), "host")?;
    expect_eq(
        &b["written"],
        &json!([".github/workflows/ci.yml"]),
        "written",
    )?;
    expect_eq(
        &b["changes"],
        &json!([{
            "path": ".github/workflows/ci.yml",
            "line": 7,
            "provider": "github",
            "host": "github.com",
            "action": "actions/checkout@v4.0.0",
            "range": { "start": 31, "end": 37 },
            "from": "v4.0.0",
            "to": "v4.2.0",
            "before": "      - uses: actions/checkout@v4.0.0",
            "after": "      - uses: actions/checkout@v4.2.0",
        }]),
        "changes",
    )?;
    let diff = "--- a/.github/workflows/ci.yml\n+++ b/.github/workflows/ci.yml\n\
                @@ -4,6 +4,6 @@\n   build:\n     runs-on: ubuntu-latest\n     steps:\n\
                -      - uses: actions/checkout@v4.0.0\n+      - uses: actions/checkout@v4.2.0\n\
                \x20      - uses: actions/setup-node@v4\n       - run: echo done\n";
    expect_eq(&b["diff"], &json!(diff), "diff")?;
    let reverse = diff.replace(
        "-      - uses: actions/checkout@v4.0.0\n+      - uses: actions/checkout@v4.2.0",
        "-      - uses: actions/checkout@v4.2.0\n+      - uses: actions/checkout@v4.0.0",
    );
    expect_eq(&b["reversal"]["diff"], &json!(reverse), "reversal diff")?;
    expect_eq(
        &b["reversal"]["lines"],
        &json!([{
            "path": ".github/workflows/ci.yml",
            "line": 7,
            "current": "      - uses: actions/checkout@v4.2.0",
            "original": "      - uses: actions/checkout@v4.0.0",
        }]),
        "reversal lines",
    )?;
    let written = ws.read(".github/workflows/ci.yml")?;
    expect(
        written == WORKFLOW.replace("checkout@v4.0.0", "checkout@v4.2.0"),
        || format!("file after updateFile: {written}"),
    )?;

    let again = update_file(client, args)?;
    let a = body(&again);
    expect_eq(&a["status"], &json!("upToDate"), "second call status")?;
    expect_eq(&a["changes"], &json!([]), "second call changes")?;
    expect_eq(&a["written"], &json!([]), "second call written")?;
    expect_eq(&a["diff"], &json!(""), "second call diff")?;
    expect_eq(
        &a["unchanged"][0]["ref"],
        &json!("v4.2.0"),
        "second call unchanged",
    )?;
    let after = ws.read(".github/workflows/ci.yml")?;
    expect(after == written, || "second call rewrote the file".into())
}

fn update_file_dry_run_and_refusals_write_nothing(
    client: &mut Client,
    ws: &Workspace,
) -> TestResult {
    let dry = update_file(
        client,
        json!({
            "path": ".github/workflows/ci.yml",
            "edits": [{ "line": 8, "action": "actions/setup-node", "ref": "v5" }],
            "dryRun": true,
        }),
    )?;
    let d = body(&dry);
    expect_eq(&d["dryRun"], &json!(true), "dryRun")?;
    expect_eq(&d["written"], &json!([]), "dry run written")?;
    expect_eq(&d["changes"][0]["to"], &json!("v5"), "dry run change")?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "dry run wrote the file".into()
    })?;

    let atomic = update_file(
        client,
        json!({
            "path": ".github/workflows/ci.yml",
            "edits": [
                { "line": 8, "action": "actions/setup-node", "ref": "v5" },
                { "line": 8, "action": "actions/checkout", "ref": "v5" },
            ],
        }),
    )?;
    expect(is_error(&atomic), || {
        format!("edit naming no `uses:` must fail: {atomic}")
    })?;
    expect(
        error_text(&atomic).contains("0 `uses:` value(s) name `actions/checkout`"),
        || error_text(&atomic),
    )?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "a refused call wrote the file".into()
    })?;

    let overlapping = update_file(
        client,
        json!({
            "path": ".github/workflows/ci.yml",
            "edits": [
                { "line": 7, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 8, "action": "actions/setup-node", "ref": "v5" },
                { "line": 8, "action": "actions/setup-node", "ref": "v6" },
            ],
        }),
    )?;
    expect(is_error(&overlapping), || {
        format!("overlapping edits must fail: {overlapping}")
    })?;
    expect(
        error_text(&overlapping).contains(
            "line 7: edit skipped (Overlap); line 8: edit skipped (Overlap); line 8: edit skipped (Overlap); nothing was written",
        ),
        || error_text(&overlapping),
    )?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "overlapping edits wrote the file".into()
    })?;

    let broken = format!("{WORKFLOW}  bad: [unclosed\n");
    ws.write(".github/workflows/broken.yml", &broken)?;
    let edit = json!([{ "line": 7, "action": "actions/checkout", "ref": "v4.2.0" }]);
    let validated = update_file(
        client,
        json!({ "path": ".github/workflows/broken.yml", "edits": edit }),
    )?;
    expect(is_error(&validated), || {
        format!("invalid YAML must be refused: {validated}")
    })?;
    expect(ws.read(".github/workflows/broken.yml")? == broken, || {
        "validateYaml wrote the file".into()
    })?;
    let unvalidated = update_file(
        client,
        json!({ "path": ".github/workflows/broken.yml", "edits": edit, "validateYaml": false }),
    )?;
    expect(!is_error(&unvalidated), || {
        format!("validateYaml false must write: {unvalidated}")
    })?;
    expect(
        ws.read(".github/workflows/broken.yml")?
            == broken.replace("checkout@v4.0.0", "checkout@v4.2.0"),
        || "validateYaml false did not write".into(),
    )
}

fn update_file_refuses_escaped_values_overlaps_and_mixed_hosts(
    client: &mut Client,
    ws: &Workspace,
) -> TestResult {
    let escaped = "jobs:\n  build:\n    steps:\n      - uses: \"actions/checkout@v4.0.\\x30\"\n";
    ws.write(".github/workflows/escaped.yml", escaped)?;
    let result = update_file(
        client,
        json!({
            "path": ".github/workflows/escaped.yml",
            "edits": [{ "line": 4, "action": "actions/checkout", "ref": "v4.2.0" }],
        }),
    )?;
    expect(is_error(&result), || {
        format!("escaped value must be refused: {result}")
    })?;
    expect(
        error_text(&result).contains("line 4: `actions/checkout@v4.0.0` is escaped or spans lines"),
        || error_text(&result),
    )?;
    expect(ws.read(".github/workflows/escaped.yml")? == escaped, || {
        "a refused escaped edit wrote the file".into()
    })?;

    let overlap = update_file(
        client,
        json!({
            "path": ".github/workflows/ci.yml",
            "edits": [
                { "line": 7, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 7, "action": "actions/checkout", "ref": "v5" },
            ],
        }),
    )?;
    expect(is_error(&overlap), || {
        format!("overlapping edits must be refused: {overlap}")
    })?;
    expect(error_text(&overlap).contains("line 7"), || {
        error_text(&overlap)
    })?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "overlapping edits wrote the file".into()
    })?;

    let mixed = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.0.0\n      - uses: ghe.corp.com/actions/checkout@v4.0.0\n";
    ws.write(".github/workflows/mixed.yml", mixed)?;
    let hosts = update_file(
        client,
        json!({
            "path": ".github/workflows/mixed.yml",
            "edits": [
                { "line": 4, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 5, "action": "ghe.corp.com/actions/checkout", "ref": "v4.2.0" },
            ],
        }),
    )?;
    expect(is_error(&hosts), || {
        format!("edits on two hosts must be refused: {hosts}")
    })?;
    expect(
        error_text(&hosts)
            .contains("edits name actions on more than one host (github.com, ghe.corp.com)"),
        || error_text(&hosts),
    )?;
    expect(ws.read(".github/workflows/mixed.yml")? == mixed, || {
        "edits on two hosts wrote the file".into()
    })
}

fn update_file_refreshes_version_comments(client: &mut Client, ws: &Workspace) -> TestResult {
    let noted = "steps:\n  - uses: actions/checkout@v4.0.0 # v4.0.0 keep\n  - uses: actions/setup-node@v4 # 1st step\n";
    ws.write(".github/workflows/noted.yml", noted)?;
    let tagged = update_file(
        client,
        json!({
            "path": ".github/workflows/noted.yml",
            "edits": [
                { "line": 2, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 3, "action": "actions/setup-node", "ref": "v5" },
            ],
        }),
    )?;
    expect(!is_error(&tagged), || format!("tag edits failed: {tagged}"))?;
    let retagged = "steps:\n  - uses: actions/checkout@v4.2.0 # v4.2.0 keep\n  - uses: actions/setup-node@v5 # 1st step\n";
    expect(ws.read(".github/workflows/noted.yml")? == retagged, || {
        "tag edits did not refresh the version comment".into()
    })?;
    expect_eq(
        &body(&tagged)["changes"][0]["after"],
        &json!("  - uses: actions/checkout@v4.2.0 # v4.2.0 keep"),
        "reported line",
    )?;

    let pinned = update_file(
        client,
        json!({
            "path": ".github/workflows/noted.yml",
            "edits": [
                { "line": 2, "action": "actions/checkout", "ref": sha('b') },
                { "line": 3, "action": "actions/setup-node", "ref": sha('c') },
            ],
        }),
    )?;
    expect(!is_error(&pinned), || format!("SHA edits failed: {pinned}"))?;
    let expected = format!(
        "steps:\n  - uses: actions/checkout@{} # keep\n  - uses: actions/setup-node@{} # 1st step\n",
        sha('b'),
        sha('c')
    );
    let written = ws.read(".github/workflows/noted.yml")?;
    expect(written == expected, || {
        format!("SHA edits wrote {written:?}")
    })?;

    ws.write(
        ".github/workflows/noted.yml",
        &format!(
            "steps:\n  - uses: actions/checkout@{} # v4.0.0 keep\n",
            sha('b')
        ),
    )?;
    let unpinned = update_file(
        client,
        json!({
            "path": ".github/workflows/noted.yml",
            "edits": [{ "line": 2, "action": "actions/checkout", "ref": "v4.2.0" }],
        }),
    )?;
    expect(!is_error(&unpinned), || {
        format!("tag over a SHA pin failed: {unpinned}")
    })?;
    let written = ws.read(".github/workflows/noted.yml")?;
    expect(
        written == "steps:\n  - uses: actions/checkout@v4.2.0 # keep\n",
        || format!("tag over a SHA pin wrote {written:?}"),
    )
}

fn mutators_refuse_paths_outside_the_root(client: &mut Client, ws: &Workspace) -> TestResult {
    let outside = ws.dir.path().join("outside.yml");
    let edit = json!([{ "line": 7, "action": "actions/checkout", "ref": "v4.2.0" }]);
    let mut paths = vec![
        "../outside.yml".to_string(),
        outside.display().to_string(),
        ".github/../../outside.yml".to_string(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, ws.path(".github/workflows/escape.yml"))?;
        std::os::unix::fs::symlink(ws.dir.path(), ws.path("parent"))?;
        paths.push(".github/workflows/escape.yml".to_string());
        paths.push("parent/outside.yml".to_string());
    }
    for path in paths {
        let result = update_file(client, json!({ "path": path, "edits": edit }))?;
        expect(is_error(&result), || {
            format!("{path}: expected refusal: {result}")
        })?;
        let text = error_text(&result);
        expect(text.contains("outside the server root"), || {
            format!("{path}: {text}")
        })?;
    }
    for workspace_root in ["..", ws.dir.path().display().to_string().as_str()] {
        let result = client.call(
            "updateAllUsesToVersion",
            json!({
                "workspaceRoot": workspace_root,
                "match": { "owner": "actions", "repo": "checkout" },
                "targetRef": "v4.2.0",
            }),
        )?;
        expect(is_error(&result), || {
            format!("{workspace_root}: expected refusal: {result}")
        })?;
    }
    expect(fs::read_to_string(&outside)? == WORKFLOW, || {
        "a file outside the root changed".into()
    })
}

fn update_all_pins_shas_honours_dry_run_and_is_idempotent(
    client: &mut Client,
    ws: &Workspace,
) -> TestResult {
    let composite = format!(
        "runs:\n  using: composite\n  steps:\n    - uses: actions/checkout@{} # v4.0.0\n",
        sha('a')
    );
    ws.write(".github/actions/setup/action.yml", &composite)?;
    let args = |dry_run: bool, composite: bool| {
        json!({
            "workspaceRoot": ".",
            "match": { "owner": "actions", "repo": "checkout" },
            "targetRef": "v4.2.0",
            "includeCompositeActions": composite,
            "dryRun": dry_run,
        })
    };

    let dry = client.call("updateAllUsesToVersion", args(true, true))?;
    let d = body(&dry);
    expect(!is_error(&dry), || format!("dry run failed: {dry}"))?;
    expect_eq(&d["provider"], &json!("github"), "provider")?;
    expect_eq(&d["host"], &json!("github.com"), "host")?;
    expect_eq(&d["status"], &json!("outdated"), "dry run status")?;
    expect_eq(&d["written"], &json!([]), "dry run written")?;
    let tos: Vec<&Value> = d["changes"]
        .as_array()
        .ok_or("changes")?
        .iter()
        .map(|c| &c["to"])
        .collect();
    expect(tos == [&json!(sha('b')), &json!("v4.2.0")], || {
        format!("dry run targets: {tos:?}")
    })?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "dry run wrote ci.yml".into()
    })?;
    expect(
        ws.read(".github/actions/setup/action.yml")? == composite,
        || "dry run wrote action.yml".into(),
    )?;

    let workflows_only = client.call("updateAllUsesToVersion", args(false, false))?;
    expect_eq(
        &body(&workflows_only)["written"],
        &json!([".github/workflows/ci.yml"]),
        "workflows only",
    )?;
    expect(
        ws.read(".github/actions/setup/action.yml")? == composite,
        || "excluded composite changed".into(),
    )?;

    let all = client.call("updateAllUsesToVersion", args(false, true))?;
    expect_eq(
        &body(&all)["written"],
        &json!([".github/actions/setup/action.yml"]),
        "composite written",
    )?;
    expect(
        ws.read(".github/actions/setup/action.yml")?
            == format!(
                "runs:\n  using: composite\n  steps:\n    - uses: actions/checkout@{} # v4.2.0\n",
                sha('b')
            ),
        || "composite was not re-pinned with a fresh comment".into(),
    )?;
    expect(
        ws.read(".github/workflows/ci.yml")?
            == WORKFLOW.replace("checkout@v4.0.0", "checkout@v4.2.0"),
        || "workflow was not updated".into(),
    )?;

    let again = client.call("updateAllUsesToVersion", args(false, true))?;
    let a = body(&again);
    expect_eq(&a["status"], &json!("upToDate"), "second call status")?;
    expect_eq(&a["changes"], &json!([]), "second call changes")?;
    expect_eq(&a["written"], &json!([]), "second call written")?;
    expect(
        a["unchanged"].as_array().is_some_and(|u| u.len() == 2),
        || format!("second call unchanged: {}", a["unchanged"]),
    )?;

    let missing = client.call(
        "updateAllUsesToVersion",
        json!({ "workspaceRoot": ".", "match": { "owner": "actions", "repo": "checkout" }, "targetRef": "v9" }),
    )?;
    expect(is_error(&missing), || {
        format!("unknown target must fail: {missing}")
    })?;
    expect_eq(
        &body(&missing)["status"],
        &json!("unresolvable"),
        "unknown target status",
    )
}

fn update_all_comments_lines_with_a_hash_before_the_value(
    client: &mut Client,
    ws: &Workspace,
) -> TestResult {
    let workflow = |pinned: &str, comment: &str, tag: &str| {
        format!(
            "on: push\njobs:\n  build:\n    runs-on: x\n    steps:\n      - {{ name: \"Build #1\", uses: actions/checkout@{pinned} }}{comment}\n      - {{ name: \"Build #2\", uses: actions/checkout@{tag} }}\n"
        )
    };
    ws.write(
        ".github/workflows/build.yml",
        &workflow(&sha('a'), "", "v4.0.0"),
    )?;
    let all = client.call(
        "updateAllUsesToVersion",
        json!({
            "workspaceRoot": ".",
            "match": { "owner": "actions", "repo": "checkout" },
            "targetRef": "v4.2.0",
        }),
    )?;
    expect(!is_error(&all), || format!("updateAll failed: {all}"))?;
    let written = &body(&all)["written"];
    expect(
        written
            .as_array()
            .is_some_and(|w| w.contains(&json!(".github/workflows/build.yml"))),
        || format!("build.yml not written: {written}"),
    )?;
    let expected = workflow(&sha('b'), " # v4.2.0", "v4.2.0");
    let actual = ws.read(".github/workflows/build.yml")?;
    expect(actual == expected, || {
        format!("build.yml:\n{actual}\nexpected:\n{expected}")
    })
}

fn mutators_read_markdown_and_text_files(client: &mut Client, ws: &Workspace) -> TestResult {
    fs::create_dir_all(ws.path("docs"))?;
    fs::create_dir_all(ws.path("infra"))?;
    let md = format!(
        "# Usage\n\n```yaml\n- uses: actions/checkout@{} # v4.0.0\n```\nuses: actions/checkout@v4.0.0\n",
        sha('a')
    );
    let tf = format!(
        "resource \"a\" {{\n  steps = \"[{{ uses: actions/checkout@{} # v4.0.0 }}]\"\n}}\n",
        sha('a')
    );
    ws.write("docs/usage.md", &md)?;
    ws.write("infra/main.tf", &tf)?;

    let all = client.call(
        "updateAllUsesToVersion",
        json!({
            "workspaceRoot": ".",
            "match": { "owner": "actions", "repo": "checkout" },
            "targetRef": "v4.2.0",
        }),
    )?;
    expect(!is_error(&all), || format!("updateAll failed: {all}"))?;
    let written = &body(&all)["written"];
    for path in ["docs/usage.md", "infra/main.tf"] {
        expect(
            written.as_array().is_some_and(|w| w.contains(&json!(path))),
            || format!("{path} not written: {written}"),
        )?;
    }
    expect_eq(
        &json!(ws.read("docs/usage.md")?),
        &json!(md.replace(
            &format!("{} # v4.0.0", sha('a')),
            &format!("{} # v4.2.0", sha('b'))
        )),
        "markdown block re-pinned with a fresh comment",
    )?;
    expect_eq(
        &json!(ws.read("infra/main.tf")?),
        &json!(tf.replace(&sha('a'), &sha('b'))),
        "text line re-pinned without a comment change",
    )?;

    let script = "resource \"b\" {\n  step = { uses: \"actions/setup-node@v4\" }\n}\n";
    ws.write("infra/tag.tf", script)?;
    let file = update_file(
        client,
        json!({
            "path": "infra/tag.tf",
            "edits": [{ "line": 2, "action": "actions/setup-node", "ref": "v5" }],
        }),
    )?;
    expect(!is_error(&file), || {
        format!("updateFile on text failed: {file}")
    })?;
    expect_eq(
        &json!(ws.read("infra/tag.tf")?),
        &json!(script.replace("@v4", "@v5")),
        "text file written",
    )
}

fn mutators_report_invalid_refs(client: &mut Client, ws: &Workspace) -> TestResult {
    let bad = "runs:\n  steps:\n  - uses: actions/checkout@a~1\n  - uses: actions/checkout@v1.lock\n  - uses: actions/checkout\n  - uses: actions/checkout@\n  - uses: other/repo@a~1\n  - uses: ./local\n    with:\n      uses: actions/checkout\n";
    ws.write(".github/workflows/bad.yml", bad)?;

    let all = client.call(
        "updateAllUsesToVersion",
        json!({ "workspaceRoot": ".", "match": { "owner": "actions", "repo": "checkout" }, "targetRef": "v4.2.0" }),
    )?;
    let a = body(&all);
    expect(is_error(&all), || {
        format!("invalid refs must fail the call: {all}")
    })?;
    expect_eq(&a["status"], &json!("unresolvable"), "status")?;
    let unresolvable: Vec<(&Value, &Value, &Value)> = a["unresolvable"]
        .as_array()
        .ok_or("unresolvable")?
        .iter()
        .map(|u| (&u["path"], &u["line"], &u["action"]))
        .collect();
    let path = json!(".github/workflows/bad.yml");
    expect(
        unresolvable
            == [
                (&path, &json!(3), &json!("actions/checkout@a~1")),
                (&path, &json!(4), &json!("actions/checkout@v1.lock")),
                (&path, &json!(5), &json!("actions/checkout")),
                (&path, &json!(6), &json!("actions/checkout@")),
            ],
        || format!("unresolvable: {}", a["unresolvable"]),
    )?;
    expect(
        a["unresolvable"][0]["error"]
            == json!(
                "invalid action reference 'actions/checkout@a~1': ref `a~1` holds the character '~'"
            ),
        || format!("error: {}", a["unresolvable"][0]["error"]),
    )?;
    expect_eq(
        &a["written"],
        &json!([".github/workflows/ci.yml"]),
        "valid refs still written",
    )?;
    expect(ws.read(".github/workflows/bad.yml")? == bad, || {
        "invalid refs were rewritten".into()
    })?;

    let no_at = update_file(
        client,
        json!({
            "path": ".github/workflows/bad.yml",
            "edits": [
                { "line": 3, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 5, "action": "actions/checkout", "ref": "v4.2.0" },
            ],
        }),
    )?;
    expect(is_error(&no_at), || {
        format!("an edit of a value without `@` must fail: {no_at}")
    })?;
    expect(
        error_text(&no_at).contains("line 5: `actions/checkout` has no `@`"),
        || error_text(&no_at),
    )?;
    expect(ws.read(".github/workflows/bad.yml")? == bad, || {
        "a refused call wrote the file".into()
    })?;

    let fixed = update_file(
        client,
        json!({
            "path": ".github/workflows/bad.yml",
            "edits": [
                { "line": 3, "action": "actions/checkout", "ref": "v4.2.0" },
                { "line": 4, "action": "actions/checkout", "ref": "v4.2.0" },
            ],
        }),
    )?;
    expect(!is_error(&fixed), || {
        format!("an edit of an invalid ref must apply: {fixed}")
    })?;
    let f = body(&fixed);
    let changed: Vec<(&Value, &Value, &Value)> = f["changes"]
        .as_array()
        .ok_or("changes")?
        .iter()
        .map(|c| (&c["line"], &c["from"], &c["to"]))
        .collect();
    expect(
        changed
            == [
                (&json!(3), &json!("a~1"), &json!("v4.2.0")),
                (&json!(4), &json!("v1.lock"), &json!("v4.2.0")),
            ],
        || format!("changes: {}", f["changes"]),
    )?;
    expect(
        ws.read(".github/workflows/bad.yml")?
            == bad
                .replace("checkout@a~1", "checkout@v4.2.0")
                .replace("checkout@v1.lock", "checkout@v4.2.0"),
        || "invalid refs were not replaced".into(),
    )
}

fn binary(ws: &Workspace) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_actup-mcp"));
    command
        .env("XDG_CONFIG_HOME", ws.dir.path())
        .env("HOME", ws.dir.path())
        .env("XDG_CACHE_HOME", ws.dir.path().join(".cache"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ACTUP_") {
            command.env_remove(key);
        }
    }
    command
}

fn binary_serves_the_tools_from_its_working_directory(ws: &Workspace) -> TestResult {
    let mut command = binary(ws);
    command.current_dir(ws.root());
    let mut client = Client::spawn(command)?;
    let tools = client.request("tools/list", json!({}))?;
    expect(
        tools["tools"].as_array().is_some_and(|t| t.len() == 6),
        || format!("tools/list: {tools}"),
    )?;
    let result = client.call(
        "updateFile",
        json!({
            "path": ".github/workflows/ci.yml",
            "edits": [{ "line": 7, "action": "actions/checkout", "ref": "v4.2.0" }],
            "dryRun": true,
        }),
    )?;
    expect(!is_error(&result), || {
        format!("updateFile failed: {result}")
    })?;
    expect_eq(
        &body(&result)["changes"][0]["provider"],
        &json!("github"),
        "provider",
    )?;
    expect(ws.read(".github/workflows/ci.yml")? == WORKFLOW, || {
        "dry run wrote the file".into()
    })
}

fn binary_loads_the_config_from_flag_or_env(ws: &Workspace) -> TestResult {
    let config = ws.dir.path().join("custom.toml");
    fs::write(
        &config,
        "default_host = \"ghe.corp.com\"\n[hosts.\"ghe.corp.com\"]\nkind = \"github\"\n",
    )?;
    let served_host = |mut command: Command| -> Result<Value, Box<dyn Error>> {
        command.current_dir(ws.root());
        let mut client = Client::spawn(command)?;
        let result = client.call(
            "updateFile",
            json!({
                "path": ".github/workflows/ci.yml",
                "edits": [{ "line": 7, "action": "actions/checkout", "ref": "v4.2.0" }],
                "dryRun": true,
            }),
        )?;
        Ok(body(&result)["changes"][0]["host"].clone())
    };
    let mut flag = binary(ws);
    flag.arg("--config").arg(&config);
    expect_eq(&served_host(flag)?, &json!("ghe.corp.com"), "--config")?;
    let mut env = binary(ws);
    env.env("ACTUP_CONFIG", "../custom.toml");
    expect_eq(&served_host(env)?, &json!("ghe.corp.com"), "ACTUP_CONFIG")?;
    let user = ws.dir.path().join("actup");
    fs::create_dir_all(&user)?;
    fs::write(
        user.join("config.toml"),
        "default_host = \"ghe.user.com\"\n[hosts.\"ghe.user.com\"]\nkind = \"github\"\n",
    )?;
    let mut empty = binary(ws);
    empty.env("ACTUP_CONFIG", "");
    expect_eq(
        &served_host(empty)?,
        &json!("ghe.user.com"),
        "empty ACTUP_CONFIG",
    )?;

    let output = binary(ws)
        .arg("--root")
        .arg(ws.root())
        .arg("-c")
        .arg(ws.dir.path().join("missing.toml"))
        .stdin(Stdio::null())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    expect(
        output.status.code() == Some(2) && stderr.contains("missing.toml"),
        || format!("missing config: {:?} {stderr}", output.status.code()),
    )
}

fn binary_exits_on_a_bad_root_config_or_host(ws: &Workspace) -> TestResult {
    let run = |root: &Path| -> Result<(Option<i32>, String), Box<dyn Error>> {
        let output = binary(ws)
            .arg("--root")
            .arg(root)
            .stdin(Stdio::null())
            .output()?;
        Ok((
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    };
    let (code, stderr) = run(&ws.path("missing"))?;
    expect(code == Some(2) && stderr.contains("invalid root"), || {
        format!("missing root: {code:?} {stderr}")
    })?;
    fs::write(ws.dir.path().join("actup.toml"), "policy = 3\n")?;
    let (code, stderr) = run(&ws.root())?;
    expect(code == Some(0), || {
        format!("config above the root: {code:?} {stderr}")
    })?;
    ws.write("actup.toml", "policy = 3\n")?;
    let (code, stderr) = run(&ws.root())?;
    expect(code == Some(2) && stderr.contains("actup.toml"), || {
        format!("invalid config: {code:?} {stderr}")
    })?;
    ws.write(
        "actup.toml",
        "[hosts.\"ghe.corp.com\"]\ntoken_env = \"GHE_TOKEN\"\n",
    )?;
    let (code, stderr) = run(&ws.root())?;
    expect(
        code == Some(3) && stderr.contains("cannot initialize providers"),
        || format!("host without kind: {code:?} {stderr}"),
    )
}

type Case = fn(&mut Client, &Workspace) -> TestResult;

type BinaryCase = fn(&Workspace) -> TestResult;

const BINARY_CASES: [(&str, BinaryCase); 4] = [
    (
        "latest_names_ref_kind_and_same_commit_target",
        latest_names_ref_kind_and_same_commit_target,
    ),
    (
        "binary_serves_the_tools_from_its_working_directory",
        binary_serves_the_tools_from_its_working_directory,
    ),
    (
        "binary_loads_the_config_from_flag_or_env",
        binary_loads_the_config_from_flag_or_env,
    ),
    (
        "binary_exits_on_a_bad_root_config_or_host",
        binary_exits_on_a_bad_root_config_or_host,
    ),
];

const CASES: [(&str, Case); 13] = [
    (
        "tools_list_marks_mutators_destructive",
        tools_list_marks_mutators_destructive,
    ),
    (
        "read_tools_report_provider_host_and_status",
        read_tools_report_provider_host_and_status,
    ),
    (
        "read_tools_classify_provider_failures",
        read_tools_classify_provider_failures,
    ),
    (
        "invalid_arguments_are_tool_errors",
        invalid_arguments_are_tool_errors,
    ),
    (
        "update_file_returns_ranges_diff_and_reversal",
        update_file_returns_ranges_diff_and_reversal,
    ),
    (
        "update_file_dry_run_and_refusals_write_nothing",
        update_file_dry_run_and_refusals_write_nothing,
    ),
    (
        "update_file_refuses_escaped_values_overlaps_and_mixed_hosts",
        update_file_refuses_escaped_values_overlaps_and_mixed_hosts,
    ),
    (
        "mutators_refuse_paths_outside_the_root",
        mutators_refuse_paths_outside_the_root,
    ),
    (
        "update_all_pins_shas_honours_dry_run_and_is_idempotent",
        update_all_pins_shas_honours_dry_run_and_is_idempotent,
    ),
    (
        "update_all_comments_lines_with_a_hash_before_the_value",
        update_all_comments_lines_with_a_hash_before_the_value,
    ),
    (
        "mutators_read_markdown_and_text_files",
        mutators_read_markdown_and_text_files,
    ),
    ("mutators_report_invalid_refs", mutators_report_invalid_refs),
    (
        "update_file_refreshes_version_comments",
        update_file_refreshes_version_comments,
    ),
];

fn run(case: Case) -> TestResult {
    let ws = Workspace::new()?;
    let mut client = Client::start(&ws.root())?;
    case(&mut client, &ws)
}

fn run_binary(case: BinaryCase) -> TestResult {
    case(&Workspace::new()?)
}

fn main() -> ExitCode {
    if let Some(root) = std::env::var_os(CHILD_ENV) {
        return serve_child(Path::new(&root));
    }
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let selected = |name: &str| filter.as_deref().is_none_or(|f| name.contains(f));
    let results = CASES
        .into_iter()
        .filter(|(name, _)| selected(name))
        .map(|(name, case)| (name, run(case)))
        .chain(
            BINARY_CASES
                .into_iter()
                .filter(|(name, _)| selected(name))
                .map(|(name, case)| (name, run_binary(case))),
        );
    let mut failed = 0;
    for (name, result) in results {
        match result {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED\n  {e}");
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        println!("{failed} contract test(s) failed");
        ExitCode::FAILURE
    }
}
