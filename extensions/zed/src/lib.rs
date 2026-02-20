//! Zed editor extension for actup — provides LSP integration and the
//! `/actions:update` slash command.

use zed_extension_api::{
    self as zed, SlashCommand, SlashCommandOutput, SlashCommandOutputSection, settings::LspSettings,
};

/// Zed extension entry point. Delegates to `actup-lsp` for language intelligence.
struct ActupExtension;

impl zed::Extension for ActupExtension {
    fn new() -> Self {
        Self
    }

    fn run_slash_command(
        &self,
        command: SlashCommand,
        _args: Vec<String>,
        _worktree: Option<&zed::Worktree>,
    ) -> Result<SlashCommandOutput, String> {
        match command.name.as_str() {
            "actions:update" => {
                let text = concat!(
                    "Update all GitHub Actions `uses:` refs in this workspace ",
                    "to their latest versions.\n",
                    "\n",
                    "Instructions:\n",
                    "1. Find all workflow files ",
                    "(`.github/workflows/**/*.yml`, `.github/workflows/**/*.yaml`) ",
                    "and composite action files ",
                    "(`action.yml`, `action.yaml`) in this workspace.\n",
                    "2. For each `uses:` reference (e.g. `actions/checkout@v3`), ",
                    "check if a newer version exists.\n",
                    "3. Update outdated refs to the latest stable version, ",
                    "respecting semver constraints.\n",
                    "4. Support all configured providers: GitHub, GitLab, Gitea, ",
                    "or any other host configured in `actup.toml`.\n",
                    "5. Preserve YAML formatting, comments, and quoting style.\n",
                    "6. If `actup` CLI is available, prefer running ",
                    "`actup check` then `actup update` for accurate results.\n",
                    "7. If `actup-lsp` is running, use its workspace diagnostics ",
                    "and code actions instead.\n",
                    "8. Summarize what was updated (old ref -> new ref) ",
                    "for each file.\n",
                );
                Ok(SlashCommandOutput {
                    sections: vec![SlashCommandOutputSection {
                        range: (0..text.len()).into(),
                        label: "actup: Update Actions".to_string(),
                    }],
                    text: text.to_string(),
                })
            }
            cmd => Err(format!("unknown slash command: \"{cmd}\"")),
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        let binary_settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.binary);

        let args = binary_settings
            .as_ref()
            .and_then(|s| s.arguments.clone())
            .unwrap_or_default();

        let command = if let Some(path) = binary_settings.as_ref().and_then(|s| s.path.clone()) {
            path
        } else {
            let settings_key = format!("lsp.{}.binary.path", language_server_id.as_ref());
            worktree.which("actup-lsp").ok_or_else(|| {
                format!(
                    "`actup-lsp` not found in PATH. \
                         Install: `cargo install --path crates/lsp` or \
                         set binary path in Zed settings under {settings_key}"
                )
            })?
        };

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

zed::register_extension!(ActupExtension);
