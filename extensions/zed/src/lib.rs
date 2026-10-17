//! Zed editor extension for actup: starts the `actup-lsp` language server and the `actup-mcp` context server.
use zed_extension_api::{self as zed};

use zed::settings::LspSettings;

const LANGUAGE_SERVER: &str = "actup-lsp";

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
            .unwrap_or_default();

        if let Some(command) = binary_settings.as_ref().and_then(|s| s.path.clone()) {
            return Ok(zed::Command {
                command,
                args,
                env: Vec::default(),
            });
        }

        let settings_key = format!("lsp.{server_name}.binary.path");
        let command = worktree.which(LANGUAGE_SERVER).ok_or_else(|| {
            format!(
                "`{LANGUAGE_SERVER}` not found in PATH. \
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Result<toml::Table, toml::de::Error> {
        toml::from_str(include_str!("../extension.toml"))
    }

    #[test]
    fn manifest_names_only_what_the_extension_runs() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = manifest()?;
        let servers = manifest
            .get("language_servers")
            .and_then(toml::Value::as_table)
            .ok_or("extension.toml declares no language servers")?;
        assert_eq!(servers.keys().collect::<Vec<_>>(), [LANGUAGE_SERVER]);
        assert_eq!(manifest.get("capabilities"), None);

        let grammar = manifest
            .get("grammars")
            .and_then(|grammars| grammars.get("github_actions"))
            .ok_or("extension.toml declares no `github_actions` grammar")?;
        let field = |key: &str| grammar.get(key).and_then(toml::Value::as_str);
        assert_eq!(
            field("repository"),
            Some("https://github.com/kjanat/actup-v2")
        );
        assert_eq!(
            field("rev"),
            Some("57a4f01c4bd1c8cd7599fdfc72f72fd41bd7414b")
        );
        assert_eq!(field("path"), Some("languages/tree-sitter-github-actions"));
        Ok(())
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
