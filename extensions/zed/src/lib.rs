//! Zed editor extension for actup — provides LSP integration and the
//! `/actions:update` slash command with embedded actup-core WASM logic.
use zed_extension_api::{self as zed};

mod provider;

use std::{env, fmt::Write as _, fs};

use actup_core::{
    ActionRef, CheckResult, CheckStatus, Config, UsesLocation, extract_uses_from_yaml,
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

    // Try to read actup config from the worktree
    let config = read_config(worktree)?;

    // Discover and read workflow files
    let files = discover_workflow_files(worktree);
    if files.is_empty() {
        let text = "No workflow or action files found in this workspace.\n\
                    Looked for `.github/workflows/*.yml` and `action.yml`.\n";
        return Ok(SlashCommandOutput {
            sections: vec![SlashCommandOutputSection {
                range: (0..text.len()).into(),
                label: "actup: No files found".to_string(),
            }],
            text: text.to_string(),
        });
    }

    // Collect all uses refs across all files
    let mut all_refs: Vec<(String, UsesLocation, ActionRef)> = Vec::new();
    for (path, content) in &files {
        let uses = extract_uses_from_yaml(content);
        for u in uses {
            // parse_uses returns Ok(None) for local refs (./path, docker://)
            if let Ok(Some(parsed)) = actup_core::parse_uses(u.value()) {
                all_refs.push((path.clone(), u, parsed));
            }
        }
    }

    if all_refs.is_empty() {
        let text = format!(
            "Found {} workflow file(s), but no remote `uses:` references.\n",
            files.len()
        );
        return Ok(SlashCommandOutput {
            sections: vec![SlashCommandOutputSection {
                range: (0..text.len()).into(),
                label: "actup: No refs found".to_string(),
            }],
            text,
        });
    }

    // Deduplicate repos to minimize API calls. Key by (host, owner, repo) so the
    // same `owner/repo` on different hosts (e.g. GitHub.com vs GHE) does not collide.
    let mut unique_repos: std::collections::HashMap<
        (String, String, String),
        (String, String, String),
    > = std::collections::HashMap::new();
    for (_, _, action_ref) in &all_refs {
        let host = action_ref.host().unwrap_or("github.com").to_string();
        let owner = action_ref.owner().to_string();
        let repo = action_ref.repo().to_string();
        let api_base = api_base_for_host(&host);
        unique_repos
            .entry((host, owner.clone(), repo.clone()))
            .or_insert((owner, repo, api_base));
    }

    // Resolve GitHub token from shell environment
    let shell_env = worktree.shell_env();
    let token = shell_env
        .iter()
        .find(|(k, _)| k == "GITHUB_TOKEN")
        .map(|(_, v)| v.as_str());

    // Fetch versions for each unique (host, owner, repo)
    let mut version_cache: std::collections::HashMap<
        (String, String, String),
        actup_core::version::Set,
    > = std::collections::HashMap::new();
    let mut fetch_errors: Vec<(String, String)> = Vec::new();

    for (key, (owner, repo, api_base)) in &unique_repos {
        match provider::fetch_version_set(owner, repo, token, api_base, 3) {
            Ok(versions) => {
                version_cache.insert(key.clone(), versions);
            }
            Err(e) => {
                fetch_errors.push((format!("{}/{}/{}", key.0, key.1, key.2), e));
            }
        }
    }

    // Check each ref
    let mut results: Vec<(String, UsesLocation, CheckResult)> = Vec::new();
    for (path, uses_loc, action_ref) in &all_refs {
        let host = action_ref.host().unwrap_or("github.com").to_string();
        let key = (
            host,
            action_ref.owner().to_string(),
            action_ref.repo().to_string(),
        );
        if let Some(versions) = version_cache.get(&key) {
            let result = actup_core::check_action(action_ref, versions, &config);
            results.push((path.clone(), uses_loc.clone(), result));
        }
    }

    // Format output
    let text = format_check_output(&results, &fetch_errors, files.len(), all_refs.len());

    Ok(SlashCommandOutput {
        sections: vec![SlashCommandOutputSection {
            range: (0..text.len()).into(),
            label: "actup: Check Results".to_string(),
        }],
        text,
    })
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

/// Try to read actup config from the worktree.
fn read_config(worktree: &zed::Worktree) -> Result<Config, String> {
    for name in &["actup.toml", ".actup.toml", ".config/actup.toml"] {
        if let Ok(content) = worktree.read_text_file(name) {
            return parse_config(name, &content);
        }
    }
    Ok(Config::default())
}

/// Parse an actup TOML config, naming the file in the error.
fn parse_config(name: &str, content: &str) -> Result<Config, String> {
    toml::from_str::<Config>(content).map_err(|e| format!("invalid config at '{name}': {e}"))
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

/// Render the "Pin branch" section: branch refs to pin to their HEAD SHA.
fn write_pin_branch_section(out: &mut String, pin_branch: &[&(String, UsesLocation, CheckResult)]) {
    if pin_branch.is_empty() {
        return;
    }
    out.push_str("### Pin branch\n\n");
    for (path, uses_loc, result) in pin_branch {
        if let CheckStatus::PinBranch { ref_name, sha } = result.status() {
            let short = sha.get(..7).unwrap_or(sha);
            let _ = writeln!(
                out,
                "- `{path}` line {}: `{}` — pin `{ref_name}` to `{short}`",
                uses_loc.line(),
                uses_loc.value(),
            );
        }
    }
    out.push('\n');
}

/// Format check results into human-readable output for the slash command.
fn format_check_output(
    results: &[(String, UsesLocation, CheckResult)],
    fetch_errors: &[(String, String)],
    file_count: usize,
    ref_count: usize,
) -> String {
    let mut out = String::new();

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

    // Summary line
    let _ = writeln!(
        out,
        "**Summary:** {} outdated, {} up-to-date, {} pinned, {} floating, {} to pin, {} ignored\n",
        outdated.len(),
        up_to_date,
        pinned,
        floating.len(),
        pin_branch.len(),
        ignored,
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

    // Floating branch refs
    if !floating.is_empty() {
        out.push_str("### Floating\n\n");
        for (path, uses_loc, result) in &floating {
            if let CheckStatus::Floating { ref_name } = result.status() {
                let _ = writeln!(
                    out,
                    "- `{path}` line {}: `{}` — floating branch `{ref_name}`, pin to a tag or SHA",
                    uses_loc.line(),
                    uses_loc.value(),
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
    use super::*;

    #[test]
    fn parse_config_rejects_invalid_policy() {
        let Err(error) = parse_config("actup.toml", "[policy]\nbump = \"sideways\"\n") else {
            panic!("invalid config must not parse");
        };
        assert!(error.contains("actup.toml"), "{error}");
        assert!(error.contains("sideways"), "{error}");
    }

    #[test]
    fn parse_config_reads_policy() -> Result<(), String> {
        let config = parse_config("actup.toml", "[policy]\nbump = \"minor\"\n")?;
        assert_eq!(config.policy.bump, actup_core::policy::Update::Minor);
        Ok(())
    }
}
