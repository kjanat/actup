//! action.yml parsing utilities
//!
//! Provides raw deserialization types and parsing logic for GitHub Actions
//! action.yml files, shared across all provider implementations.

use actup_core::error::Result;
use actup_core::provider::{ActionInput, ActionOutput, ActionRuns, ActionStep, ActionYml};
use serde::Deserialize;
use std::collections::HashMap;

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

/// Parse action.yml content into `ActionYml` struct
///
/// # Errors
///
/// Returns error if the YAML content cannot be parsed.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
pub fn parse_action_yml(raw: &str) -> Result<ActionYml> {
    let parsed: RawActionYml =
        serde_yaml_ng::from_str(raw).map_err(|e| actup_core::Error::YamlParse {
            path: "action.yml".into(),
            reason: e.to_string(),
        })?;

    let inputs = parsed
        .inputs
        .into_iter()
        .map(|(k, v)| {
            let mut input = ActionInput::new();
            if let Some(desc) = v.description {
                input = input.with_description(desc);
            }
            if v.required {
                input = input.with_required(true);
            }
            if let Some(def) = v.default {
                input = input.with_default(def);
            }
            if let Some(msg) = v.deprecation_message {
                input = input.with_deprecation_message(msg);
            }
            (k, input)
        })
        .collect();

    let outputs = parsed
        .outputs
        .into_iter()
        .map(|(k, v)| {
            let mut output = ActionOutput::new();
            if let Some(desc) = v.description {
                output = output.with_description(desc);
            }
            if let Some(val) = v.value {
                output = output.with_value(val);
            }
            (k, output)
        })
        .collect();

    let runs = parsed
        .runs
        .map(|r| {
            let steps: Vec<ActionStep> = r
                .steps
                .into_iter()
                .map(|s| {
                    let mut step = ActionStep::new();
                    if let Some(name) = s.name {
                        step = step.with_name(name);
                    }
                    if let Some(id) = s.id {
                        step = step.with_id(id);
                    }
                    if let Some(uses) = s.uses {
                        step = step.with_uses(uses);
                    }
                    if let Some(run) = s.run {
                        step = step.with_run(run);
                    }
                    if let Some(shell) = s.shell {
                        step = step.with_shell(shell);
                    }
                    if let Some(wd) = s.working_directory {
                        step = step.with_working_directory(wd);
                    }
                    step
                })
                .collect();

            let mut runs = ActionRuns::new(&r.using);
            if let Some(main) = r.main.clone() {
                runs = runs.with_main(main);
            }
            if let Some(pre) = r.pre {
                runs = runs.with_pre(pre);
            }
            if let Some(post) = r.post {
                runs = runs.with_post(post);
            }
            if let Some(image) = r.image.clone() {
                runs = runs.with_image(image);
            }
            runs = runs.with_steps(steps.clone());

            // Validate runs based on using type
            let using = r.using.as_str();
            let has_main = r
                .main
                .as_deref()
                .is_some_and(|main| !main.trim().is_empty());
            if using.starts_with("node") && !has_main {
                return Err(actup_core::Error::YamlParse {
                    path: "action.yml".into(),
                    reason: format!("'runs.using: {using}' requires 'runs.main' to be set"),
                });
            }
            let has_image = r
                .image
                .as_deref()
                .is_some_and(|image| !image.trim().is_empty());
            if using == "docker" && !has_image {
                return Err(actup_core::Error::YamlParse {
                    path: "action.yml".into(),
                    reason: "'runs.using: docker' requires 'runs.image' to be set".into(),
                });
            }
            if using == "composite" && steps.is_empty() {
                return Err(actup_core::Error::YamlParse {
                    path: "action.yml".into(),
                    reason: "'runs.using: composite' requires at least one step".into(),
                });
            }

            Ok(runs)
        })
        .transpose()?;

    let mut yml = ActionYml::new(raw);
    if let Some(name) = parsed.name {
        yml = yml.with_name(name);
    }
    if let Some(desc) = parsed.description {
        yml = yml.with_description(desc);
    }
    if let Some(author) = parsed.author {
        yml = yml.with_author(author);
    }
    yml = yml.with_inputs(inputs).with_outputs(outputs);
    if let Some(runs) = runs {
        yml = yml.with_runs(runs);
    }

    Ok(yml)
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml).unwrap();
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
        let result = parse_action_yml(yaml);
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
        let result = parse_action_yml(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_invalid_yaml() {
        let yaml = "name: [invalid";
        let result = parse_action_yml(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_action_yml_preserves_raw() {
        let yaml = "name: Test\ndescription: Test";
        let result = parse_action_yml(yaml).unwrap();
        assert_eq!(result.raw(), yaml);
    }
}
