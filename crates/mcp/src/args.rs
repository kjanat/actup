//! Tool arguments, their JSON schemas and boundary validation.

use std::{collections::BTreeMap, num::NonZeroUsize};

use actup_core::{ActionRepoId, parse_action_path, policy, version::is_short_sha};
use rust_mcp_sdk::schema::ToolInputSchema;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Arguments of `actionGetReadme`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Readme {
    #[schemars(
        description = "Host serving the repository. Defaults to the configured default host."
    )]
    pub host: Option<String>,
    #[schemars(description = "Repository owner or group.")]
    pub owner: String,
    #[schemars(description = "Repository or project name.")]
    pub repo: String,
    #[serde(rename = "ref")]
    #[schemars(description = "Tag, branch or commit to read. Defaults to the default branch.")]
    pub reference: Option<String>,
}

/// Arguments of `actionGetActionYml`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionYml {
    #[schemars(
        description = "Host serving the repository. Defaults to the configured default host."
    )]
    pub host: Option<String>,
    #[schemars(description = "Repository owner or group.")]
    pub owner: String,
    #[schemars(description = "Repository or project name.")]
    pub repo: String,
    #[serde(rename = "ref")]
    #[schemars(description = "Tag, branch or commit to read. Defaults to the default branch.")]
    pub reference: Option<String>,
    #[schemars(
        description = "Directory of the action inside the repository, or the path of its action.yml."
    )]
    pub path: Option<String>,
}

/// Arguments of `getVersions`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Versions {
    #[schemars(
        description = "Host serving the repository. Defaults to the configured default host."
    )]
    pub host: Option<String>,
    #[schemars(description = "Repository owner or group.")]
    pub owner: String,
    #[schemars(description = "Repository or project name.")]
    pub repo: String,
    #[serde(default)]
    #[schemars(description = "Also list branches with their HEAD commits.")]
    pub include_branches: bool,
    #[serde(default)]
    #[schemars(description = "Also list prerelease tags such as v2.0.0-rc.1.")]
    pub include_prerelease: bool,
}

/// Arguments of `getLatest`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Latest {
    #[schemars(
        description = "Host serving the repository. Defaults to the configured default host."
    )]
    pub host: Option<String>,
    #[schemars(description = "Repository owner or group.")]
    pub owner: String,
    #[schemars(description = "Repository or project name.")]
    pub repo: String,
    #[schemars(description = "The ref after `@` in the `uses:` value.")]
    pub current_ref: String,
    #[schemars(description = "Bump level to use instead of the configured `policy.bump`.")]
    pub policy_override: Option<policy::Update>,
}

/// One ref replacement of `updateFile`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RefEdit {
    #[schemars(description = "1-based line holding the `uses:` value.")]
    pub line: NonZeroUsize,
    #[schemars(
        description = "The `uses:` value before `@`, as `[host/]owner/repo[/path]`, naming the reference to edit on that line."
    )]
    pub action: String,
    #[serde(rename = "ref")]
    #[schemars(
        description = "New ref to write after `@`: a tag, branch or full 40-character SHA."
    )]
    pub reference: String,
}

/// Arguments of `updateFile`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateFile {
    #[schemars(
        description = "Workflow or action file, relative to the server root or absolute inside it."
    )]
    pub path: String,
    #[schemars(description = "Ref replacements. Every edit applies or none does.")]
    pub edits: Vec<RefEdit>,
    #[serde(default = "yes")]
    #[schemars(
        description = "Refuse the write when the edited file is not valid YAML or holds a different number of `uses:` values."
    )]
    pub validate_yaml: bool,
    #[serde(default)]
    #[schemars(description = "Plan and report the edits without writing the file.")]
    pub dry_run: bool,
}

/// The action that `updateAllUsesToVersion` updates.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    #[schemars(
        description = "Host serving the repository. Defaults to the configured default host."
    )]
    pub host: Option<String>,
    #[schemars(description = "Repository owner or group.")]
    pub owner: String,
    #[schemars(description = "Repository or project name.")]
    pub repo: String,
    #[schemars(description = "Only references with this subpath. Omit to match every subpath.")]
    pub path: Option<String>,
}

/// Arguments of `updateAllUsesToVersion`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAll {
    #[schemars(
        description = "Directory to scan, relative to the server root or absolute inside it."
    )]
    pub workspace_root: String,
    #[serde(rename = "match")]
    #[schemars(description = "The action whose references to update.")]
    pub target: Target,
    #[schemars(
        description = "Tag or branch to update to. The provider resolves its commit for SHA-pinned references."
    )]
    pub target_ref: String,
    #[serde(default = "yes")]
    #[schemars(description = "Scan composite actions (`action.yml`).")]
    pub include_composite_actions: bool,
    #[serde(default = "yes")]
    #[schemars(description = "Scan workflows and the configured `scan.extra_paths`.")]
    pub include_workflows: bool,
    #[serde(default)]
    #[schemars(description = "Plan and report the edits without writing any file.")]
    pub dry_run: bool,
}

const fn yes() -> bool {
    true
}

/// The MCP input schema of `T`, with every subschema inlined.
#[must_use]
pub fn input_schema<T: JsonSchema>() -> ToolInputSchema {
    let schema = SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator()
        .into_root_schema_for::<T>();
    let object = schema.as_object();
    let properties = object
        .and_then(|o| o.get("properties"))
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .filter_map(|(name, prop)| Some((name.clone(), prop.as_object()?.clone())))
                .collect::<BTreeMap<_, _>>()
        });
    let required = object
        .and_then(|o| o.get("required"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    ToolInputSchema::new(required, properties, None)
}

/// Parse `[host/]owner/repo[/path]` from separate fields.
///
/// # Errors
///
/// Returns a message naming the field that does not form a valid reference.
pub fn repo_id(
    host: Option<&str>,
    owner: &str,
    repo: &str,
    path: Option<&str>,
) -> Result<ActionRepoId, String> {
    if let Some(path) = path
        && path.split('/').any(|s| matches!(s, "" | "." | ".."))
    {
        return Err(format!("path `{path}` must be `/`-separated names"));
    }
    let joined = [host, Some(owner), Some(repo), path]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("/");
    let id = parse_action_path(&joined).map_err(|e| e.to_string())?;
    let wanted_host = host.map(str::to_ascii_lowercase);
    if id.host() != wanted_host.as_deref() {
        return Err(host.map_or_else(
            || format!("owner `{owner}` reads as a host"),
            |host| format!("host `{host}` is not a hostname"),
        ));
    }
    if id.owner() != owner || id.repo() != repo || id.subpath() != path {
        return Err(format!(
            "owner `{owner}` and repo `{repo}` must be single names"
        ));
    }
    Ok(id)
}

/// Check that `value` can stand after `@` in a `uses:` value.
///
/// # Errors
///
/// Returns a message naming `field` when `value` is empty or holds a
/// character that git refs and plain YAML scalars do not allow.
pub fn check_ref(field: &str, value: &str) -> Result<(), String> {
    const FORBIDDEN: [char; 11] = ['@', '#', '\'', '"', '\\', '~', '^', ':', '?', '*', '['];
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if let Some(c) = value
        .chars()
        .find(|c| !c.is_ascii_graphic() || FORBIDDEN.contains(c))
    {
        return Err(format!("{field} `{value}` holds the character {c:?}"));
    }
    if value.contains("..") || value.starts_with(['-', '/']) || value.ends_with('/') {
        return Err(format!("{field} `{value}` is not a valid git ref"));
    }
    Ok(())
}

/// Check a ref that a mutator writes: [`check_ref`] and no abbreviated SHA.
///
/// # Errors
///
/// Returns a message naming `field` when `value` is not writable.
pub fn check_written_ref(field: &str, value: &str) -> Result<(), String> {
    check_ref(field, value)?;
    if is_short_sha(value) {
        return Err(format!(
            "{field} `{value}` is an abbreviated commit SHA; write the full 40-character SHA"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_schema_lists_required_fields_and_inlines_nested_types() {
        let schema = input_schema::<UpdateAll>();
        assert_eq!(schema.required, ["workspaceRoot", "match", "targetRef"]);
        let Some(props) = schema.properties else {
            panic!("schema has properties");
        };
        let Some(target) = props.get("match") else {
            panic!("match property");
        };
        assert!(target.contains_key("properties"), "{target:?}");
        assert!(!format!("{props:?}").contains("$ref"));

        let latest = input_schema::<Latest>();
        assert_eq!(latest.required, ["owner", "repo", "currentRef"]);
        let Some(props) = latest.properties else {
            panic!("schema has properties");
        };
        assert!(format!("{:?}", props.get("policyOverride")).contains("patch"));
    }

    #[test]
    fn repo_id_parses_host_owner_repo_and_path() -> Result<(), String> {
        let id = repo_id(Some("GHE.corp.com"), "org", "tools", Some("lint/go"))?;
        assert_eq!(id.host(), Some("ghe.corp.com"));
        assert_eq!((id.owner(), id.repo()), ("org", "tools"));
        assert_eq!(id.subpath(), Some("lint/go"));
        let bare = repo_id(None, "actions", "checkout", None)?;
        assert_eq!(bare.host(), None);
        Ok(())
    }

    #[test]
    fn repo_id_rejects_names_that_shift_segments() {
        assert!(repo_id(None, "a/b", "c", None).is_err());
        assert!(repo_id(None, "a", "b/c", None).is_err());
        assert!(repo_id(None, "a", "", None).is_err());
        assert!(repo_id(Some("not a host"), "a", "b", None).is_err());
        assert!(repo_id(None, "a.b", "c", Some("d")).is_err());
        assert!(repo_id(None, "a", "b", Some("../x")).is_err());
        assert!(repo_id(None, "a", "b", Some("x/")).is_err());
    }

    #[test]
    fn check_ref_accepts_tags_branches_and_shas() {
        for value in ["v4", "v4.2.0", "release/v1", "main", &"a".repeat(40)] {
            assert_eq!(check_ref("ref", value), Ok(()), "{value}");
        }
    }

    #[test]
    fn check_ref_rejects_unwritable_values() {
        for value in [
            "", "v1 x", "v1#x", "a@b", "x:y", "\"v1\"", "a..b", "-v1", "/v1", "v1/", "v1\n",
        ] {
            assert!(check_ref("ref", value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn check_written_ref_rejects_abbreviated_shas() {
        assert!(check_written_ref("ref", "abc1234").is_err());
        assert_eq!(check_written_ref("ref", &"a".repeat(40)), Ok(()));
        assert!(check_written_ref("ref", "").is_err());
    }
}
