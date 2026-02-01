//! CLI integration tests
//!
//! These tests verify the CLI commands work correctly with real workflow files.

#![allow(clippy::unwrap_used)]

mod parser_tests {
    use actup_core::{extract_uses_from_yaml, parse_uses};

    #[test]
    fn test_extract_from_simple_workflow() {
        let content = include_str!("fixtures/simple_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].value, "actions/checkout@v4");
        assert_eq!(uses[1].value, "actions/setup-node@v4");
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
            let parsed = parse_uses(&loc.value).unwrap();
            if let Some(action) = parsed {
                assert!(
                    action.ref_str == "v3",
                    "Expected v3, got {}",
                    action.ref_str
                );
            }
        }
    }

    #[test]
    fn test_local_action_returns_none() {
        let result = parse_uses("./local-action").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_pinned_workflow_has_shas() {
        let content = include_str!("fixtures/pinned_workflow.yml");
        let uses = extract_uses_from_yaml(content);

        for loc in &uses {
            let parsed = parse_uses(&loc.value).unwrap();
            if let Some(action) = parsed {
                assert!(action.is_sha(), "Expected SHA ref, got {}", action.ref_str);
            }
        }
    }
}

mod discovery_tests {
    use actup_core::{FileKind, discover_files};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_discover_workflows() {
        let temp = TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".github/workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(workflows_dir.join("ci.yml"), "name: CI").unwrap();
        fs::write(workflows_dir.join("release.yaml"), "name: Release").unwrap();

        let files = discover_files(temp.path(), &[]);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.kind == FileKind::Workflow));
    }

    #[test]
    fn test_discover_composite_actions() {
        let temp = TempDir::new().unwrap();
        let action_dir = temp.path().join(".github/actions/my-action");
        fs::create_dir_all(&action_dir).unwrap();
        fs::write(action_dir.join("action.yml"), "name: My Action").unwrap();

        let files = discover_files(temp.path(), &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, FileKind::CompositeAction);
    }

    #[test]
    fn test_discover_empty_repo() {
        let temp = TempDir::new().unwrap();
        let files = discover_files(temp.path(), &[]);
        assert!(files.is_empty());
    }

    #[test]
    fn test_discover_with_extra_paths() {
        let temp = TempDir::new().unwrap();
        let custom_dir = temp.path().join("custom");
        fs::create_dir_all(&custom_dir).unwrap();
        fs::write(custom_dir.join("workflow.yml"), "name: Custom").unwrap();

        let extra = vec!["custom/*.yml".to_string()];
        let files = discover_files(temp.path(), &extra);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, FileKind::Custom);
    }
}

mod config_tests {
    use actup_core::{Config, find_repo_config, iter_config_candidates};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_config_discovery_order() {
        let temp = TempDir::new().unwrap();
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
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("actup.toml"), "update_policy = \"minor\"").unwrap();

        let found = find_repo_config(temp.path());
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("actup.toml"));
    }

    #[test]
    fn test_find_repo_config_json() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("actup.json"),
            r#"{"update_policy": "minor"}"#,
        )
        .unwrap();

        let found = find_repo_config(temp.path());
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("actup.json"));
    }

    #[test]
    fn test_find_repo_config_dotconfig() {
        let temp = TempDir::new().unwrap();
        let config_dir = temp.path().join(".config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("actup.toml"), "update_policy = \"patch\"").unwrap();

        let found = find_repo_config(temp.path());
        assert!(found.is_some());
        assert!(found.unwrap().to_string_lossy().contains(".config"));
    }

    #[test]
    fn test_config_load_with_overrides() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("actup.toml"),
            r#"
update_policy = "minor"

[actions."special/action"]
update_policy = "patch"
"#,
        )
        .unwrap();

        let config = Config::load(None, temp.path()).unwrap();
        assert_eq!(config.policy_for("normal", "action").to_string(), "minor");
        assert_eq!(config.policy_for("special", "action").to_string(), "patch");
    }

    #[test]
    fn test_config_ignore_patterns() {
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("actup.toml"),
            r#"
ignore = ["internal/*", "deprecated/action"]
"#,
        )
        .unwrap();

        let config = Config::load(None, temp.path()).unwrap();
        assert!(config.should_ignore("internal", "anything"));
        assert!(config.should_ignore("deprecated", "action"));
        assert!(!config.should_ignore("external", "action"));
    }
}

mod version_tests {
    use actup_core::{Version, VersionInfo, VersionSet};
    use std::collections::HashMap;

    fn make_version_info(ver: &str, sha: &str) -> VersionInfo {
        VersionInfo {
            version: Version::parse(ver),
            sha: sha.to_string(),
        }
    }

    fn make_version_set(
        tags: Vec<VersionInfo>,
        latest: Option<VersionInfo>,
        latest_per_major: HashMap<u32, VersionInfo>,
    ) -> VersionSet {
        VersionSet {
            semver_tags: tags,
            latest_per_major,
            major_tags: HashMap::new(),
            latest,
            default_branch: None,
            fetched_at: None,
        }
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

        let current = Version::parse("v1.0.0");
        let latest = versions.find_latest_for(&current, false, false);

        assert!(latest.is_some());
        assert_eq!(latest.unwrap().version.to_string(), "v1.2.0");
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

        let current = Version::parse("v1.0.0");
        let latest = versions.find_latest_for(&current, true, false);

        assert!(latest.is_some());
        assert_eq!(latest.unwrap().version.to_string(), "v3.0.0");
    }

    #[test]
    fn test_version_parse_major_only() {
        let v = Version::parse("v4");
        assert!(v.is_semver());
        assert_eq!(v.major(), Some(4));
    }

    #[test]
    fn test_version_is_sha() {
        let sha = Version::parse("b4ffde65f46336ab88eb53be808477a3936bae11");
        assert!(sha.is_sha());
        assert!(!sha.is_semver());
    }
}
