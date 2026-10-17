//! Zed editor extension for actup — provides LSP integration and the
//! `/actions:update` slash command with embedded actup-core WASM logic.
use zed_extension_api::{self as zed};

mod provider;

use std::{collections::HashMap, env, fmt::Write as _, fs, path::Path};

use actup_core::{
    ActionRef, CachingProvider, CheckResult, CheckStatus, Config, ConfigSource, RefKind, Uses,
    UsesLocation,
    config::{EnvOverrides, repo_candidates},
    extract_uses_from_yaml,
};
use zed::{SlashCommand, SlashCommandOutput, SlashCommandOutputSection, settings::LspSettings};

/// Known workflow file paths to scan (relative to worktree root).
const WORKFLOW_GLOBS: &[&str] = &[".github/workflows"];

/// Known action file names.
const ACTION_FILES: &[&str] = &["action.yml", "action.yaml"];

/// GitHub Actions language server npm package.
const ACTIONS_LANGUAGE_SERVER_PACKAGE: &str = "@actions/languageserver";

/// GitHub Actions language server executable name.
const ACTIONS_LANGUAGE_SERVER_BINARY: &str = "actions-languageserver";

/// GitHub Actions language server package bin path after npm install.
const ACTIONS_LANGUAGE_SERVER_BIN_PATH: &str =
    "node_modules/@actions/languageserver/bin/actions-languageserver";

/// Zed extension entry point. Delegates to `actup-lsp` for language intelligence
/// and embeds actup-core for the `/actions:update` slash command.
struct ActupExtension;

impl zed::Extension for ActupExtension {
    fn new() -> Self {
        Self
    }

    fn run_slash_command(
        &self,
        command: SlashCommand,
        _args: Vec<String>,
        worktree: Option<&zed::Worktree>,
    ) -> Result<SlashCommandOutput, String> {
        match command.name.as_str() {
            "actions:update" => run_actions_check(worktree),
            cmd => Err(format!("unknown slash command: \"{cmd}\"")),
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let server_name = language_server_id.as_ref();
        let binary_settings = LspSettings::for_worktree(server_name, worktree)
            .ok()
            .and_then(|s| s.binary);

        let args = binary_settings
            .as_ref()
            .and_then(|s| s.arguments.clone())
            .unwrap_or_else(|| match server_name {
                "actions-languageserver" | "yaml-language-server" => vec!["--stdio".to_string()],
                _ => Vec::new(),
            });

        if let Some(command) = binary_settings.as_ref().and_then(|s| s.path.clone()) {
            return Ok(zed::Command {
                command,
                args,
                env: Vec::default(),
            });
        }

        if server_name == "actions-languageserver" {
            return actions_language_server_command(language_server_id, worktree, args);
        }

        let binary_name = match server_name {
            "yaml-language-server" => "yaml-language-server",
            _ => "actup-lsp",
        };
        let settings_key = format!("lsp.{server_name}.binary.path");
        let command = worktree.which(binary_name).ok_or_else(|| {
            format!(
                "`{binary_name}` not found in PATH. \
                 Set binary path in Zed settings under {settings_key}"
            )
        })?;

        Ok(zed::Command {
            command,
            args,
            env: Vec::default(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.initialization_options))
    }

    fn language_server_workspace_configuration(
        &mut self,
        server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.settings))
    }
}

mod extension_entrypoint {
    #![allow(missing_docs)]

    use zed_extension_api as zed;

    use super::ActupExtension;

    zed::register_extension!(ActupExtension);
}

/// Return the command for GitHub's Actions language server.
fn actions_language_server_command(
    language_server_id: &zed::LanguageServerId,
    worktree: &zed::Worktree,
    args: Vec<String>,
) -> zed::Result<zed::Command> {
    if let Some(command) = worktree.which(ACTIONS_LANGUAGE_SERVER_BINARY) {
        return Ok(zed::Command {
            command,
            args,
            env: Vec::default(),
        });
    }

    let server_path = ensure_actions_language_server_installed(language_server_id)?;
    let mut node_args = Vec::with_capacity(args.len() + 1);
    node_args.push(server_path);
    node_args.extend(args);

    Ok(zed::Command {
        command: zed::node_binary_path()?,
        args: node_args,
        env: Vec::default(),
    })
}

/// Install `@actions/languageserver` from npm if the package bin is absent or stale.
fn ensure_actions_language_server_installed(
    language_server_id: &zed::LanguageServerId,
) -> zed::Result<String> {
    let server_exists = actions_language_server_bin_exists();

    zed::set_language_server_installation_status(
        language_server_id,
        &zed::LanguageServerInstallationStatus::CheckingForUpdate,
    );
    let latest_version = match zed::npm_package_latest_version(ACTIONS_LANGUAGE_SERVER_PACKAGE) {
        Ok(version) => version,
        Err(_) if server_exists => return actions_language_server_bin_path(),
        Err(error) => return Err(error),
    };

    let installed_version = zed::npm_package_installed_version(ACTIONS_LANGUAGE_SERVER_PACKAGE)?;
    if !server_exists || installed_version.as_ref() != Some(&latest_version) {
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
        match zed::npm_install_package(ACTIONS_LANGUAGE_SERVER_PACKAGE, &latest_version) {
            Ok(()) => {}
            Err(_) if actions_language_server_bin_exists() => {
                return actions_language_server_bin_path();
            }
            Err(error) => return Err(error),
        }
    }

    if !actions_language_server_bin_exists() {
        return Err(format!(
            "installed package `{ACTIONS_LANGUAGE_SERVER_PACKAGE}` did not contain expected path `{ACTIONS_LANGUAGE_SERVER_BIN_PATH}`"
        ));
    }

    actions_language_server_bin_path()
}

fn actions_language_server_bin_exists() -> bool {
    fs::metadata(ACTIONS_LANGUAGE_SERVER_BIN_PATH).is_ok_and(|stat| stat.is_file())
}

fn actions_language_server_bin_path() -> zed::Result<String> {
    let path = env::current_dir()
        .map_err(|error| format!("failed to resolve extension directory: {error}"))?
        .join(ACTIONS_LANGUAGE_SERVER_BIN_PATH);

    path.to_str()
        .map(ToString::to_string)
        .ok_or_else(|| "actions-languageserver path is not valid UTF-8".to_string())
}

/// Run the `/actions:update` slash command using embedded actup-core.
///
/// Reads workflow files from the worktree, parses `uses:` refs,
/// fetches version info via Zed's HTTP client, and returns a summary.
fn run_actions_check(worktree: Option<&zed::Worktree>) -> Result<SlashCommandOutput, String> {
    let worktree = worktree.ok_or("no worktree available")?;

    let shell_env = worktree.shell_env();
    let config_path = LspSettings::for_worktree("actup-lsp", worktree)
        .ok()
        .and_then(|settings| settings.initialization_options)
        .and_then(|options| config_path_option(&options))
        .map(|path| {
            worktree_relative(
                PathStyle::of(zed::current_platform().0),
                &worktree.root_path(),
                &path,
            )
        })
        .transpose()?;
    let config = read_config(
        |path| worktree.read_text_file(path),
        &shell_env,
        config_path.as_ref(),
    )?;

    // Discover and read workflow files
    let files = discover_workflow_files(worktree);
    if files.is_empty() {
        return Ok(no_files_output(&config));
    }

    let Collected {
        refs: all_refs,
        mut results,
    } = collect_refs(&files);
    let ref_count = all_refs.len() + results.len();
    if ref_count == 0 {
        return Ok(no_refs_output(&config, files.len()));
    }

    let unique_repos = plan_fetches(all_refs.iter().map(|(_, _, action_ref)| action_ref));

    // Resolve GitHub token from shell environment
    let token = shell_env
        .iter()
        .find(|(k, _)| k == "GITHUB_TOKEN")
        .map(|(_, v)| v.clone());
    let http = provider::ZedHttp::new(token.clone());

    let fetched: HashMap<RepoKey, actup_core::Result<actup_core::version::Set>> = unique_repos
        .iter()
        .map(|(key, fetch)| {
            let versions = provider::fetch_version_set(
                &http,
                &key.0,
                &fetch.owner,
                &fetch.repo,
                &fetch.api_base,
                provider::MAX_PAGES,
                fetch.with_branches,
            );
            (key.clone(), versions)
        })
        .collect();

    let mut providers: HashMap<String, CachingProvider<provider::RestProvider<provider::ZedHttp>>> =
        HashMap::new();
    let (checked, fetch_errors) = collect_results(&all_refs, &fetched, |action_ref, versions| {
        let host = repo_key(action_ref).0;
        let lookup = providers.entry(host.clone()).or_insert_with(|| {
            CachingProvider::new(provider::RestProvider::new(
                provider::ZedHttp::new(token.clone()),
                api_base_for_host(&host),
                host,
            ))
        });
        provider::check(action_ref, versions, &config, lookup)
    });
    results.extend(checked);

    // Format output
    let text = format_check_output(&config, &results, &fetch_errors, files.len(), ref_count);
    Ok(slash_output(text, "actup: Check Results"))
}

type Located = (String, UsesLocation, CheckResult);

fn collect_results(
    all_refs: &[(String, UsesLocation, ActionRef)],
    fetched: &HashMap<RepoKey, actup_core::Result<actup_core::version::Set>>,
    mut check: impl FnMut(&ActionRef, &actup_core::version::Set) -> CheckResult,
) -> (Vec<Located>, Vec<(String, String)>) {
    let results = all_refs
        .iter()
        .filter_map(|(path, uses_loc, action_ref)| {
            let result = match fetched.get(&repo_key(action_ref))? {
                Ok(versions) => check(action_ref, versions),
                Err(e) if e.is_user_error() => CheckResult::new(
                    action_ref.clone(),
                    CheckStatus::Unresolvable {
                        reason: actup_core::error::chain(e),
                        is_environment_error: false,
                    },
                ),
                Err(_) => return None,
            };
            Some((path.clone(), uses_loc.clone(), result))
        })
        .collect();
    let mut fetch_errors: Vec<(String, String)> = fetched
        .iter()
        .filter_map(|(key, versions)| match versions {
            Err(e) if !e.is_user_error() => Some((
                format!("{}/{}/{}", key.0, key.1, key.2),
                actup_core::error::chain(e),
            )),
            _ => None,
        })
        .collect();
    fetch_errors.sort();
    (results, fetch_errors)
}

struct Collected {
    refs: Vec<(String, UsesLocation, ActionRef)>,
    results: Vec<(String, UsesLocation, CheckResult)>,
}

fn collect_refs(files: &[(String, String)]) -> Collected {
    let mut collected = Collected {
        refs: Vec::new(),
        results: Vec::new(),
    };
    for (path, content) in files {
        for loc in extract_uses_from_yaml(content) {
            match loc.classify() {
                None | Some(Uses::Local) => {}
                Some(Uses::Action(parsed)) => collected.refs.push((path.clone(), loc, parsed)),
                Some(Uses::Invalid { action, error }) => {
                    let result = CheckResult::new(
                        action,
                        CheckStatus::Unresolvable {
                            reason: error.to_string(),
                            is_environment_error: false,
                        },
                    );
                    collected.results.push((path.clone(), loc, result));
                }
            }
        }
    }
    collected
}

struct RepoFetch {
    owner: String,
    repo: String,
    api_base: String,
    with_branches: bool,
}

type RepoKey = (String, String, String);

fn repo_key(action_ref: &ActionRef) -> RepoKey {
    (
        action_ref.host().unwrap_or("github.com").to_string(),
        action_ref.owner().to_string(),
        action_ref.repo().to_string(),
    )
}

fn plan_fetches<'a>(refs: impl IntoIterator<Item = &'a ActionRef>) -> HashMap<RepoKey, RepoFetch> {
    let mut unique_repos: HashMap<RepoKey, RepoFetch> = HashMap::new();
    for action_ref in refs {
        let key = repo_key(action_ref);
        let entry = unique_repos
            .entry(key.clone())
            .or_insert_with(|| RepoFetch {
                owner: key.1,
                repo: key.2,
                api_base: api_base_for_host(&key.0),
                with_branches: false,
            });
        entry.with_branches |= names_branch(action_ref);
    }
    unique_repos
}

fn names_branch(action_ref: &ActionRef) -> bool {
    !action_ref.is_sha() && !actup_core::version::Version::parse(action_ref.ref_str()).is_semver()
}

fn slash_output(text: String, label: &str) -> SlashCommandOutput {
    SlashCommandOutput {
        sections: vec![SlashCommandOutputSection {
            range: (0..text.len()).into(),
            label: label.to_string(),
        }],
        text,
    }
}

fn config_note(config: &Config) -> String {
    let source = if config.sources.is_empty() {
        "defaults".to_string()
    } else {
        config
            .sources
            .iter()
            .map(|source| match source {
                ConfigSource::Env { vars } => {
                    let vars: Vec<String> = vars.iter().map(|var| format!("`{var}`")).collect();
                    format!("environment {}", vars.join(", "))
                }
                ConfigSource::Repo { path, ignored } if !ignored.is_empty() => {
                    let ignored: Vec<String> =
                        ignored.iter().map(|key| format!("`{key}`")).collect();
                    format!("`{}` (ignored {})", path.display(), ignored.join(", "))
                }
                ConfigSource::Explicit { path } => format!("`{}` (`configPath`)", path.display()),
                ConfigSource::User { path } | ConfigSource::Repo { path, .. } => {
                    format!("`{}`", path.display())
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("Config: {source}. Parent-directory configs and the user config are not applied.\n\n")
}

fn no_files_output(config: &Config) -> SlashCommandOutput {
    let text = format!(
        "{}No workflow or action files found in this workspace.\n\
         Looked for `.github/workflows/*.yml` and `action.yml`.\n",
        config_note(config)
    );
    slash_output(text, "actup: No files found")
}

fn no_refs_output(config: &Config, file_count: usize) -> SlashCommandOutput {
    let text = format!(
        "{}Found {file_count} workflow file(s), but no remote `uses:` references.\n",
        config_note(config)
    );
    slash_output(text, "actup: No refs found")
}

/// Map a host to a GitHub-compatible REST API base URL.
///
/// `github.com` → `https://api.github.com`. Any other host is assumed to be
/// GitHub Enterprise Server with the standard `/api/v3` suffix. Hosts that
/// require a different scheme (GitLab, Gitea) are not yet supported here —
/// the slash command will report fetch errors for them, and the user should
/// run `actup` from the CLI for non-GitHub providers.
fn api_base_for_host(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

fn config_path_option(options: &zed::serde_json::Value) -> Option<String> {
    let path = options.get("configPath")?.as_str()?;
    (!path.trim().is_empty()).then(|| path.to_owned())
}

#[derive(Debug, Clone, Copy)]
enum PathStyle {
    Posix,
    Windows,
}

impl PathStyle {
    const fn of(os: zed::Os) -> Self {
        match os {
            zed::Os::Windows => Self::Windows,
            zed::Os::Mac | zed::Os::Linux => Self::Posix,
        }
    }

    const fn is_separator(self, c: char) -> bool {
        c == '/' || (matches!(self, Self::Windows) && c == '\\')
    }

    fn is_absolute(self, path: &str) -> bool {
        match self {
            Self::Posix => path.starts_with('/'),
            Self::Windows => {
                let bytes = path.as_bytes();
                path.starts_with(['/', '\\'])
                    || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
            }
        }
    }

    fn same_name(self, a: &str, b: &str) -> bool {
        match self {
            Self::Posix => a == b,
            Self::Windows => a.eq_ignore_ascii_case(b),
        }
    }

    fn components(self, path: &str) -> Option<Vec<&str>> {
        let mut parts = Vec::new();
        for part in path.split(|c| self.is_separator(c)) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                _ => parts.push(part),
            }
        }
        Some(parts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreePath(String);

fn worktree_relative(
    style: PathStyle,
    root: &str,
    config_path: &str,
) -> Result<WorktreePath, String> {
    let outside = || format!("configPath `{config_path}` is outside the worktree `{root}`");
    let parts = style.components(config_path).ok_or_else(outside)?;
    let parts = if style.is_absolute(config_path) {
        let root_parts = style.components(root).ok_or_else(outside)?;
        let inside = parts.len() >= root_parts.len()
            && root_parts
                .iter()
                .zip(&parts)
                .all(|(root_part, part)| style.same_name(root_part, part));
        if !inside {
            return Err(outside());
        }
        parts[root_parts.len()..].to_vec()
    } else {
        parts
    };
    if parts.is_empty() {
        return Err(format!(
            "configPath `{config_path}` names the worktree root `{root}`"
        ));
    }
    Ok(WorktreePath(parts.join("/")))
}

fn file_parts((path, contents): &(String, String)) -> (&Path, &str) {
    (Path::new(path.as_str()), contents.as_str())
}

fn read_config(
    read: impl Fn(&str) -> Result<String, String>,
    shell_env: &[(String, String)],
    config_path: Option<&WorktreePath>,
) -> Result<Config, String> {
    let env = EnvOverrides::from_vars(shell_env.iter().map(|(name, value)| (name, value)));
    let found = repo_candidates().find_map(|candidate| {
        let path = candidate.relative();
        read(&path).ok().map(|contents| (path, contents))
    });
    let (discovered, explicit) = match (found, config_path) {
        (Some(found), Some(WorktreePath(path))) if found.0 == *path => (None, Some(found)),
        (found, None) => (found, None),
        (found, Some(WorktreePath(path))) => {
            let contents =
                read(path).map_err(|e| format!("cannot read configPath `{path}`: {e}"))?;
            (found, Some((path.clone(), contents)))
        }
    };
    Config::from_repo_and_explicit(
        discovered.as_ref().map(file_parts),
        explicit.as_ref().map(file_parts),
        env,
    )
    .map_err(|e| actup_core::error::chain(&e))
}

/// Discover workflow and action files from the worktree.
///
/// Since Zed doesn't have directory listing, we try known paths.
fn discover_workflow_files(worktree: &zed::Worktree) -> Vec<(String, String)> {
    let mut files = Vec::new();

    // Try common workflow file names
    for dir in WORKFLOW_GLOBS {
        for name in &[
            "ci.yml",
            "ci.yaml",
            "build.yml",
            "build.yaml",
            "test.yml",
            "test.yaml",
            "release.yml",
            "release.yaml",
            "deploy.yml",
            "deploy.yaml",
            "lint.yml",
            "lint.yaml",
            "pr.yml",
            "pr.yaml",
            "main.yml",
            "main.yaml",
            "push.yml",
            "push.yaml",
            "pull_request.yml",
            "pull_request.yaml",
            "publish.yml",
            "publish.yaml",
            "check.yml",
            "check.yaml",
            "codeql.yml",
            "codeql.yaml",
            "dependabot.yml",
            "dependabot.yaml",
            "docker.yml",
            "docker.yaml",
            "pages.yml",
            "pages.yaml",
            "security.yml",
            "security.yaml",
            "stale.yml",
            "stale.yaml",
            "greetings.yml",
            "greetings.yaml",
            "labeler.yml",
            "labeler.yaml",
        ] {
            let path = format!("{dir}/{name}");
            if let Ok(content) = worktree.read_text_file(&path) {
                files.push((path, content));
            }
        }
    }

    // Try action files at root and common locations
    for name in ACTION_FILES {
        if let Ok(content) = worktree.read_text_file(name) {
            files.push((name.to_string(), content));
        }
    }

    // Try .github/actions/*/action.yml pattern (common composite actions)
    for action_dir in &[
        "setup", "build", "test", "deploy", "lint", "cache", "notify", "cleanup",
    ] {
        for ext in ACTION_FILES {
            let path = format!(".github/actions/{action_dir}/{ext}");
            if let Ok(content) = worktree.read_text_file(&path) {
                files.push((path, content));
            }
        }
    }

    files
}

/// Render the "Pin" section: branches and non-semver tags to pin to their commit SHA.
fn write_pin_branch_section(out: &mut String, pin_branch: &[&(String, UsesLocation, CheckResult)]) {
    if pin_branch.is_empty() {
        return;
    }
    out.push_str("### Pin\n\n");
    for (path, uses_loc, result) in pin_branch {
        if let CheckStatus::PinBranch { ref_name, sha } = result.status() {
            let short = sha.get(..7).unwrap_or(sha);
            let _ = writeln!(
                out,
                "- `{path}` line {}: `{}` — pin {} `{ref_name}` to `{short}`",
                uses_loc.line(),
                uses_loc.value(),
                RefKind::noun(result.ref_kind()),
            );
        }
    }
    out.push('\n');
}

/// Format check results into human-readable output for the slash command.
fn format_check_output(
    config: &Config,
    results: &[(String, UsesLocation, CheckResult)],
    fetch_errors: &[(String, String)],
    file_count: usize,
    ref_count: usize,
) -> String {
    let mut out = config_note(config);

    let _ = writeln!(
        out,
        "Scanned {file_count} file(s), found {ref_count} remote `uses:` ref(s).\n"
    );

    // Group results by status
    let mut outdated: Vec<&(String, UsesLocation, CheckResult)> = Vec::new();
    let mut up_to_date = 0usize;
    let mut pinned = 0usize;
    let mut unresolvable: Vec<&(String, UsesLocation, CheckResult)> = Vec::new();
    let mut floating: Vec<&(String, UsesLocation, CheckResult)> = Vec::new();
    let mut pin_branch: Vec<&(String, UsesLocation, CheckResult)> = Vec::new();
    let mut ignored = 0usize;

    for r in results {
        match r.2.status() {
            CheckStatus::UpToDate => up_to_date += 1,
            CheckStatus::Outdated { .. } => outdated.push(r),
            CheckStatus::Pinned { .. } => pinned += 1,
            CheckStatus::Ignored => ignored += 1,
            CheckStatus::Floating { .. } => floating.push(r),
            CheckStatus::PinBranch { .. } => pin_branch.push(r),
            CheckStatus::Unresolvable { .. } => unresolvable.push(r),
        }
    }

    let unknown = ref_count.saturating_sub(results.len());
    let _ = writeln!(
        out,
        "**Summary:** {} outdated, {} up-to-date, {} pinned, {} floating, {} refs to pin, {} ignored, {} unknown, {} unresolvable\n",
        outdated.len(),
        up_to_date,
        pinned,
        floating.len(),
        pin_branch.len(),
        ignored,
        unknown,
        unresolvable.len(),
    );

    // Outdated refs
    if !outdated.is_empty() {
        out.push_str("### Outdated\n\n");
        for (path, uses_loc, result) in &outdated {
            if let CheckStatus::Outdated { current, target } = result.status() {
                let _ = writeln!(
                    out,
                    "- `{path}` line {}: `{}` — **{current}** → **{}**",
                    uses_loc.line(),
                    uses_loc.value(),
                    target.version(),
                );
            }
        }
        out.push('\n');
    }

    // Floating refs
    if !floating.is_empty() {
        out.push_str("### Floating\n\n");
        for (path, uses_loc, result) in &floating {
            if let CheckStatus::Floating { ref_name } = result.status() {
                let _ = writeln!(
                    out,
                    "- `{path}` line {}: `{}` — floating {} `{ref_name}`, pin to a tag or SHA",
                    uses_loc.line(),
                    uses_loc.value(),
                    RefKind::noun(result.ref_kind()),
                );
            }
        }
        out.push('\n');
    }

    write_pin_branch_section(&mut out, &pin_branch);

    // Unresolvable refs
    if !unresolvable.is_empty() {
        out.push_str("### Unresolvable\n\n");
        for (path, uses_loc, result) in &unresolvable {
            if let CheckStatus::Unresolvable { reason, .. } = result.status() {
                let _ = writeln!(
                    out,
                    "- `{path}` line {}: `{}` — {reason}",
                    uses_loc.line(),
                    uses_loc.value(),
                );
            }
        }
        out.push('\n');
    }

    // Fetch errors
    if !fetch_errors.is_empty() {
        out.push_str("### Fetch Errors\n\n");
        for (repo, err) in fetch_errors {
            let _ = writeln!(out, "- `{repo}`: {err}");
        }
        out.push('\n');
    }

    // Instructions for updating
    if !outdated.is_empty() {
        out.push_str(
            "To update these refs, edit the files above and change the version after `@`.\n\
             If `actup` CLI is available, run `actup update` for automated updates.\n",
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, path::PathBuf};

    use actup_core::{iter_config_candidates, policy};

    use super::*;

    fn read_from<'a>(
        files: &'a HashMap<&str, &str>,
        requested: &'a RefCell<Vec<String>>,
    ) -> impl Fn(&str) -> Result<String, String> + 'a {
        move |path| {
            requested.borrow_mut().push(path.to_owned());
            files
                .get(path)
                .map(|contents| (*contents).to_owned())
                .ok_or_else(|| format!("{path}: not found"))
        }
    }

    #[test]
    fn plan_fetches_lists_branches_when_any_ref_names_one() {
        let refs = [
            ActionRef::new(None, "actions", "checkout", None, "v4"),
            ActionRef::new(None, "actions", "checkout", None, "main"),
            ActionRef::new(None, "actions", "checkout", None, "v4.2.0"),
            ActionRef::new(None, "actions", "cache", None, "v4"),
            ActionRef::new(Some("ghe.corp".into()), "actions", "checkout", None, "v4"),
        ];
        let plan = plan_fetches(&refs);
        let fetch = |host: &str, repo: &str| {
            plan.get(&(host.to_string(), "actions".to_string(), repo.to_string()))
                .map(|f| (f.with_branches, f.api_base.clone()))
        };
        assert_eq!(plan.len(), 3);
        assert_eq!(
            fetch("github.com", "checkout"),
            Some((true, "https://api.github.com".to_string()))
        );
        assert_eq!(
            fetch("github.com", "cache"),
            Some((false, "https://api.github.com".to_string()))
        );
        assert_eq!(
            fetch("ghe.corp", "checkout"),
            Some((false, "https://ghe.corp/api/v3".to_string()))
        );
    }

    #[test]
    fn names_branch_skips_semver_and_sha_refs() {
        let action = |r: &str| ActionRef::new(None, "actions", "checkout", None, r);
        assert!(names_branch(&action("main")));
        assert!(names_branch(&action("release/v1")));
        assert!(!names_branch(&action("v4")));
        assert!(!names_branch(&action("v4.2.0")));
        assert!(!names_branch(&action(&"a".repeat(40))));
    }

    #[test]
    fn read_config_rejects_invalid_policy() {
        let files = HashMap::from([("actup.toml", "[policy]\nbump = \"sideways\"\n")]);
        let requested = RefCell::new(Vec::new());
        let Err(error) = read_config(read_from(&files, &requested), &[], None) else {
            panic!("invalid config must not parse");
        };
        assert!(error.contains("actup.toml"), "{error}");
        assert!(error.contains("sideways"), "{error}");
    }

    #[test]
    fn read_config_rejects_renamed_key() {
        let files = HashMap::from([(".github/actup.yaml", "update_policy: patch\n")]);
        let requested = RefCell::new(Vec::new());
        let Err(error) = read_config(read_from(&files, &requested), &[], None) else {
            panic!("renamed key must not parse");
        };
        assert!(error.contains(".github/actup.yaml"), "{error}");
        assert!(
            error.contains("`update_policy` is no longer supported; use `policy.bump`"),
            "{error}"
        );
    }

    #[test]
    fn read_config_reads_github_yaml() -> Result<(), String> {
        let files = HashMap::from([(".github/actup.yaml", "policy:\n  bump: minor\n")]);
        let requested = RefCell::new(Vec::new());
        let config = read_config(read_from(&files, &requested), &[], None)?;
        assert_eq!(config.policy.bump, policy::Update::Minor);
        assert_eq!(config.loaded_from(), Some(Path::new(".github/actup.yaml")));
        Ok(())
    }

    #[test]
    fn read_config_prefers_earlier_candidate() -> Result<(), String> {
        let files = HashMap::from([
            ("actup.toml", "[policy]\nbump = \"patch\"\n"),
            (".github/actup.yaml", "policy:\n  bump: minor\n"),
        ]);
        let requested = RefCell::new(Vec::new());
        let config = read_config(read_from(&files, &requested), &[], None)?;
        assert_eq!(config.policy.bump, policy::Update::Patch);
        assert_eq!(*requested.borrow(), ["actup.toml"]);
        Ok(())
    }

    #[test]
    fn read_config_tries_core_candidates_in_order() -> Result<(), String> {
        let files = HashMap::new();
        let requested = RefCell::new(Vec::new());
        let config = read_config(read_from(&files, &requested), &[], None)?;
        assert!(config.loaded_from().is_none());
        let requested: Vec<PathBuf> = requested.borrow().iter().map(PathBuf::from).collect();
        let core: Vec<PathBuf> = iter_config_candidates(Path::new("")).collect();
        assert_eq!(requested, core);
        assert_eq!(requested.len(), 20);
        Ok(())
    }

    fn shell_env(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn read_config_applies_shell_env_over_the_file() -> Result<(), String> {
        let files = HashMap::from([("actup.toml", "[policy]\nbump = \"minor\"\npin = \"sha\"\n")]);
        let requested = RefCell::new(Vec::new());
        let env = shell_env(&[("ACTUP_POLICY__BUMP", "patch"), ("GITHUB_TOKEN", "t")]);
        let config = read_config(read_from(&files, &requested), &env, None)?;
        assert_eq!(config.policy.bump, policy::Update::Patch);
        assert_eq!(config.policy.pin, policy::Style::Sha);
        assert_eq!(config.loaded_from(), Some(Path::new("actup.toml")));
        Ok(())
    }

    #[test]
    fn read_config_applies_shell_env_without_a_file() -> Result<(), String> {
        let files = HashMap::new();
        let requested = RefCell::new(Vec::new());
        let env = shell_env(&[("actup_policy__branch", "warn")]);
        let config = read_config(read_from(&files, &requested), &env, None)?;
        assert_eq!(config.policy.branch, policy::Branch::Warn);
        assert!(config.loaded_from().is_none());
        Ok(())
    }

    #[test]
    fn read_config_rejects_invalid_shell_env() {
        let files = HashMap::new();
        let requested = RefCell::new(Vec::new());
        let env = shell_env(&[("ACTUP_POLICY__BUMP", "sideways")]);
        let Err(error) = read_config(read_from(&files, &requested), &env, None) else {
            panic!("invalid env value must not parse");
        };
        assert!(
            error.contains("`ACTUP_` environment variable(s)"),
            "{error}"
        );
        assert!(error.contains("sideways"), "{error}");
    }

    const SCOPE: &str = "Parent-directory configs and the user config are not applied.";

    fn loaded(path: &str) -> Config {
        Config {
            sources: vec![ConfigSource::Repo {
                path: PathBuf::from(path),
                ignored: Vec::new(),
            }],
            ..Config::default()
        }
    }

    #[test]
    fn config_note_names_ignored_repo_keys() -> Result<(), String> {
        let files = HashMap::from([(
            "actup.toml",
            "default_host = \"x.example\"\n[scan]\nextra_paths = [\"a.md\"]\n",
        )]);
        let requested = RefCell::new(Vec::new());
        let config = read_config(read_from(&files, &requested), &[], None)?;
        assert_eq!(config.default_host, None);
        assert_eq!(
            config_note(&config),
            format!(
                "Config: `actup.toml` (ignored `default_host`, `scan.extra_paths`). {SCOPE}\n\n"
            )
        );
        Ok(())
    }

    #[test]
    fn config_note_names_the_file_and_environment() -> Result<(), String> {
        let files = HashMap::from([("actup.toml", "[policy]\nbump = \"minor\"\n")]);
        let requested = RefCell::new(Vec::new());
        let env = shell_env(&[("ACTUP_POLICY__PIN", "sha"), ("ACTUP_CACHE__TTL", "60")]);
        let config = read_config(read_from(&files, &requested), &env, None)?;
        assert_eq!(
            config_note(&config),
            format!(
                "Config: `actup.toml`, environment `ACTUP_CACHE__TTL`, `ACTUP_POLICY__PIN`. {SCOPE}\n\n"
            )
        );
        Ok(())
    }

    #[test]
    fn config_note_names_the_loaded_file() {
        assert_eq!(
            config_note(&loaded(".github/actup.yaml")),
            format!("Config: `.github/actup.yaml`. {SCOPE}\n\n")
        );
    }

    #[test]
    fn config_note_names_defaults_without_a_file() {
        assert_eq!(
            config_note(&Config::default()),
            format!("Config: defaults. {SCOPE}\n\n")
        );
    }

    #[test]
    fn config_note_names_the_config_path_file() {
        let explicit = ConfigSource::Explicit {
            path: PathBuf::from("ci/actup.toml"),
        };
        let only = Config {
            sources: vec![explicit.clone()],
            ..Config::default()
        };
        assert_eq!(
            config_note(&only),
            format!("Config: `ci/actup.toml` (`configPath`). {SCOPE}\n\n")
        );
        let both = Config {
            sources: vec![
                ConfigSource::Repo {
                    path: PathBuf::from("actup.toml"),
                    ignored: Vec::new(),
                },
                explicit,
            ],
            ..Config::default()
        };
        assert_eq!(
            config_note(&both),
            format!("Config: `actup.toml`, `ci/actup.toml` (`configPath`). {SCOPE}\n\n")
        );
    }

    #[test]
    fn config_path_option_reads_a_non_blank_string() {
        let option = |value| config_path_option(&value);
        assert_eq!(
            option(zed::serde_json::json!({ "configPath": " ci/actup.toml " })).as_deref(),
            Some(" ci/actup.toml ")
        );
        assert_eq!(option(zed::serde_json::json!({ "configPath": "  " })), None);
        assert_eq!(option(zed::serde_json::json!({ "configPath": 3 })), None);
        assert_eq!(option(zed::serde_json::json!({})), None);
    }

    #[test]
    fn read_config_takes_default_host_from_the_config_path_file() -> Result<(), String> {
        let files = HashMap::from([
            ("actup.toml", "[policy]\nbump = \"minor\"\npin = \"tag\"\n"),
            (
                "ci/actup.toml",
                "default_host = \"ghe.corp.example\"\n\n[policy]\npin = \"sha\"\n",
            ),
        ]);
        let requested = RefCell::new(Vec::new());
        let config_path = WorktreePath("ci/actup.toml".to_owned());
        let config = read_config(read_from(&files, &requested), &[], Some(&config_path))?;
        assert_eq!(config.default_host.as_deref(), Some("ghe.corp.example"));
        assert_eq!(config.policy.bump, policy::Update::Minor);
        assert_eq!(config.policy.pin, policy::Style::Sha);
        assert_eq!(
            config.sources,
            [
                ConfigSource::Repo {
                    path: PathBuf::from("actup.toml"),
                    ignored: Vec::new(),
                },
                ConfigSource::Explicit {
                    path: PathBuf::from("ci/actup.toml"),
                },
            ]
        );
        assert_eq!(*requested.borrow(), ["actup.toml", "ci/actup.toml"]);
        Ok(())
    }

    #[test]
    fn read_config_reads_a_discovered_config_path_file_once() -> Result<(), String> {
        let files = HashMap::from([("actup.toml", "default_host = \"ghe.corp.example\"\n")]);
        for written in [
            "actup.toml",
            "./actup.toml",
            "ci/../actup.toml",
            "/repo/actup.toml",
        ] {
            let requested = RefCell::new(Vec::new());
            let config_path = worktree_relative(PathStyle::Posix, "/repo", written)?;
            let config = read_config(read_from(&files, &requested), &[], Some(&config_path))?;
            assert_eq!(config.default_host.as_deref(), Some("ghe.corp.example"));
            assert_eq!(
                config.sources,
                [ConfigSource::Explicit {
                    path: PathBuf::from("actup.toml"),
                }],
                "{written}"
            );
            assert_eq!(*requested.borrow(), ["actup.toml"], "{written}");
        }
        Ok(())
    }

    #[test]
    fn worktree_relative_maps_paths_inside_the_worktree() -> Result<(), String> {
        let cases = [
            (
                PathStyle::Posix,
                "/home/u/repo",
                "ci/actup.toml",
                "ci/actup.toml",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "./ci//./actup.toml",
                "ci/actup.toml",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "ci/../.github/actup.yaml",
                ".github/actup.yaml",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "/home/u/repo/ci/actup.toml",
                "ci/actup.toml",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo/",
                "/home/u/x/../repo/actup.toml",
                "actup.toml",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "ci\\actup.toml",
                "ci\\actup.toml",
            ),
            (
                PathStyle::Windows,
                "C:\\Users\\u\\repo",
                "ci\\actup.toml",
                "ci/actup.toml",
            ),
            (
                PathStyle::Windows,
                "C:\\Users\\u\\repo",
                "c:/users/U/Repo/ci/actup.toml",
                "ci/actup.toml",
            ),
        ];
        for (style, root, written, expected) in cases {
            assert_eq!(
                worktree_relative(style, root, written)?,
                WorktreePath(expected.to_owned()),
                "{style:?} {root} {written}"
            );
        }
        Ok(())
    }

    #[test]
    fn worktree_relative_rejects_paths_outside_the_worktree() {
        let cases = [
            (
                PathStyle::Posix,
                "/home/u/repo",
                "../actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "ci/../../actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "/etc/actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "/home/u/repo2/actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "/home/u/repo/../actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "/home/u/repo",
                "names the worktree root",
            ),
            (
                PathStyle::Posix,
                "/home/u/repo",
                "ci/..",
                "names the worktree root",
            ),
            (
                PathStyle::Windows,
                "C:\\repo",
                "D:\\repo\\actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Windows,
                "C:\\repo",
                "..\\actup.toml",
                "is outside the worktree",
            ),
            (
                PathStyle::Windows,
                "C:\\repo",
                "\\\\server\\share\\actup.toml",
                "is outside the worktree",
            ),
        ];
        for (style, root, written, expected) in cases {
            let Err(error) = worktree_relative(style, root, written) else {
                panic!("{style:?} {root} {written} must fail");
            };
            assert_eq!(
                error,
                format!("configPath `{written}` {expected} `{root}`"),
                "{style:?}"
            );
        }
    }

    #[test]
    fn path_style_follows_the_host_os() {
        assert!(matches!(
            PathStyle::of(zed::Os::Windows),
            PathStyle::Windows
        ));
        assert!(matches!(PathStyle::of(zed::Os::Mac), PathStyle::Posix));
        assert!(matches!(PathStyle::of(zed::Os::Linux), PathStyle::Posix));
    }

    #[test]
    fn read_config_rejects_a_missing_config_path_file() {
        let files = HashMap::new();
        let requested = RefCell::new(Vec::new());
        let config_path = WorktreePath("ci/actup.toml".to_owned());
        let Err(error) = read_config(read_from(&files, &requested), &[], Some(&config_path)) else {
            panic!("a missing configPath file must fail");
        };
        assert!(
            error.starts_with("cannot read configPath `ci/actup.toml`"),
            "{error}"
        );
    }

    #[test]
    fn no_files_output_starts_with_the_config_note() {
        let output = no_files_output(&loaded("actup.toml"));
        assert!(
            output.text.starts_with(&config_note(&loaded("actup.toml"))),
            "{}",
            output.text
        );
        assert!(output.text.contains("No workflow or action files found"));
        assert_eq!(output.sections.len(), 1);
        assert_eq!(output.sections[0].label, "actup: No files found");
    }

    #[test]
    fn no_refs_output_starts_with_the_config_note() {
        let output = no_refs_output(&Config::default(), 3);
        assert!(
            output.text.starts_with(&config_note(&Config::default())),
            "{}",
            output.text
        );
        assert!(output.text.contains("Found 3 workflow file(s)"));
        assert_eq!(output.sections[0].label, "actup: No refs found");
    }

    #[test]
    fn format_check_output_summary_counts_every_ref() {
        let sha = "a".repeat(40);
        let target = std::sync::Arc::new(actup_core::version::Info::new(
            actup_core::version::Version::parse("v5.0.0"),
            &sha,
        ));
        let statuses = [
            CheckStatus::UpToDate,
            CheckStatus::Outdated {
                current: "v4".to_string(),
                target,
            },
            CheckStatus::Pinned {
                sha: sha.clone(),
                tag: None,
            },
            CheckStatus::Ignored,
            CheckStatus::Floating {
                ref_name: "dev".to_string(),
            },
            CheckStatus::PinBranch {
                ref_name: "main".to_string(),
                sha,
            },
            CheckStatus::Unresolvable {
                reason: "not found".to_string(),
                is_environment_error: false,
            },
        ];
        let results: Vec<(String, UsesLocation, CheckResult)> = statuses
            .into_iter()
            .zip(1..)
            .map(|(status, line)| {
                let loc = UsesLocation::new(line, 9, "o/r@v4".to_string(), "  uses: o/r@v4".into());
                let result = CheckResult::new(ActionRef::simple("o", "r", "v4"), status);
                ("ci.yml".to_string(), loc, result)
            })
            .collect();
        let fetch_errors = [("github.com/o/x".to_string(), "timeout".to_string())];

        let text = format_check_output(&loaded("actup.toml"), &results, &fetch_errors, 1, 9);
        assert!(
            text.contains("**Summary:** 1 outdated, 1 up-to-date, 1 pinned, 1 floating, 1 refs to pin, 1 ignored, 2 unknown, 1 unresolvable\n"),
            "{text}"
        );
        assert!(
            text.contains("### Pin\n\n- `ci.yml` line 6: `o/r@v4` — pin ref `main` to `aaaaaaa`\n"),
            "{text}"
        );
        assert!(
            text.contains("line 5: `o/r@v4` — floating ref `dev`, pin to a tag or SHA\n"),
            "{text}"
        );
    }

    #[test]
    fn a_missing_repository_counts_as_unresolvable() {
        let repos = ["gone", "limited", "denied", "offline", "fine"];
        let all_refs: Vec<(String, UsesLocation, ActionRef)> = repos
            .iter()
            .zip(1..)
            .map(|(repo, line)| {
                let value = format!("o/{repo}@v1");
                let loc = UsesLocation::new(line, 9, value.clone(), format!("  uses: {value}"));
                (
                    "ci.yml".to_string(),
                    loc,
                    ActionRef::simple("o", *repo, "v1"),
                )
            })
            .collect();
        let key = |repo: &str| ("github.com".to_string(), "o".to_string(), repo.to_string());
        let fetched = HashMap::from([
            (
                key("gone"),
                Err(actup_core::Error::NotFound {
                    owner: "o".into(),
                    repo: "gone".into(),
                    host: Some("github.com".into()),
                }),
            ),
            (
                key("limited"),
                Err(actup_core::Error::RateLimited {
                    provider: "github".into(),
                    retry_after: None,
                }),
            ),
            (
                key("denied"),
                Err(actup_core::Error::Auth {
                    provider: "github".into(),
                    reason: "status code 401".into(),
                    source: None,
                }),
            ),
            (
                key("offline"),
                Err(actup_core::Error::Network {
                    operation: "GET tags".into(),
                    message: "connection reset".into(),
                    source: None,
                }),
            ),
            (key("fine"), Ok(empty_versions())),
        ]);

        let (results, fetch_errors) = collect_results(&all_refs, &fetched, |action_ref, _| {
            CheckResult::new(action_ref.clone(), CheckStatus::UpToDate)
        });
        let text = format_check_output(
            &Config::default(),
            &results,
            &fetch_errors,
            1,
            all_refs.len(),
        );

        assert!(
            text.contains("**Summary:** 0 outdated, 1 up-to-date, 0 pinned, 0 floating, 0 refs to pin, 0 ignored, 3 unknown, 1 unresolvable\n"),
            "{text}"
        );
        assert!(
            text.contains("- `ci.yml` line 1: `o/gone@v1` — action not found: github.com/o/gone\n"),
            "{text}"
        );
        let repos_with_errors: Vec<&str> =
            fetch_errors.iter().map(|(repo, _)| repo.as_str()).collect();
        assert_eq!(
            repos_with_errors,
            [
                "github.com/o/denied",
                "github.com/o/limited",
                "github.com/o/offline"
            ]
        );
    }

    fn empty_versions() -> actup_core::version::Set {
        actup_core::version::Set::build(Vec::new(), HashMap::new(), HashMap::new(), None)
    }

    #[test]
    fn invalid_refs_are_unresolvable_user_errors() {
        let files = [(
            ".github/workflows/ci.yml".to_string(),
            "runs:\n  steps:\n  - uses: o/r@a~1\n  - uses: o/r@v1.lock\n  - uses: o/r\n  - uses: o/r@\n  - uses: ./local\n    with:\n      uses: lint\n  - uses: o/s@v1\n".to_string(),
        )];
        let Collected { refs, results } = collect_refs(&files);
        let valid: Vec<(usize, String)> = refs
            .iter()
            .map(|(_, loc, action)| (loc.line(), action.to_string()))
            .collect();
        assert_eq!(valid, [(10, "o/s@v1".to_string())]);
        let invalid: Vec<(usize, &str, bool)> = results
            .iter()
            .map(|(_, loc, result)| match result.status() {
                CheckStatus::Unresolvable {
                    reason,
                    is_environment_error,
                } => (
                    loc.line(),
                    loc.value(),
                    !*is_environment_error && reason.starts_with("invalid action reference"),
                ),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            invalid,
            [
                (3, "o/r@a~1", true),
                (4, "o/r@v1.lock", true),
                (5, "o/r", true),
                (6, "o/r@", true),
            ]
        );
        let text = format_check_output(&Config::default(), &results, &[], 1, 5);
        assert!(text.contains(" 1 unknown, 4 unresolvable\n"), "{text}");
        assert!(
            text.contains("- `.github/workflows/ci.yml` line 3: `o/r@a~1` — invalid action reference 'o/r@a~1': ref `a~1` holds the character '~'\n"),
            "{text}"
        );
    }

    #[test]
    fn format_check_output_starts_with_the_config_note() {
        let config = loaded(".config/actup.toml");
        let text = format_check_output(&config, &[], &[], 2, 0);
        assert!(text.starts_with(&config_note(&config)), "{text}");
        assert!(text.contains("Scanned 2 file(s), found 0 remote `uses:` ref(s)."));
    }
}
