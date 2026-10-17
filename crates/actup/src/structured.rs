//! JSON and TOML text for stdout that a GitHub Actions runner reads as no workflow command.

use std::borrow::Cow;

use actup_core::neutralize_workflow_commands;
use serde::Serialize;

fn escape_legacy_commands(json: String) -> String {
    let json = if json.contains('\u{7f}') {
        json.replace('\u{7f}', "\\u007f")
    } else {
        json
    };
    if json.contains("##[") {
        json.replace("##[", "##\\u005b")
    } else {
        json
    }
}

/// `value` as compact JSON, with DEL escaped as `\u007f` and `##[` as `##\u005b`.
pub fn json<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string(value).map(escape_legacy_commands)
}

/// `value` as pretty JSON, with DEL escaped as `\u007f` and `##[` as `##\u005b`.
pub fn json_pretty<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(value).map(escape_legacy_commands)
}

/// `value` as pretty TOML passed through [`neutralize_workflow_commands`].
pub fn toml<T: Serialize + ?Sized>(value: &T) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(value).map(|text| match neutralize_workflow_commands(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(neutralized) => neutralized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Report<'a> {
        file: &'a str,
        reference: &'a str,
    }

    const FORGED: Report<'static> = Report {
        file: "##[error]forged.yml",
        reference: "v\n::error::lf\r::warning::cr",
    };

    fn commands(text: &str) -> Vec<&str> {
        text.split(['\n', '\r'])
            .filter(|line| line.trim_start().starts_with("::") || line.contains("##["))
            .collect()
    }

    #[test]
    fn json_escapes_legacy_commands_and_keeps_the_value() -> Result<(), serde_json::Error> {
        for text in [json(&FORGED)?, json_pretty(&FORGED)?] {
            assert_eq!(commands(&text), Vec::<&str>::new(), "{text}");
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            assert_eq!(parsed["file"], FORGED.file);
            assert_eq!(parsed["reference"], FORGED.reference);
        }
        Ok(())
    }

    #[test]
    fn stripped_output_opens_no_command() -> Result<(), Box<dyn std::error::Error>> {
        let mut opened = Vec::new();
        for c in (0u32..=0x2070).filter_map(char::from_u32) {
            let value = format!("#{c}#[error]x");
            let reference = format!("v\n{c}::error::x");
            let report = Report {
                file: &value,
                reference: &reference,
            };
            for (kind, text) in [("json", json(&report)?), ("toml", toml(&report)?)] {
                let shown = anstream::adapter::strip_str(&text).to_string();
                if !commands(&shown).is_empty() {
                    opened.push(format!("{kind} U+{:04X}", u32::from(c)));
                }
            }
        }
        assert_eq!(opened, Vec::<String>::new());
        let del = json(&Report {
            file: "#\u{7f}#[error]x",
            reference: "v",
        })?;
        assert_eq!(del, r##"{"file":"#\u007f#[error]x","reference":"v"}"##);
        let parsed: serde_json::Value = serde_json::from_str(&del)?;
        assert_eq!(parsed["file"], "#\u{7f}#[error]x");
        Ok(())
    }

    #[test]
    fn toml_neutralizes_commands_and_stays_valid() -> Result<(), Box<dyn std::error::Error>> {
        let text = toml(&FORGED)?;
        assert_eq!(commands(&text), Vec::<&str>::new(), "{text}");
        let parsed: toml::Table = toml::from_str(&text)?;
        assert_eq!(parsed["file"].as_str(), Some("##\u{fffd}error]forged.yml"));
        assert_eq!(
            toml(&Report {
                file: "ci.yml",
                reference: "v1"
            })?,
            "file = \"ci.yml\"\nreference = \"v1\"\n"
        );
        Ok(())
    }
}
