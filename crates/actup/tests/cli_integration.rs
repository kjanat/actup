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
    use super::test_support::ok_or_panic;
    use actup_core::{ActionRef, extract_uses_from_yaml, parse_uses};

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
    use super::test_support::ok_or_panic;
    use actup_core::{FileKind, discover_files};
    use std::fs;
    use tempfile::TempDir;

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
        assert!(files.is_empty());
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
}

mod config_tests {
    use super::test_support::{ok_or_panic, some_or_panic};
    use actup_core::{Config, find_repo_config, iter_config_candidates};
    use std::fs;
    use tempfile::TempDir;

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
            fs::write(temp.path().join("actup.toml"), "update_policy = \"minor\""),
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
                r#"{"update_policy": "minor"}"#,
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
            fs::write(config_dir.join("actup.toml"), "update_policy = \"patch\""),
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
update_policy = "minor"

[actions."special/action"]
update_policy = "patch"
"#,
            ),
            "write actup.toml fixture",
        );

        let config = ok_or_panic(Config::load(None, temp.path()), "load config from fixture");
        assert_eq!(config.policy_for("normal", "action").to_string(), "minor");
        assert_eq!(config.policy_for("special", "action").to_string(), "patch");
    }

    #[test]
    fn test_config_ignore_patterns() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(
                temp.path().join("actup.toml"),
                r#"
ignore = ["internal/*", "deprecated/action"]
"#,
            ),
            "write actup.toml fixture",
        );

        let config = ok_or_panic(Config::load(None, temp.path()), "load config from fixture");
        assert!(config.should_ignore("internal", "anything", "v1"));
        assert!(config.should_ignore("deprecated", "action", "v2"));
        assert!(!config.should_ignore("external", "action", "v1"));
    }
}

mod new_config_format_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::test_support::ok_or_panic;
    use actup_core::{ApiMode, Config, FetchSettings, HostConfig, HostKind};
    use std::fs;
    use std::num::NonZeroU8;
    use tempfile::TempDir;

    #[test]
    fn empty_config_defaults_to_graphql_and_no_hosts() {
        let temp = ok_or_panic(TempDir::new(), "create temp dir");
        ok_or_panic(
            fs::write(temp.path().join("actup.toml"), ""),
            "write empty actup.toml",
        );

        let config = ok_or_panic(Config::load(None, temp.path()), "load empty config");
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
        assert_eq!(config.hosts.len(), 1);
        let host = config.hosts.get("ghe.corp.com").expect("ghe host present");
        assert_eq!(host.kind, Some(HostKind::Github));
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
        assert_eq!(config.hosts.len(), 2);
        assert!(config.hosts.contains_key("ghe.corp.com"));
        assert!(config.hosts.contains_key("gitlab.corp.com"));

        let gitlab = config.hosts.get("gitlab.corp.com").unwrap();
        assert_eq!(gitlab.kind, Some(HostKind::Gitlab));
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
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
update_policy = "minor"
ref_style = "sha"
default_host = "github.com"
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

        let config = ok_or_panic(Config::load(None, temp.path()), "load config");
        assert_eq!(config.update_policy.to_string(), "minor");
        assert_eq!(config.default_host.as_deref(), Some("github.com"));
        assert_eq!(config.fetch.max_tags_per_repo.get(), 100);
        assert_eq!(config.hosts.len(), 1);
        assert!(config.should_ignore("internal", "action", "v1"));
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
                        kind: Some(HostKind::Github),
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
        assert_eq!(host.kind, Some(HostKind::Github));
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
                        kind: Some(HostKind::Github),
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

        let result = Config::load(None, temp.path());
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

        let result = Config::load(None, temp.path());
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

        let result = Config::load(None, temp.path());
        assert!(
            result.is_err(),
            "unsupported host kind should error during deserialization"
        );
    }
}

mod version_tests {
    use super::test_support::some_or_panic;
    use actup_core::version;
    use std::collections::HashMap;
    use std::sync::Arc;

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
        let latest = versions.find_latest_for(&current, false, false);

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
        let latest = versions.find_latest_for(&current, true, false);

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

        // Old field must be gone
        assert!(
            props.get("providers").is_none(),
            "providers property still present — old field not removed"
        );
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
