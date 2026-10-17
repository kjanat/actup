//! Zed editor extension for actup: starts the `actup-lsp` language server and the `actup-mcp` context server.
use zed_extension_api::{self as zed};

use std::{env, fs};

use zed::settings::LspSettings;

/// GitHub Actions language server npm package.
const ACTIONS_LANGUAGE_SERVER_PACKAGE: &str = "@actions/languageserver";

/// GitHub Actions language server executable name.
const ACTIONS_LANGUAGE_SERVER_BINARY: &str = "actions-languageserver";

/// GitHub Actions language server package bin path after npm install.
const ACTIONS_LANGUAGE_SERVER_BIN_PATH: &str =
    "node_modules/@actions/languageserver/bin/actions-languageserver";

const CONTEXT_SERVER: &str = "actup-mcp";

/// Zed extension entry point. Delegates to `actup-lsp` for language intelligence.
struct ActupExtension;

impl zed::Extension for ActupExtension {
    fn new() -> Self {
        Self
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

    fn context_server_command(
        &mut self,
        _context_server_id: &zed::ContextServerId,
        _project: &zed::Project,
    ) -> zed::Result<zed::Command> {
        Ok(mcp_command())
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
    use zed_extension_api as zed;

    use super::ActupExtension;

    zed::register_extension!(ActupExtension);
}

fn mcp_command() -> zed::Command {
    zed::Command {
        command: CONTEXT_SERVER.to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Result<toml::Table, toml::de::Error> {
        toml::from_str(include_str!("../extension.toml"))
    }

    #[test]
    fn manifest_declares_the_actup_mcp_context_server() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = manifest()?;
        let servers = manifest
            .get("context_servers")
            .and_then(toml::Value::as_table)
            .ok_or("extension.toml declares no context servers")?;
        assert_eq!(servers.keys().collect::<Vec<_>>(), [CONTEXT_SERVER]);
        Ok(())
    }

    #[test]
    fn mcp_command_runs_actup_mcp_from_path() {
        let command = mcp_command();
        assert_eq!(command.command, "actup-mcp");
        assert_eq!(command.args, Vec::<String>::new());
        assert_eq!(command.env, Vec::<(String, String)>::new());
    }
}
