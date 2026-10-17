//! End-to-end tests that run the `actup` binary against a mocked REST API.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const HOST: &str = "actup-e2e.invalid";
const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const HEAD: &str = "3333333333333333333333333333333333333333";

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn remote() -> MockServer {
    let server = MockServer::start().await;
    for repo in ["old", "new"] {
        let tags = serde_json::json!([
            { "name": "v1.1.0", "commit": { "sha": NEW } },
            { "name": "v1.0.0", "commit": { "sha": OLD } },
        ]);
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/{repo}/tags")))
            .respond_with(ResponseTemplate::new(200).set_body_json(tags))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/{repo}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "full_name": format!("o/{repo}"), "default_branch": "main" }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/{repo}/branches/main")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "name": "main", "commit": { "sha": HEAD } }),
                ),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/repos/o/limited/tags"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(serde_json::json!({ "message": "rate limited" })),
        )
        .mount(&server)
        .await;
    server
}

struct Repo {
    dir: TempDir,
    config: PathBuf,
}

impl Repo {
    fn new(server: &MockServer, steps: &[&str]) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let workflows = dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows)?;
        let uses: Vec<String> = steps.iter().map(|s| format!("  - uses: {s}")).collect();
        fs::write(
            workflows.join("ci.yml"),
            format!("steps:\n{}\n", uses.join("\n")),
        )?;
        let config = dir.path().join("e2e.toml");
        fs::write(
            &config,
            format!(
                "default_host = \"{HOST}\"\n\n[cache]\nenabled = false\n\n[hosts.\"{HOST}\"]\nkind = \"github\"\napi_base = \"{}\"\ntoken_env = \"ACTUP_E2E_NO_TOKEN\"\napi_mode = \"rest\"\n",
                server.uri()
            ),
        )?;
        Ok(Self { dir, config })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn workflow(&self) -> Result<String, std::io::Error> {
        fs::read_to_string(self.path().join(".github/workflows/ci.yml"))
    }

    async fn run(&self, args: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
        let mut cmd = actup(self.path());
        cmd.current_dir(self.path())
            .arg("--config")
            .arg(&self.config)
            .args(args);
        Ok(tokio::task::spawn_blocking(move || cmd.output()).await??)
    }
}

fn actup(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_actup"));
    cmd.env("XDG_CONFIG_HOME", home)
        .env("XDG_CACHE_HOME", home)
        .env("HOME", home)
        .env_remove("GITHUB_TOKEN")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("COLUMNS");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ACTUP_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn stdout(output: &Output) -> Result<String, std::string::FromUtf8Error> {
    String::from_utf8(output.stdout.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn check_exits_0_and_reports_every_format() -> TestResult {
    let server = remote().await;
    let repo = Repo::new(&server, &["o/old@v1.0.0", "o/new@v1.1.0"])?;

    let human = repo.run(&["check", "."]).await?;
    assert_eq!(human.status.code(), Some(0), "{human:?}");
    let text = stdout(&human)?;
    assert!(
        text.contains("o/old@v1.0.0  v1.0.0 (tag) -> v1.1.0"),
        "{text}"
    );
    assert!(text.contains("o/new@v1.1.0  up to date"), "{text}");
    assert!(text.contains("Scanned 2 actions: 1 up to date"), "{text}");

    let json = repo.run(&["check", ".", "--format", "json"]).await?;
    assert_eq!(json.status.code(), Some(0));
    let findings: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let statuses: Vec<(&str, &str, &str)> = findings
        .as_array()
        .ok_or("findings are no array")?
        .iter()
        .map(|f| {
            (
                f["repo"].as_str().unwrap_or_default(),
                f["status"].as_str().unwrap_or_default(),
                f["host"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [("old", "outdated", HOST), ("new", "up_to_date", HOST)]
    );
    assert_eq!(findings[0]["latest_sha"], NEW);

    let verbose = repo
        .run(&["--verbose", "check", ".", "--format", "json"])
        .await?;
    assert_eq!(verbose.status.code(), Some(0));
    let verbose_findings: serde_json::Value = serde_json::from_slice(&verbose.stdout)?;
    assert_eq!(verbose_findings, findings);
    let log = String::from_utf8(verbose.stderr)?;
    assert!(log.contains("DEBUG"), "{log}");

    let toml = repo.run(&["check", ".", "--format", "toml"]).await?;
    assert_eq!(toml.status.code(), Some(0));
    let parsed: toml::Value = toml::from_str(&stdout(&toml)?)?;
    let first = &parsed["findings"][0];
    assert_eq!(first["status"].as_str(), Some("outdated"));
    assert_eq!(first["latest_version"].as_str(), Some("v1.1.0"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn check_strict_exits_1_on_an_outdated_ref() -> TestResult {
    let server = remote().await;
    let repo = Repo::new(&server, &["o/old@v1.0.0"])?;
    let output = repo.run(&["check", ".", "--strict"]).await?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let clean = Repo::new(&server, &["o/new@v1.1.0"])?;
    assert_eq!(
        clean.run(&["check", ".", "--strict"]).await?.status.code(),
        Some(0)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn check_exits_2_on_a_missing_tag_and_bad_input() -> TestResult {
    let server = remote().await;
    let repo = Repo::new(&server, &["o/old@v9.9.9"])?;
    let output = repo.run(&["check", ".", "--format", "json"]).await?;
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let findings: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(findings[0]["status"], "unresolvable");

    let usage = repo.run(&["check", "--format", "yaml"]).await?;
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8(usage.stderr)?.contains("invalid value 'yaml'"));

    fs::write(&repo.config, "[policy]\nbump = \"sideways\"\n")?;
    let config = repo.run(&["check", "."]).await?;
    assert_eq!(config.status.code(), Some(2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn check_exits_3_on_rate_limits_and_unserved_hosts() -> TestResult {
    let server = remote().await;
    let limited = Repo::new(&server, &["o/limited@v1"])?;
    let output = limited.run(&["check", ".", "--format", "json"]).await?;
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let findings: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(findings[0]["status"], "error");

    let unserved = Repo::new(&server, &["other.invalid/o/r@v1"])?;
    let output = unserved.run(&["check", "."]).await?;
    assert_eq!(output.status.code(), Some(3));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn update_writes_and_dry_run_exits_1() -> TestResult {
    let server = remote().await;
    let repo = Repo::new(&server, &["o/old@v1.0.0", "o/new@v1.1.0"])?;
    let before = repo.workflow()?;

    let dry = repo
        .run(&["update", ".", "--dry-run", "--format", "json"])
        .await?;
    assert_eq!(dry.status.code(), Some(1), "{dry:?}");
    let report: serde_json::Value = serde_json::from_slice(&dry.stdout)?;
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["edited"][0]["to"], "v1.1.0");
    assert_eq!(repo.workflow()?, before);

    let applied = repo.run(&["update", "."]).await?;
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert!(stdout(&applied)?.contains("Updated 1 action reference(s)"));
    assert_eq!(repo.workflow()?, before.replace("old@v1.0.0", "old@v1.1.0"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pin_writes_shas_and_reports_toml() -> TestResult {
    let server = remote().await;
    let repo = Repo::new(&server, &["o/old@v1.0.0"])?;

    let output = repo.run(&["pin", ".", "--format", "toml"]).await?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: toml::Value = toml::from_str(&stdout(&output)?)?;
    assert_eq!(report["edited"][0]["to"].as_str(), Some(OLD));
    assert_eq!(
        repo.workflow()?,
        format!("steps:\n  - uses: o/old@{OLD} # v1.0.0\n")
    );
    Ok(())
}

#[test]
fn help_matches_the_snapshot() -> TestResult {
    let home = TempDir::new()?;
    let output = actup(home.path()).arg("--help").output()?;
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8(output.stdout)?;
    let expected = include_str!("fixtures/help.txt").replace("\r\n", "\n");
    assert_eq!(help, expected);
    Ok(())
}
