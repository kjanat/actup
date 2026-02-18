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
