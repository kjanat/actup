//! CLI integration tests
//!
//! These tests verify the CLI commands work correctly with real workflow files.

mod test_support {
    use std::fmt::Display;

    pub fn ok_or_panic<T, E: Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{context}: {error}"),
        }
    }

    pub fn some_or_panic<T>(option: Option<T>, context: &str) -> T {
        option.unwrap_or_else(|| panic!("{context}"))
    }
}

mod parser_tests {
    use actup_core::{ActionRef, extract_uses_from_yaml, parse_uses};

    use super::test_support::ok_or_panic;

    fn parse_uses_or_panic(input: &str) -> Option<ActionRef> {
        ok_or_panic(
            parse_uses(input),
            &format!("parse_uses failed for `{input}`"),
        )
    }

    #[test]
    fn test_extract_from_simple_workflow() {
        let content = include_str!("fixtures/simple_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].value(), "actions/checkout@v4");
        assert_eq!(uses[1].value(), "actions/setup-node@v4");
    }

    #[test]
    fn test_extract_from_mixed_workflow() {
        let content = include_str!("fixtures/mixed_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        // Should find 4: checkout, setup-node, cache, and local-action
        assert_eq!(uses.len(), 4);
    }

    #[test]
    fn test_parse_extracts_correct_refs() {
        let content = include_str!("fixtures/outdated_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        for loc in &uses {
            let parsed = parse_uses_or_panic(loc.value());
            if let Some(action) = parsed {
                assert!(
                    action.ref_str() == "v3",
                    "Expected v3, got {}",
                    action.ref_str()
                );
            }
        }
    }

    #[test]
    fn test_local_action_returns_none() {
        let result = parse_uses_or_panic("./local-action");
        assert!(result.is_none());
    }

    #[test]
    fn test_pinned_workflow_has_shas() {
        let content = include_str!("fixtures/pinned_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        for loc in &uses {
            let parsed = parse_uses_or_panic(loc.value());
            if let Some(action) = parsed {
                assert!(
                    action.is_sha(),
                    "Expected SHA ref, got {}",
                    action.ref_str()
                );
            }
        }
    }
}

mod discovery_tests {
    use std::fs;

    use actup_core::{FileKind, discover_files};
    use tempfile::TempDir;

    use super::test_support::ok_or_panic;

    #[test]
    fn test_discover_workflows() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let workflows_dir = temp.path().join(".github/workflows");
        ok_or_panic(fs::create_dir_all(&workflows_dir), "create workflows dir");
        ok_or_panic(
            fs::write(workflows_dir.join("ci.yml"), "name: CI"),
            "write ci fixture",
        );
        ok_or_panic(
            fs::write(workflows_dir.join("release.yaml"), "name: Release"),
            "write release fixture",
        );

        let files = discover_files(temp.path(), &[]);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.kind() == FileKind::Workflow));
    }

    #[test]
    fn test_discover_composite_actions() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let action_dir = temp.path().join(".github/actions/my-action");
        ok_or_panic(fs::create_dir_all(&action_dir), "create action dir");
        ok_or_panic(
            fs::write(action_dir.join("action.yml"), "name: My Action"),
            "write action fixture",
        );

        let files = discover_files(temp.path(), &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind(), FileKind::CompositeAction);
    }

    #[test]
    fn test_discover_empty_repo() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let files = discover_files(temp.path(), &[]);
        assert_eq!(files, []);
    }

    #[test]
    fn test_discover_with_extra_paths() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let custom_dir = temp.path().join("custom");
        ok_or_panic(fs::create_dir_all(&custom_dir), "create custom dir");
        ok_or_panic(
            fs::write(custom_dir.join("workflow.yml"), "name: Custom"),
            "write custom workflow fixture",
        );

        let extra = vec!["custom/*.yml".to_string()];
        let files = discover_files(temp.path(), &extra);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind(), FileKind::Custom);
    }

    #[test]
    fn test_discover_explicit_workflow_file() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let workflows_dir = temp.path().join(".github/workflows");
        ok_or_panic(fs::create_dir_all(&workflows_dir), "create workflows dir");
        let workflow = workflows_dir.join("release.yml");
        ok_or_panic(
            fs::write(&workflow, "name: Release"),
            "write release fixture",
        );

        let files = discover_files(&workflow, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind(), FileKind::Workflow);
        assert_eq!(files[0].path(), workflow.as_path());
    }
}

mod config_tests {
    use std::{fs, path::PathBuf};

    use actup_core::{ActionRef, Config, find_repo_config, iter_config_candidates, parse_uses};
    use tempfile::TempDir;

    use super::test_support::{ok_or_panic, some_or_panic};

    fn action(uses: &str) -> ActionRef {
        some_or_panic(ok_or_panic(parse_uses(uses), uses), uses)
    }

    #[test]
    fn test_config_discovery_order() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let candidates: Vec<_> = iter_config_candidates(temp.path()).collect();

        // First should be actup.toml at root
        assert!(candidates[0].ends_with("actup.toml"));
        // Should include all extensions
        assert!(
            candidates
                .iter()
                .any(|p| p.to_string_lossy().contains(".json"))
        );
        assert!(
            candidates
                .iter()
                .any(|p| p.to_string_lossy().contains(".yaml"))
        );
    }

    #[test]
    fn test_find_repo_config_toml() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(temp.path().join("actup.toml"), "[policy]\nbump = \"minor\""),
            "write actup.toml fixture",
        );

        let found = some_or_panic(find_repo_config(temp.path()), "find repo config");
        assert!(found.ends_with("actup.toml"));
    }

    #[test]
    fn test_find_repo_config_json() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.json"),
                r#"{"policy": {"bump": "minor"}}"#,
            ),
            "write actup.json fixture",
        );

        let found = some_or_panic(find_repo_config(temp.path()), "find repo config");
        assert!(found.ends_with("actup.json"));
    }

    #[test]
    fn test_find_repo_config_dotconfig() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        let config_dir = temp.path().join(".config");
        ok_or_panic(fs::create_dir_all(&config_dir), "create .config dir");
        ok_or_panic(
            fs::write(config_dir.join("actup.toml"), "[policy]\nbump = \"patch\""),
            "write .config actup.toml fixture",
        );

        let found = some_or_panic(find_repo_config(temp.path()), "find repo config");
        assert!(found.to_string_lossy().contains(".config"));
    }

    #[test]
    fn test_config_load_with_overrides() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[policy]
bump = "minor"

[[overrides]]
pattern = "special/*"
policy  = { bump = "major", track = "exact" }

[[overrides]]
pattern = "special/action"
policy  = { bump = "patch" }
"#,
            ),
            "write actup.toml fixture",
        );

        let config = ok_or_panic(
            Config::load(None, None, temp.path()),
            "load config from fixture",
        );
        let bump = |uses: &str| config.policy_for(&action(uses)).bump.to_string();
        assert_eq!(bump("normal/action@v1"), "minor");
        assert_eq!(bump("special/other@v1"), "major");
        assert_eq!(bump("special/action@v1"), "patch");
        assert_eq!(
            config
                .policy_for(&action("special/action@v1"))
                .track
                .to_string(),
            "exact"
        );
    }

    #[test]
    fn test_config_load_rejects_renamed_key_naming_file() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                "[actions.\"a/b\"]\nupdate_policy = \"patch\"\n",
            ),
            "write actup.toml fixture",
        );

        match Config::load(None, None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, reason }) => {
                assert_eq!(path, temp.path().join("actup.toml").display().to_string());
                assert!(
                    reason.starts_with("`actions` is no longer supported; use `[[overrides]]`"),
                    "{reason}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn test_config_load_renamed_key_names_the_file_holding_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let explicit = temp.path().join("explicit.toml");
        fs::write(&explicit, "[policy]\nbump = \"minor\"\n")?;
        fs::write(temp.path().join("actup.toml"), "ref_style = \"sha\"\n")?;

        match Config::load(Some(&explicit), None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, reason }) => {
                assert_eq!(path, temp.path().join("actup.toml").display().to_string());
                assert!(
                    reason.starts_with("`ref_style` is no longer supported; use `policy.pin`"),
                    "{reason}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_config_load_syntax_error_names_the_file() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        fs::write(temp.path().join("actup.toml"), "[policy\n")?;

        match Config::load(None, None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, .. }) => {
                assert_eq!(path, temp.path().join("actup.toml").display().to_string());
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_config_load_invalid_user_config_names_the_user_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let user = temp.path().join("user.toml");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo)?;
        fs::write(repo.join("actup.toml"), "[cache]\nenabled = false\n")?;

        for broken in ["update_policy = \"minor\"\n", "[policy\n"] {
            fs::write(&user, broken)?;
            match Config::load(None, Some(&user), &repo) {
                Err(actup_core::Error::InvalidConfig { path, .. }) => {
                    assert_eq!(path, user.display().to_string(), "{broken}");
                }
                other => panic!("expected InvalidConfig for {broken:?}, got {other:?}"),
            }
        }
        Ok(())
    }

    #[test]
    fn test_config_load_skips_missing_user_config() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let repo_config = temp.path().join("actup.toml");
        fs::write(&repo_config, "[cache]\nenabled = false\n")?;

        let config = Config::load(None, Some(&temp.path().join("absent.toml")), temp.path())?;
        assert_eq!(config.loaded_from, Some(repo_config));
        assert!(!config.cache.enabled);
        Ok(())
    }

    #[test]
    fn test_config_load_missing_explicit_path_is_invalid() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = TempDir::new()?;
        fs::write(temp.path().join("actup.toml"), "[cache]\nenabled = false\n")?;
        let explicit = temp.path().join("deleted.toml");

        match Config::load(Some(&explicit), None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, .. }) => {
                assert_eq!(path, explicit.display().to_string());
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_config_load_relative_explicit_path_skips_ancestors()
    -> Result<(), Box<dyn std::error::Error>> {
        let cwd = std::env::current_dir()?;
        let Some(name) = cwd.ancestors().skip(1).find_map(|dir| {
            fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
                let name = PathBuf::from(entry.file_name());
                (dir.join(&name).is_file() && !cwd.join(&name).exists()).then_some(name)
            })
        }) else {
            panic!(
                "no ancestor of {} holds a file absent from it",
                cwd.display()
            );
        };
        let temp = TempDir::new()?;

        match Config::load(Some(&name), None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, .. }) => {
                assert_eq!(path, name.display().to_string());
            }
            other => panic!(
                "expected InvalidConfig for {}, got {other:?}",
                name.display()
            ),
        }
        Ok(())
    }

    #[test]
    fn test_config_load_renamed_key_in_jsonc_names_the_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let explicit = temp.path().join("actup.jsonc");
        fs::write(&explicit, "{\n  // old key\n  \"extra_paths\": []\n}\n")?;

        match Config::load(Some(&explicit), None, temp.path()) {
            Err(actup_core::Error::InvalidConfig { path, reason }) => {
                assert_eq!(path, explicit.display().to_string());
                assert!(
                    reason.starts_with(
                        "`extra_paths` is no longer supported; use `scan.extra_paths`"
                    ),
                    "{reason}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_config_ignore_patterns() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[scan]
ignore = ["internal/*", "deprecated/action"]
"#,
            ),
            "write actup.toml fixture",
        );

        let config = ok_or_panic(
            Config::load(None, None, temp.path()),
            "load config from fixture",
        );
        assert!(config.should_ignore(&action("internal/anything@v1")));
        assert!(config.should_ignore(&action("deprecated/action@v2")));
        assert!(!config.should_ignore(&action("external/action@v1")));
    }
}

mod new_config_format_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::{fs, num::NonZeroU8};

    use actup_core::{ApiMode, Config, FetchSettings, HostConfig, HostKind};
    use tempfile::TempDir;

    use super::test_support::ok_or_panic;

    #[test]
    fn empty_config_defaults_to_graphql_and_no_hosts() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(temp.path().join("actup.toml"), ""),
            "write empty actup.toml",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load empty config");
        assert_eq!(config.fetch.api_mode, ApiMode::Graphql);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 10);
        assert!(config.hosts.is_empty());
        assert!(config.default_host.is_none());
    }

    #[test]
    fn fetch_section_overrides_defaults() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[fetch]
api_mode = "rest"
max_tags_per_repo = 50
"#,
            ),
            "write actup.toml with fetch",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        assert_eq!(config.fetch.api_mode, ApiMode::Rest);
        assert_eq!(config.fetch.max_tags_per_repo.get(), 50);
    }

    #[test]
    fn hosts_section_loads_correctly() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[hosts."ghe.corp.com"]
kind = "github"
api_base = "https://ghe.corp.com/api/v3"
token_env = "GHE_TOKEN"
"#,
            ),
            "write actup.toml with hosts",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        assert_eq!(config.hosts.len(), 1);
        let host = config.hosts.get("ghe.corp.com").expect("ghe host present");
        assert_eq!(host.kind, Some(HostKind::GitHub));
        assert_eq!(
            host.api_base.as_deref(),
            Some("https://ghe.corp.com/api/v3")
        );
        assert_eq!(host.token_env.as_deref(), Some("GHE_TOKEN"));
    }

    #[test]
    fn default_host_loads_from_config() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"default_host = "github.com""#,
            ),
            "write actup.toml with default_host",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
    }

    #[test]
    fn multiple_hosts_load_correctly() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[hosts."ghe.corp.com"]
kind = "github"

[hosts."gitlab.corp.com"]
kind = "gitlab"
api_base = "https://gitlab.corp.com/api/v4"
token_env = "GITLAB_TOKEN"
"#,
            ),
            "write actup.toml with multiple hosts",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        assert_eq!(config.hosts.len(), 2);
        assert!(config.hosts.contains_key("ghe.corp.com"));
        assert!(config.hosts.contains_key("gitlab.corp.com"));

        let gitlab = config.hosts.get("gitlab.corp.com").unwrap();
        assert_eq!(gitlab.kind, Some(HostKind::GitLab));
    }

    #[test]
    fn hosts_with_fetch_overrides() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[hosts."ghe.corp.com"]
kind = "github"
api_mode = "rest"
max_tags_per_repo = 25
"#,
            ),
            "write actup.toml with per-host fetch overrides",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        let host = config.hosts.get("ghe.corp.com").unwrap();
        assert_eq!(host.api_mode, Some(ApiMode::Rest));
        assert_eq!(host.max_tags_per_repo.map(NonZeroU8::get), Some(25));
    }

    #[test]
    fn full_new_format_config() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
default_host = "github.com"

[policy]
bump = "minor"
pin = "sha"

[scan]
ignore = ["internal/action"]

[cache]
ttl = 3600
enabled = true

[fetch]
api_mode = "graphql"
max_tags_per_repo = 100

[hosts."ghe.corp.com"]
kind = "github"
token_env = "GHE_TOKEN"
"#,
            ),
            "write full new-format actup.toml",
        );

        let config = ok_or_panic(Config::load(None, None, temp.path()), "load config");
        assert_eq!(config.policy.bump.to_string(), "minor");
        assert_eq!(config.policy.pin.to_string(), "sha");
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
        assert_eq!(config.fetch.max_tags_per_repo.get(), 100);
        assert_eq!(config.hosts.len(), 1);
        let Ok(Some(internal)) = actup_core::parse_uses("internal/action@v1") else {
            panic!("parse internal/action@v1");
        };
        assert!(config.should_ignore(&internal));
    }

    #[test]
    fn json_serialize_roundtrip() {
        let config = Config {
            default_host: Some("github.com".to_string()),
            fetch: FetchSettings {
                api_mode: ApiMode::Rest,
                max_tags_per_repo: NonZeroU8::new(42).unwrap(),
            },
            hosts: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "ghe.corp.com".to_string(),
                    HostConfig {
                        kind: Some(HostKind::GitHub),
                        api_base: Some("https://ghe.corp.com/api/v3".to_string()),
                        token_env: Some("GHE_TOKEN".to_string()),
                        api_mode: None,
                        max_tags_per_repo: None,
                    },
                );
                m
            },
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).expect("serialize to JSON");
        let roundtripped: Config = serde_json::from_str(&json).expect("deserialize from JSON");

        assert_eq!(roundtripped.default_host, config.default_host);
        assert_eq!(roundtripped.fetch.api_mode, config.fetch.api_mode);
        assert_eq!(
            roundtripped.fetch.max_tags_per_repo,
            config.fetch.max_tags_per_repo
        );
        assert_eq!(roundtripped.hosts.len(), 1);
        let host = roundtripped.hosts.get("ghe.corp.com").unwrap();
        assert_eq!(host.kind, Some(HostKind::GitHub));
    }

    #[test]
    fn toml_serialize_roundtrip() {
        let config = Config {
            default_host: Some("github.com".to_string()),
            fetch: FetchSettings {
                api_mode: ApiMode::Graphql,
                max_tags_per_repo: NonZeroU8::new(30).unwrap(),
            },
            ..Default::default()
        };

        let toml_str = toml::to_string_pretty(&config).expect("serialize to TOML");
        let roundtripped: Config = toml::from_str(&toml_str).expect("deserialize from TOML");

        assert_eq!(roundtripped.default_host, config.default_host);
        assert_eq!(roundtripped.fetch.api_mode, config.fetch.api_mode);
        assert_eq!(
            roundtripped.fetch.max_tags_per_repo,
            config.fetch.max_tags_per_repo
        );
    }

    #[test]
    fn config_command_json_output_contains_new_fields() {
        let config = Config {
            default_host: Some("github.com".to_string()),
            fetch: FetchSettings::default(),
            hosts: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "ghe.corp.com".to_string(),
                    HostConfig {
                        kind: Some(HostKind::GitHub),
                        api_base: None,
                        token_env: None,
                        api_mode: None,
                        max_tags_per_repo: None,
                    },
                );
                m
            },
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).expect("serialize");
        assert!(
            json.contains("default_host"),
            "JSON must include default_host"
        );
        assert!(json.contains("fetch"), "JSON must include fetch section");
        assert!(json.contains("hosts"), "JSON must include hosts section");
        assert!(
            json.contains("ghe.corp.com"),
            "JSON must include host entries"
        );
        assert!(
            json.contains("api_mode"),
            "JSON must include api_mode in fetch"
        );
    }

    #[test]
    fn config_command_toml_output_contains_new_fields() {
        let config = Config {
            default_host: Some("github.com".to_string()),
            fetch: FetchSettings {
                api_mode: ApiMode::Rest,
                max_tags_per_repo: NonZeroU8::new(20).unwrap(),
            },
            ..Default::default()
        };

        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            toml_str.contains("default_host"),
            "TOML must include default_host"
        );
        assert!(
            toml_str.contains("api_mode"),
            "TOML must include api_mode in fetch"
        );
        assert!(
            toml_str.contains("max_tags_per_repo"),
            "TOML must include max_tags_per_repo"
        );
    }

    #[test]
    fn invalid_config_unknown_field_in_hosts_errors() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[hosts."example.com"]
kind = "github"
bogus_field = "oops"
"#,
            ),
            "write actup.toml with unknown field in hosts",
        );

        let result = Config::load(None, None, temp.path());
        assert!(
            result.is_err(),
            "unknown field in hosts should error due to deny_unknown_fields"
        );
    }

    #[test]
    fn invalid_config_unknown_field_in_fetch_errors() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[fetch]
api_mode = "graphql"
not_a_field = true
"#,
            ),
            "write actup.toml with unknown field in fetch",
        );

        let result = Config::load(None, None, temp.path());
        assert!(
            result.is_err(),
            "unknown field in fetch should error due to deny_unknown_fields"
        );
    }

    #[test]
    fn invalid_host_kind_errors() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
[hosts."example.com"]
kind = "bitbucket"
"#,
            ),
            "write actup.toml with invalid kind",
        );

        let result = Config::load(None, None, temp.path());
        assert!(
            result.is_err(),
            "unsupported host kind should error during deserialization"
        );
    }
}

mod version_tests {
    use std::{collections::HashMap, sync::Arc};

    use actup_core::{policy, version};

    use super::test_support::some_or_panic;

    fn make_version_info(ver: &str, sha: &str) -> Arc<version::Info> {
        Arc::new(version::Info::new(version::Version::parse(ver), sha))
    }

    fn make_version_set(
        tags: Vec<Arc<version::Info>>,
        latest: Option<Arc<version::Info>>,
        latest_per_major: HashMap<u32, Arc<version::Info>>,
    ) -> version::Set {
        version::Set::build(tags, latest_per_major, HashMap::new(), latest)
    }

    #[test]
    fn test_find_latest_for_minor_policy() {
        let v1_2 = make_version_info("v1.2.0", "ccc");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1_2.clone());
        latest_per_major.insert(2, make_version_info("v2.0.0", "ddd"));

        let versions = make_version_set(
            vec![
                make_version_info("v1.0.0", "aaa"),
                make_version_info("v1.1.0", "bbb"),
                v1_2,
                make_version_info("v2.0.0", "ddd"),
            ],
            Some(make_version_info("v2.0.0", "ddd")),
            latest_per_major,
        );

        let current = version::Version::parse("v1.0.0");
        let latest = versions.find_latest_for(&current, policy::Update::Minor, false);

        let latest = some_or_panic(latest, "expected latest version for minor policy");
        assert_eq!(latest.version().to_string(), "v1.2.0");
    }

    #[test]
    fn test_find_latest_for_major_policy() {
        let v3 = make_version_info("v3.0.0", "ccc");
        let versions = make_version_set(
            vec![
                make_version_info("v1.0.0", "aaa"),
                make_version_info("v2.0.0", "bbb"),
                v3.clone(),
            ],
            Some(v3),
            HashMap::new(),
        );

        let current = version::Version::parse("v1.0.0");
        let latest = versions.find_latest_for(&current, policy::Update::Major, false);

        let latest = some_or_panic(latest, "expected latest version for major policy");
        assert_eq!(latest.version().to_string(), "v3.0.0");
    }

    #[test]
    fn test_version_parse_major_only() {
        let v = version::Version::parse("v4");
        assert!(v.is_semver());
        assert_eq!(v.major(), Some(4));
    }

    #[test]
    fn test_version_is_sha() {
        let sha = version::Version::parse("b4ffde65f46336ab88eb53be808477a3936bae11");
        assert!(sha.is_sha());
        assert!(!sha.is_semver());
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
mod schema_tests {
    use actup_core::Config;
    use schemars::schema_for;

    fn generated_schema() -> serde_json::Value {
        let schema = schema_for!(Config);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn schema_file_matches_generated() {
        let on_disk = include_str!("../../../schemas/actup-config.schema.json");
        let on_disk: serde_json::Value = serde_json::from_str(on_disk).unwrap();
        let generated = generated_schema();
        assert_eq!(
            on_disk, generated,
            "schemas/actup-config.schema.json is stale — run `just schema` to regenerate"
        );
    }

    #[test]
    fn schema_has_new_config_types() {
        let schema = generated_schema();
        let defs = schema.get("$defs").expect("$defs missing");

        // New types from config-refactor must be present
        assert!(
            defs.get("FetchSettings").is_some(),
            "FetchSettings missing from $defs"
        );
        assert!(
            defs.get("HostConfig").is_some(),
            "HostConfig missing from $defs"
        );
        assert!(
            defs.get("HostKind").is_some(),
            "HostKind missing from $defs"
        );
    }

    #[test]
    fn schema_has_no_old_provider_types() {
        let schema = generated_schema();
        let defs = schema.get("$defs").expect("$defs missing");

        // Old types removed during config-refactor must be absent
        assert!(
            defs.get("Providers").is_none(),
            "Providers still in $defs — old type not removed"
        );
        assert!(
            defs.get("ProviderInstance").is_none(),
            "ProviderInstance still in $defs — old type not removed"
        );
    }

    #[test]
    fn schema_top_level_properties() {
        let schema = generated_schema();
        let props = schema.get("properties").expect("properties missing");

        // New fields
        assert!(
            props.get("default_host").is_some(),
            "default_host property missing"
        );
        assert!(props.get("fetch").is_some(), "fetch property missing");
        assert!(props.get("hosts").is_some(), "hosts property missing");

        assert!(props.get("policy").is_some(), "policy property missing");
        assert!(props.get("scan").is_some(), "scan property missing");
        assert!(
            props.get("overrides").is_some(),
            "overrides property missing"
        );

        // Old field must be gone
        assert!(
            props.get("providers").is_none(),
            "providers property still present — old field not removed"
        );
        for old in [
            "update_policy",
            "ref_style",
            "branch_policy",
            "allow_prerelease",
            "ignore",
            "extra_paths",
            "actions",
        ] {
            assert!(props.get(old).is_none(), "{old} property still present");
        }
    }

    #[test]
    fn schema_fetch_settings_has_deny_unknown() {
        let schema = generated_schema();
        let fetch = &schema["$defs"]["FetchSettings"];
        assert_eq!(
            fetch.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "FetchSettings should deny unknown fields"
        );
    }

    #[test]
    fn schema_host_config_has_deny_unknown() {
        let schema = generated_schema();
        let host = &schema["$defs"]["HostConfig"];
        assert_eq!(
            host.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "HostConfig should deny unknown fields"
        );
    }

    #[test]
    fn schema_hostkind_variants() {
        let schema = generated_schema();
        let hk = &schema["$defs"]["HostKind"];
        let variants = hk.get("oneOf").expect("HostKind missing oneOf");
        let consts: Vec<&str> = variants
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.get("const").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(consts, vec!["github", "gitlab", "gitea"]);
    }

    #[test]
    fn schema_hosts_keyed_by_hostname() {
        let schema = generated_schema();
        let hosts = &schema["properties"]["hosts"];
        // hosts should be an object with additionalProperties referencing HostConfig
        assert_eq!(
            hosts.get("type").and_then(serde_json::Value::as_str),
            Some("object")
        );
        let additional = hosts
            .get("additionalProperties")
            .expect("hosts missing additionalProperties");
        let ref_str = additional.get("$ref").and_then(serde_json::Value::as_str);
        assert_eq!(ref_str, Some("#/$defs/HostConfig"));
    }
}

mod cli_runs {
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    fn broken_repo() -> Result<TempDir, Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        fs::write(
            temp.path().join("actup.toml"),
            "[policy]\nbump = \"sideways\"\n",
        )?;
        Ok(temp)
    }

    fn actup(home: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_actup"));
        cmd.env("XDG_CONFIG_HOME", home).env("HOME", home);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ACTUP_") {
                cmd.env_remove(key);
            }
        }
        cmd
    }

    #[test]
    fn check_with_invalid_config_exits_2() -> Result<(), Box<dyn std::error::Error>> {
        let repo = broken_repo()?;
        let output = actup(repo.path()).arg("check").arg(repo.path()).output()?;

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        let config_path = repo.path().join("actup.toml").display().to_string();
        assert!(stderr.contains(&config_path), "stderr: {stderr}");
        assert!(stderr.contains("sideways"), "stderr: {stderr}");
        Ok(())
    }

    #[test]
    fn check_reads_user_config_from_xdg_config_home() -> Result<(), Box<dyn std::error::Error>> {
        let home = TempDir::new()?;
        let repo = TempDir::new()?;
        let user_config = home.path().join("actup").join("config.toml");
        fs::create_dir_all(home.path().join("actup"))?;
        fs::write(&user_config, "update_policy = \"minor\"\n")?;
        let output = actup(home.path()).arg("check").arg(repo.path()).output()?;

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains(&user_config.display().to_string()),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("update_policy"), "stderr: {stderr}");
        Ok(())
    }

    #[test]
    fn config_human_shows_track_and_overrides() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        fs::write(
            repo.path().join("actup.toml"),
            "[policy]\ntrack = \"exact\"\n\n[[overrides]]\npattern = \"my-org/*\"\npolicy = { pin = \"sha\" }\n\n[[overrides]]\npattern = \"ghe.example.com/platform/*\"\npolicy = { bump = \"patch\" }\n",
        )?;
        let output = actup(repo.path()).arg("config").arg(repo.path()).output()?;

        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            stdout.contains("  track:       exact\n"),
            "stdout: {stdout}"
        );
        let overrides = stdout
            .split_once("Overrides")
            .map(|(_, rest)| rest)
            .ok_or("no Overrides section")?;
        assert!(
            overrides.contains("  - my-org/*\n  - ghe.example.com/platform/*\n"),
            "stdout: {stdout}"
        );
        Ok(())
    }

    #[test]
    fn check_with_renamed_key_in_env_names_the_env() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        let output = actup(repo.path())
            .env("ACTUP_IGNORE", "a/b")
            .arg("check")
            .arg(repo.path())
            .output()?;

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains(
                "invalid config at '`ACTUP_` environment variable(s)': `ignore` is no longer supported; use `scan.ignore`"
            ),
            "stderr: {stderr}"
        );
        Ok(())
    }

    #[test]
    fn check_loads_relative_config_flag() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        fs::write(
            repo.path().join("x.toml"),
            "[policy]\nbump = \"sideways\"\n",
        )?;
        let output = actup(repo.path())
            .current_dir(repo.path())
            .args(["--config", "./x.toml", "check"])
            .arg(repo.path())
            .output()?;

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("invalid config at './x.toml'"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains("sideways"), "stderr: {stderr}");
        Ok(())
    }

    #[test]
    fn check_with_missing_config_flag_exits_2() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        let missing = repo.path().join("missing.toml");
        let output = actup(repo.path())
            .arg("check")
            .arg(repo.path())
            .arg("--config")
            .arg(&missing)
            .output()?;

        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains(&format!("invalid config at '{}'", missing.display())),
            "stderr: {stderr}"
        );
        Ok(())
    }

    #[test]
    fn config_shows_explicit_config_from_flag_or_env() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        fs::write(
            repo.path().join("actup.toml"),
            "[policy]\nbump = \"minor\"\npin = \"sha\"\n",
        )?;
        fs::write(repo.path().join("x.toml"), "[policy]\nbump = \"patch\"\n")?;

        let runs: [(&[&str], Option<&str>); 3] = [
            (&["--config", "x.toml", "config"], None),
            (&["config", "-c", "x.toml"], None),
            (&["config"], Some("x.toml")),
        ];
        for (args, env) in runs {
            let mut cmd = actup(repo.path());
            cmd.current_dir(repo.path()).args(args);
            if let Some(path) = env {
                cmd.env("ACTUP_CONFIG", path);
            }
            let output = cmd.output()?;

            assert_eq!(output.status.code(), Some(0), "{args:?}");
            let stdout = String::from_utf8(output.stdout)?;
            assert!(
                stdout.contains("  bump:        patch\n"),
                "{args:?}: {stdout}"
            );
            assert!(
                stdout.contains("  pin:         sha\n"),
                "{args:?}: {stdout}"
            );
            assert!(
                stdout.contains("  Loaded from: x.toml\n"),
                "{args:?}: {stdout}"
            );
        }
        Ok(())
    }

    #[test]
    fn config_reads_format_from_env_next_to_the_subcommand()
    -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        let output = actup(repo.path())
            .env("ACTUP_FORMAT", "json")
            .arg("config")
            .arg(repo.path())
            .output()?;

        assert_eq!(output.status.code(), Some(0));
        let config: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert!(config.get("policy").is_some(), "config: {config}");
        Ok(())
    }

    #[test]
    fn config_shows_double_underscore_env_overrides() -> Result<(), Box<dyn std::error::Error>> {
        let repo = TempDir::new()?;
        fs::write(
            repo.path().join("actup.toml"),
            "[policy]\nbump = \"minor\"\n",
        )?;
        let output = actup(repo.path())
            .env("ACTUP_POLICY__BUMP", "patch")
            .env("ACTUP_CACHE__TTL", "60")
            .env("ACTUP_FETCH__MAX_TAGS_PER_REPO", "7")
            .env("ACTUP_POLICY_PIN", "sha")
            .arg("config")
            .arg(repo.path())
            .output()?;

        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout)?;
        for line in [
            "  bump:        patch\n",
            "  pin:         preserve\n",
            "  ttl:      60 seconds",
            "  max_tags_per_repo: 7\n",
        ] {
            assert!(stdout.contains(line), "{line:?}: {stdout}");
        }
        Ok(())
    }

    #[test]
    fn config_layers_defaults_user_repo_explicit_env() -> Result<(), Box<dyn std::error::Error>> {
        let home = TempDir::new()?;
        let repo = TempDir::new()?;
        fs::create_dir_all(home.path().join("actup"))?;
        fs::write(
            home.path().join("actup").join("config.toml"),
            "[policy]\nbump = \"patch\"\npin = \"tag\"\ntrack = \"major\"\nbranch = \"warn\"\n",
        )?;
        fs::write(
            repo.path().join("actup.toml"),
            "[policy]\npin = \"sha\"\ntrack = \"major\"\nbranch = \"warn\"\n",
        )?;
        fs::write(
            repo.path().join("explicit.toml"),
            "[policy]\ntrack = \"exact\"\nbranch = \"warn\"\n",
        )?;
        let output = actup(home.path())
            .env("ACTUP_POLICY__BRANCH", "update")
            .current_dir(repo.path())
            .args(["--config", "explicit.toml", "config", "--format", "json"])
            .output()?;

        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let config: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            config.get("policy"),
            Some(&serde_json::json!({
                "bump": "patch",
                "pin": "sha",
                "track": "exact",
                "branch": "update",
                "prerelease": false,
            })),
            "config: {config}"
        );
        assert_eq!(
            config.pointer("/cache/ttl"),
            Some(&serde_json::json!(86400))
        );
        Ok(())
    }

    #[test]
    fn schema_ignores_invalid_config() -> Result<(), Box<dyn std::error::Error>> {
        let repo = broken_repo()?;
        let output = actup(repo.path())
            .arg("schema")
            .current_dir(repo.path())
            .output()?;

        assert_eq!(output.status.code(), Some(0));
        let schema: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert!(schema.get("properties").is_some(), "schema: {schema}");
        Ok(())
    }
}
