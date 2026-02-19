use zed_extension_api::{self as zed, settings::LspSettings};

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
        let binary_settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.binary);

        let args = binary_settings
            .as_ref()
            .and_then(|s| s.arguments.clone())
            .unwrap_or_default();

        let command = match binary_settings.as_ref().and_then(|s| s.path.clone()) {
            Some(path) => path,
            None => worktree.which("actup-lsp").ok_or_else(|| {
                concat!(
                    "`actup-lsp` not found in PATH. ",
                    "Install: `cargo install --path crates/lsp` or ",
                    "set binary path in Zed settings under ",
                    "lsp.actup-lsp.binary.path"
                )
                .to_string()
            })?,
        };

        Ok(zed::Command {
            command,
            args,
            env: Default::default(),
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
