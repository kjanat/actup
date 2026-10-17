//! Zed editor extension for actup: starts the `actup-lsp` language server and the `actup-mcp` context server.
use zed_extension_api::{self as zed};

use zed::settings::LspSettings;

const LANGUAGE_SERVER: &str = "actup-lsp";

const CONTEXT_SERVER: &str = "actup-mcp";

const CONTEXT_SERVER_SETTING: &str = "context_servers.actup-mcp.command";

#[derive(Debug, PartialEq, Eq)]
struct PathLookup {
    command: &'static str,
    args: &'static [&'static str],
}

const UNIX_LOOKUP: PathLookup = PathLookup {
    command: "sh",
    args: &["-c", "command -v actup-mcp"],
};

const WINDOWS_LOOKUP: PathLookup = PathLookup {
    command: "where.exe",
    args: &["actup-mcp"],
};

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
        let (os, _) = zed::current_platform();
        let lookup = path_lookup(os);
        mcp_command(
            zed::process::Command::new(lookup.command)
                .args(lookup.args.iter().copied())
                .output(),
        )
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

const fn path_lookup(os: zed::Os) -> PathLookup {
    match os {
        zed::Os::Mac | zed::Os::Linux => UNIX_LOOKUP,
        zed::Os::Windows => WINDOWS_LOOKUP,
    }
}

fn mcp_command(lookup: Result<zed::process::Output, String>) -> zed::Result<zed::Command> {
    let output = lookup.map_err(|error| {
        format!(
            "`{CONTEXT_SERVER}` PATH lookup failed: {error}. \
             Set its absolute path in Zed settings under {CONTEXT_SERVER_SETTING}"
        )
    })?;
    let not_found = || {
        format!(
            "`{CONTEXT_SERVER}` not found in PATH. \
             Set its absolute path in Zed settings under {CONTEXT_SERVER_SETTING}"
        )
    };
    if output.status != Some(0) {
        return Err(not_found());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|path| zed::Command {
            command: path.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        })
        .ok_or_else(not_found)
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
        assert_eq!(manifest.get("slash_commands"), None);

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
    fn extension_dir_carries_the_repository_licences() {
        assert_eq!(
            include_str!("../LICENSE-MIT"),
            include_str!("../../../LICENSE-MIT")
        );
        assert_eq!(
            include_str!("../LICENSE-APACHE"),
            include_str!("../../../LICENSE-APACHE")
        );
    }

    #[test]
    fn manifest_grants_exactly_the_actup_mcp_path_lookups() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut expected: toml::Table = toml::from_str(
            r#"
            capabilities = [
                { kind = "process:exec", command = "sh", args = ["-c", "command -v actup-mcp"] },
                { kind = "process:exec", command = "where.exe", args = ["actup-mcp"] },
            ]
            "#,
        )?;
        assert_eq!(
            manifest()?.get("capabilities"),
            expected.remove("capabilities").as_ref()
        );
        Ok(())
    }

    #[test]
    fn path_lookup_runs_sh_on_unix_and_where_exe_on_windows() {
        let unix = PathLookup {
            command: "sh",
            args: &["-c", "command -v actup-mcp"],
        };
        assert_eq!(path_lookup(zed::Os::Linux), unix);
        assert_eq!(path_lookup(zed::Os::Mac), unix);
        assert_eq!(
            path_lookup(zed::Os::Windows),
            PathLookup {
                command: "where.exe",
                args: &["actup-mcp"],
            }
        );
    }

    fn output(status: Option<i32>, stdout: &str) -> zed::process::Output {
        zed::process::Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn assert_runs(result: zed::Result<zed::Command>, path: &str) -> Result<(), String> {
        let command = result?;
        assert_eq!(command.command, path);
        assert_eq!(command.args, Vec::<String>::new());
        assert_eq!(command.env, Vec::<(String, String)>::new());
        Ok(())
    }

    fn assert_names_the_setting(result: zed::Result<zed::Command>) {
        match result {
            Ok(command) => panic!("expected a lookup error, got {:?}", command.command),
            Err(error) => assert!(
                error.contains("context_servers.actup-mcp.command"),
                "{error}"
            ),
        }
    }

    #[test]
    fn mcp_command_runs_the_path_the_lookup_prints() -> Result<(), String> {
        assert_runs(
            mcp_command(Ok(output(Some(0), "/home/user/.cargo/bin/actup-mcp\n"))),
            "/home/user/.cargo/bin/actup-mcp",
        )
    }

    #[test]
    fn mcp_command_takes_the_first_path_where_exe_prints() -> Result<(), String> {
        assert_runs(
            mcp_command(Ok(output(
                Some(0),
                "\r\nC:\\Users\\u\\.cargo\\bin\\actup-mcp.exe\r\nC:\\tools\\actup-mcp.exe\r\n",
            ))),
            r"C:\Users\u\.cargo\bin\actup-mcp.exe",
        )
    }

    #[test]
    fn mcp_command_rejects_empty_output() {
        assert_names_the_setting(mcp_command(Ok(output(Some(0), ""))));
        assert_names_the_setting(mcp_command(Ok(output(Some(0), "\r\n  \n"))));
    }

    #[test]
    fn mcp_command_rejects_a_failed_lookup() {
        assert_names_the_setting(mcp_command(Ok(output(Some(1), "/usr/bin/actup-mcp\n"))));
        assert_names_the_setting(mcp_command(Ok(output(None, "/usr/bin/actup-mcp\n"))));
        assert_names_the_setting(mcp_command(Err("process:exec denied".to_owned())));
    }
}
