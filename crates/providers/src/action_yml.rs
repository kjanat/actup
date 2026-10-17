//! action.yml parsing utilities
//!
//! Provides raw deserialization types and parsing logic for GitHub Actions
//! action.yml files, shared across all provider implementations.

use std::collections::HashMap;

use actup_core::{
    error::Result,
    provider::{ActionInput, ActionOutput, ActionRuns, ActionStep, ActionYml},
    yaml,
};
use serde::Deserialize;

/// Raw deserialization type for action.yml
#[derive(Debug, Deserialize, Default)]
pub struct RawActionYml {
    /// Action name
    pub name: Option<String>,
    /// Action description
    pub description: Option<String>,
    /// Action author
    pub author: Option<String>,
    /// Action inputs
    #[serde(default)]
    pub inputs: HashMap<String, RawInput>,
    /// Action outputs
    #[serde(default)]
    pub outputs: HashMap<String, RawOutput>,
    /// Action runs configuration
    pub runs: Option<RawRuns>,
}

/// Raw deserialization type for action input
#[derive(Debug, Deserialize, Default)]
pub struct RawInput {
    /// Input description
    pub description: Option<String>,
    /// Whether input is required
    #[serde(default)]
    pub required: bool,
    /// Default value
    pub default: Option<String>,
    /// Deprecation message
    #[serde(rename = "deprecationMessage")]
    pub deprecation_message: Option<String>,
}

/// Raw deserialization type for action output
#[derive(Debug, Deserialize, Default)]
pub struct RawOutput {
    /// Output description
    pub description: Option<String>,
    /// Output value expression
    pub value: Option<String>,
}

/// Raw deserialization type for action runs
#[derive(Debug, Deserialize)]
pub struct RawRuns {
    /// Runtime (node20, composite, docker, etc.)
    pub using: String,
    /// Main entry point (Node.js actions)
    pub main: Option<String>,
    /// Pre-run script
    pub pre: Option<String>,
    /// Post-run script
    pub post: Option<String>,
    /// Docker image (Docker actions)
    pub image: Option<String>,
    /// Steps (composite actions)
    #[serde(default)]
    pub steps: Vec<RawStep>,
}

/// Raw deserialization type for composite action step
#[derive(Debug, Deserialize, Default)]
pub struct RawStep {
    /// Step name
    pub name: Option<String>,
    /// Step ID
    pub id: Option<String>,
    /// Action to use
    pub uses: Option<String>,
    /// Command to run
    pub run: Option<String>,
    /// Shell for run command
    pub shell: Option<String>,
    /// Working directory
    #[serde(rename = "working-directory")]
    pub working_directory: Option<String>,
}

fn yaml_err(path: &str, reason: impl Into<String>) -> actup_core::Error {
    actup_core::Error::YamlParse {
        path: path.into(),
        reason: reason.into(),
        source: None,
    }
}

/// Chainable helper: apply `setter` iff `value` is `Some`. Removes the
/// repetitive `if let Some(v) = opt { b = b.with_x(v); }` idiom at each
/// builder step.
fn set_if_some<B, T>(builder: B, value: Option<T>, setter: impl FnOnce(B, T) -> B) -> B {
    match value {
        Some(v) => setter(builder, v),
        None => builder,
    }
}

impl From<RawInput> for ActionInput {
    fn from(raw: RawInput) -> Self {
        let b = Self::new().with_required(raw.required);
        let b = set_if_some(b, raw.description, Self::with_description);
        let b = set_if_some(b, raw.default, Self::with_default);
        set_if_some(b, raw.deprecation_message, Self::with_deprecation_message)
    }
}

impl From<RawOutput> for ActionOutput {
    fn from(raw: RawOutput) -> Self {
        let b = Self::new();
        let b = set_if_some(b, raw.description, Self::with_description);
        set_if_some(b, raw.value, Self::with_value)
    }
}

impl From<RawStep> for ActionStep {
    fn from(raw: RawStep) -> Self {
        let b = Self::new();
        let b = set_if_some(b, raw.name, Self::with_name);
        let b = set_if_some(b, raw.id, Self::with_id);
        let b = set_if_some(b, raw.uses, Self::with_uses);
        let b = set_if_some(b, raw.run, Self::with_run);
        let b = set_if_some(b, raw.shell, Self::with_shell);
        set_if_some(b, raw.working_directory, Self::with_working_directory)
    }
}

/// Is `using` a Node.js runtime spec?
///
/// Matches `node` followed by at least one ASCII digit (e.g. `node16`,
/// `node20`, `node24`). Rejects prefix-only matches like `nodeify` or
/// `node-custom` that the previous `starts_with("node")` check let through.
fn is_node_runtime(using: &str) -> bool {
    using
        .strip_prefix("node")
        .is_some_and(|tail| !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()))
}

/// Validate the invariants GitHub enforces for `runs.using`.
///
/// - `node*`: `main` must be present and non-blank.
/// - `docker`: `image` must be present and non-blank.
/// - `composite`: at least one step.
///
/// `path` is the filename (e.g. `action.yml` or `action.yaml`) surfaced in
/// error messages so callers trying both variants produce accurate output.
fn validate_runs(
    path: &str,
    using: &str,
    main: Option<&str>,
    image: Option<&str>,
    steps: &[ActionStep],
) -> Result<()> {
    let non_blank = |s: Option<&str>| s.is_some_and(|t| !t.trim().is_empty());

    if is_node_runtime(using) && !non_blank(main) {
        return Err(yaml_err(
            path,
            format!("'runs.using: {using}' requires 'runs.main' to be set"),
        ));
    }
    if using == "docker" && !non_blank(image) {
        return Err(yaml_err(
            path,
            "'runs.using: docker' requires 'runs.image' to be set",
        ));
    }
    if using == "composite" && steps.is_empty() {
        return Err(yaml_err(
            path,
            "'runs.using: composite' requires at least one step",
        ));
    }
    Ok(())
}

/// Build an [`ActionRuns`] from raw deserialized form.
///
/// Free function (rather than `TryFrom`) because the error case needs the
/// file path for accurate diagnostics, and `TryFrom::try_from` takes only
/// the value.
fn build_runs(path: &str, raw: RawRuns) -> Result<ActionRuns> {
    let steps: Vec<ActionStep> = raw.steps.into_iter().map(Into::into).collect();
    validate_runs(
        path,
        &raw.using,
        raw.main.as_deref(),
        raw.image.as_deref(),
        &steps,
    )?;

    let b = ActionRuns::new(&raw.using).with_steps(steps);
    let b = set_if_some(b, raw.main, ActionRuns::with_main);
    let b = set_if_some(b, raw.pre, ActionRuns::with_pre);
    let b = set_if_some(b, raw.post, ActionRuns::with_post);
    let b = set_if_some(b, raw.image, ActionRuns::with_image);
    Ok(b)
}

/// Build an [`ActionYml`] from raw deserialized form.
///
/// `source` is the original YAML text (stored in the returned struct so
/// `.raw()` keeps working); `path` is the filename used in error messages.
fn build_action_yml(path: &str, source: &str, raw: RawActionYml) -> Result<ActionYml> {
    let inputs = raw.inputs.into_iter().map(|(k, v)| (k, v.into())).collect();
    let outputs = raw
        .outputs
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect();
    let runs = raw.runs.map(|r| build_runs(path, r)).transpose()?;

    let b = ActionYml::new(source)
        .with_inputs(inputs)
        .with_outputs(outputs);
    let b = set_if_some(b, raw.name, ActionYml::with_name);
    let b = set_if_some(b, raw.description, ActionYml::with_description);
    let b = set_if_some(b, raw.author, ActionYml::with_author);
    Ok(set_if_some(b, runs, ActionYml::with_runs))
}

/// Parse action.yml / action.yaml content into [`ActionYml`].
///
/// `path` is the filename (e.g. `"action.yml"` or `"action.yaml"`) used
/// in error messages — GitHub accepts both extensions, and callers that
/// fall back from one to the other pass the filename they actually loaded
/// so diagnostics don't lie.
///
/// # Errors
///
/// Returns error if the YAML content cannot be parsed or if `runs.using`
/// invariants are violated (missing `main`/`image`/empty `steps` per type).
pub fn parse_action_yml(raw: &str, path: &str) -> Result<ActionYml> {
    let parsed: RawActionYml =
        yaml::from_str(raw, yaml::Dialect::Runner).map_err(|e| actup_core::Error::YamlParse {
            path: path.into(),
            reason: String::new(),
            source: Some(std::sync::Arc::new(e)),
        })?;
    build_action_yml(path, raw, parsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_action_yml_minimal() {
        let yaml = r"
name: Test Action
description: A test action
";
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert_eq!(result.name(), Some("Test Action"));
        assert_eq!(result.description(), Some("A test action"));
        assert!(result.author().is_none());
    }

    #[test]
    fn test_parse_action_yml_with_inputs() {
        let yaml = r#"
name: Test
description: Test
inputs:
  token:
    description: The GitHub token
    required: true
  timeout:
    description: Timeout in seconds
    required: false
    default: "30"
"#;
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert_eq!(result.inputs().len(), 2);

        let token = result.inputs().get("token").unwrap();
        assert_eq!(token.description(), Some("The GitHub token"));
        assert!(token.required());
        assert!(token.default().is_none());

        let timeout = result.inputs().get("timeout").unwrap();
        assert!(!timeout.required());
        assert_eq!(timeout.default(), Some("30"));
    }

    #[test]
    fn test_parse_action_yml_with_outputs() {
        let yaml = r"
name: Test
description: Test
outputs:
  result:
    description: The result value
    value: ${{ steps.run.outputs.result }}
";
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert_eq!(result.outputs().len(), 1);

        let output = result.outputs().get("result").unwrap();
        assert_eq!(output.description(), Some("The result value"));
        assert!(output.value().is_some());
    }

    #[test]
    fn test_parse_action_yml_node_action() {
        let yaml = r"
name: Node Action
description: A Node.js action
runs:
  using: node20
  main: dist/index.js
";
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert!(result.runs().is_some());

        let runs = result.runs().unwrap();
        assert_eq!(runs.using(), "node20");
        assert_eq!(runs.main(), Some("dist/index.js"));
    }

    #[test]
    fn test_parse_action_yml_composite_action() {
        let yaml = r#"
name: Composite Action
description: A composite action
runs:
  using: composite
  steps:
    - name: Checkout
      uses: actions/checkout@v4
    - name: Run script
      run: echo "Hello"
      shell: bash
"#;
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert!(result.runs().is_some());

        let runs = result.runs().unwrap();
        assert_eq!(runs.using(), "composite");
        assert_eq!(runs.steps().len(), 2);

        assert_eq!(runs.steps()[0].name(), Some("Checkout"));
        assert_eq!(runs.steps()[0].uses(), Some("actions/checkout@v4"));

        assert_eq!(runs.steps()[1].name(), Some("Run script"));
        assert_eq!(runs.steps()[1].run(), Some("echo \"Hello\""));
        assert_eq!(runs.steps()[1].shell(), Some("bash"));
    }

    #[test]
    fn test_parse_action_yml_docker_action() {
        let yaml = r"
name: Docker Action
description: A Docker action
runs:
  using: docker
  image: Dockerfile
";
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert!(result.runs().is_some());

        let runs = result.runs().unwrap();
        assert_eq!(runs.using(), "docker");
        assert_eq!(runs.image(), Some("Dockerfile"));
    }

    #[test]
    fn test_parse_action_yml_node_action_rejects_blank_main() {
        let yaml = r#"
name: Bad Node Action
description: Invalid node action
runs:
  using: node20
  main: "   "
"#;
        let result = parse_action_yml(yaml, "action.yml");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_docker_action_rejects_blank_image() {
        let yaml = r#"
name: Bad Docker Action
description: Invalid docker action
runs:
  using: docker
  image: "   "
"#;
        let result = parse_action_yml(yaml, "action.yml");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_invalid_yaml() {
        let yaml = "name: [invalid";
        let result = parse_action_yml(yaml, "action.yml");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_preserves_raw() {
        let yaml = "name: Test\ndescription: Test";
        let result = parse_action_yml(yaml, "action.yml").unwrap();
        assert_eq!(result.raw(), yaml);
    }

    #[test]
    fn test_raw_input_into_action_input() {
        let raw = RawInput {
            description: Some("token".into()),
            required: true,
            default: None,
            deprecation_message: Some("use foo".into()),
        };
        let input: ActionInput = raw.into();
        assert_eq!(input.description(), Some("token"));
        assert!(input.required());
        assert!(input.default().is_none());
        assert_eq!(input.deprecation_message(), Some("use foo"));
    }

    #[test]
    fn test_build_runs_validates_composite() {
        let raw = RawRuns {
            using: "composite".into(),
            main: None,
            pre: None,
            post: None,
            image: None,
            steps: Vec::new(),
        };
        assert!(build_runs("action.yml", raw).is_err());
    }

    #[test]
    fn test_is_node_runtime_requires_digits_after_node() {
        // Canonical values that must match.
        for v in ["node16", "node20", "node24", "node9"] {
            assert!(is_node_runtime(v), "expected {v} to match is_node_runtime");
        }
        // Must NOT match: bare "node", letters-after, hyphenated, etc.
        for v in [
            "node",
            "nodeify",
            "nodejs",
            "node-20",
            "NODE20",
            "composite",
            "docker",
        ] {
            assert!(
                !is_node_runtime(v),
                "expected {v} to NOT match is_node_runtime"
            );
        }
    }

    #[test]
    fn test_yaml_err_uses_caller_supplied_path() {
        // Regression: yaml_err previously hardcoded "action.yml", so parsing
        // an action.yaml file would produce error messages claiming .yml.
        // Assert against the structured Error variant — comparing formatted
        // strings is fragile because "action.yaml" contains "action.yml".
        let yaml = r"
name: Bad
description: Bad
runs:
  using: docker
";
        let err = parse_action_yml(yaml, "action.yaml").unwrap_err();
        match err {
            actup_core::Error::YamlParse { path, .. } => {
                assert_eq!(path, "action.yaml", "error should carry the caller's path");
            }
            other => panic!("expected YamlParse error, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_yaml_keeps_the_parser_error_as_source() {
        let Err(err) = parse_action_yml("name: [unclosed", "action.yml") else {
            panic!("expected a parse error");
        };
        let Some(source) = std::error::Error::source(&err) else {
            panic!("expected a source on {err:?}");
        };
        assert_eq!(err.to_string(), "failed to parse YAML 'action.yml'");
        assert_eq!(
            actup_core::error::chain(&err),
            format!("failed to parse YAML 'action.yml': {source}")
        );
    }

    fn input(text: &str) -> std::result::Result<ActionInput, Box<dyn std::error::Error>> {
        let parsed = parse_action_yml(&format!("inputs:\n  t:\n{text}"), "action.yml")?;
        parsed
            .inputs()
            .get("t")
            .cloned()
            .ok_or_else(|| "missing input t".into())
    }

    fn rejected(yaml: &str) -> String {
        let Err(err) = parse_action_yml(yaml, "action.yml") else {
            panic!("{yaml:?} must be rejected");
        };
        actup_core::error::chain(&err)
    }

    #[test]
    fn scalars_follow_the_core_schema() -> std::result::Result<(), Box<dyn std::error::Error>> {
        for (text, default) in [
            ("30", Some("30")),
            ("0x1F", Some("0x1F")),
            ("1.50", Some("1.50")),
            (".inf", Some(".inf")),
            ("true", Some("true")),
            ("yes", Some("yes")),
            ("ON", Some("ON")),
            ("tRUE", Some("tRUE")),
            ("'~'", Some("~")),
            ("!!str 12", Some("12")),
            ("~", None),
            ("Null", None),
            ("", None),
        ] {
            let parsed = input(&format!("    default: {text}\n"))?;
            assert_eq!(parsed.default(), default, "{text:?}");
        }
        for (text, required) in [("true", true), ("True", true), ("FALSE", false)] {
            let parsed = input(&format!("    required: {text}\n"))?;
            assert_eq!(parsed.required(), required, "{text}");
        }
        let parsed = parse_action_yml("name: ON\ndescription: |\n  true\n", "action.yml")?;
        assert_eq!(parsed.name(), Some("ON"));
        assert_eq!(parsed.description(), Some("true\n"));
        Ok(())
    }

    #[test]
    fn non_bool_required_is_rejected_with_its_location() {
        for text in ["yes", "tRUE", "on", "'true'", "1"] {
            let reason = rejected(&format!("inputs:\n  t:\n    required: {text}\n"));
            assert!(reason.contains("expected a boolean"), "{text}: {reason}");
            assert!(reason.contains("at line 3, column 15"), "{text}: {reason}");
        }
    }

    #[test]
    fn structural_errors_name_line_and_column_on_one_line() {
        for (yaml, needle) in [
            ("name: x\nruns: {using: [x}\n", "at line 2, column 17"),
            (
                "inputs:\n  a: &x\n    description: d\n  b: *x\n",
                "anchors are not supported at line 3, column 5",
            ),
            (
                "name: &n x\n",
                "anchors are not supported at line 1, column 10",
            ),
            (
                "x: &b {name: n}\n<<: *b\n",
                "anchors are not supported at line 1, column 7",
            ),
            (
                "<<: {name: n}\n",
                "merge keys are not supported at line 1, column 1",
            ),
            (
                "name: a\nname: b\n",
                "duplicate key `name` at line 2, column 1",
            ),
            (
                "name: !custom x\n",
                "tag `!custom` is not a core schema tag at line 1, column 15",
            ),
            (
                "name: !!int x\n",
                "`x` does not fit tag `!!int` at line 1, column 13",
            ),
            (
                "name: a\n---\nname: b\n",
                "more than one YAML document at line 2, column 1",
            ),
            (
                "inputs:\n  Token: {}\n  token: {}\n",
                "duplicate key `token` at line 3, column 3",
            ),
            (
                "inputs:\n  \u{1F88}: {}\n  \u{1F80}: {}\n",
                "duplicate key `\u{1F80}` at line 3, column 3",
            ),
            (
                "inputs:\n  t:\n    default: !!int \"1\"\n",
                "tag `!!int` needs a plain scalar at line 3, column 20",
            ),
            (
                "inputs:\n  t:\n    default: 0x100000000\n",
                "`0x100000000` does not fit in 32 bits at line 3, column 14",
            ),
            (
                &format!("runs:\n  steps: {}x{}\n", "[".repeat(200), "]".repeat(200)),
                "nest deeper than 100 levels at line 2, column 108",
            ),
        ] {
            let reason = rejected(yaml);
            assert!(reason.contains(needle), "{yaml:?}: {reason}");
            assert!(!reason.contains('\n'), "{reason}");
        }
    }

    #[test]
    fn runner_rules_accept_what_the_runner_accepts()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let parsed = parse_action_yml(
            "inputs: !custom\n  i: {}\n  \u{0131}: {}\n  s: {}\n  \u{017F}: {}\n  ${{ a }}: {}\n  ${{ A }}: {}\n",
            "action.yml",
        )?;
        assert_eq!(parsed.inputs().len(), 6);
        let parsed = parse_action_yml("inputs:\n  t:\n    default: 0xFFFFFFFF\n", "action.yml")?;
        assert_eq!(
            parsed.inputs().get("t").and_then(ActionInput::default),
            Some("0xFFFFFFFF")
        );
        Ok(())
    }

    #[test]
    fn byte_order_mark_and_empty_files_parse() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let parsed = parse_action_yml("\u{feff}name: bom\n", "action.yml")?;
        assert_eq!(parsed.name(), Some("bom"));
        let parsed = parse_action_yml("# only a comment\n", "action.yml")?;
        assert_eq!(parsed.name(), None);
        Ok(())
    }
}
