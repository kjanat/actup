//! Pin command - pin all actions to their current SHA

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use actup_core::{
    ActionRef, Config, ProviderRegistry, UsesLocation, discover_files, extract_uses_from_yaml,
    parse_uses,
};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;

/// Get the prefix of an action ref (everything before @ref)
fn action_prefix(action: &ActionRef) -> String {
    let mut s = String::new();
    if let Some(h) = &action.host {
        s.push_str(h);
        s.push('/');
    }
    s.push_str(&action.owner);
    s.push('/');
    s.push_str(&action.repo);
    if let Some(p) = &action.subpath {
        s.push('/');
        s.push_str(p);
    }
    s
}

/// Information about a pin to apply
struct PinInfo {
    /// The UsesLocation for targeted replacement
    loc: UsesLocation,
    /// Old uses value
    old_uses: String,
    /// New uses value (with SHA)
    new_uses: String,
    /// Comment with original ref
    comment: String,
}

pub async fn run(
    path: &Path,
    dry_run: bool,
    format: OutputFormat,
    config: &Config,
    registry: &ProviderRegistry,
) -> i32 {
    let discovered = discover_files(path, &config.extra_paths);

    if discovered.is_empty() {
        if matches!(format, OutputFormat::Human) {
            println!(
                "{}",
                "No workflow files found in .github/workflows/".dimmed()
            );
        }
        return 0;
    }

    let mut total_pins = 0;
    let mut errors = 0;

    for file in &discovered {
        let content = match fs::read_to_string(&file.path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), file.path.display(), e);
                continue;
            }
        };

        let uses_locations = extract_uses_from_yaml(&content);

        // Collect pins needed: line -> PinInfo
        let mut pins: HashMap<usize, PinInfo> = HashMap::new();

        for loc in uses_locations {
            let action_ref = match parse_uses(&loc.value) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => continue,
            };

            // Skip ignored actions
            if config.should_ignore(&action_ref.owner, &action_ref.repo) {
                continue;
            }

            // Skip already pinned (SHA)
            if action_ref.is_sha() {
                continue;
            }

            let provider = match registry.find(action_ref.host.as_deref()) {
                Some(p) => p,
                None => continue,
            };

            let repo_id = action_ref.to_repo_id();

            // Find the SHA for the current ref
            let versions = match provider.get_versions(&repo_id).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                    errors += 1;
                    continue;
                }
            };

            // Find version matching current ref
            let version_info = versions
                .semver_tags
                .iter()
                .find(|v| v.version.to_string() == action_ref.ref_str);

            let sha = if let Some(v) = version_info {
                v.sha.clone()
            } else {
                // Try to resolve the ref directly (might be a branch)
                match provider
                    .resolve_ref_to_sha(&repo_id, &action_ref.ref_str)
                    .await
                {
                    Ok(sha40) => sha40.as_str().to_string(),
                    Err(e) => {
                        eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                        errors += 1;
                        continue;
                    }
                }
            };

            let old_uses = action_ref.to_string();
            let prefix = action_prefix(&action_ref);
            let new_uses = format!("{}@{}", prefix, sha);
            let comment = format!(" # {}", action_ref.ref_str);

            pins.insert(
                loc.line,
                PinInfo {
                    loc,
                    old_uses,
                    new_uses,
                    comment,
                },
            );
        }

        if pins.is_empty() {
            continue;
        }

        // Read file and apply pins using targeted replacement
        let lines: Vec<&str> = content.lines().collect();
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());

        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1;
            if let Some(pin) = pins.get(&line_num) {
                // Use value_range for targeted replacement of the entire uses value
                let range = pin.loc.value_range();
                let mut new_line = String::with_capacity(line.len() + 60);
                new_line.push_str(&line[..range.start]);
                new_line.push_str(&pin.new_uses.split('@').skip(1).next().map_or_else(
                    || pin.new_uses.clone(),
                    |sha| {
                        // Reconstruct with prefix from original value
                        format!(
                            "{}@{}",
                            pin.loc.value.split('@').next().unwrap_or(&pin.loc.value),
                            sha
                        )
                    },
                ));
                new_line.push_str(&line[range.end..]);

                // Add comment if not already present
                if !new_line.contains('#') {
                    new_line.push_str(&pin.comment);
                }

                new_lines.push(new_line);

                let file_str = file.path.strip_prefix(path).unwrap_or(&file.path).display();

                if matches!(format, OutputFormat::Human) {
                    println!(
                        "{}:{}: {} -> {}",
                        file_str,
                        line_num,
                        pin.old_uses.yellow(),
                        format!("{}{}", pin.new_uses, pin.comment).cyan()
                    );
                }
                total_pins += 1;
            } else {
                new_lines.push(line.to_string());
            }
        }

        let new_content = if content.ends_with('\n') {
            new_lines.join("\n") + "\n"
        } else {
            new_lines.join("\n")
        };

        if !dry_run {
            if let Err(e) = fs::write(&file.path, new_content) {
                eprintln!(
                    "{}: {} - {}",
                    "Error writing file".red(),
                    file.path.display(),
                    e
                );
                errors += 1;
            }
        }
    }

    if matches!(format, OutputFormat::Human) {
        println!();
        if dry_run {
            println!(
                "{}",
                format!("Would pin {} action reference(s) to SHA", total_pins).bold()
            );
        } else {
            println!(
                "{}",
                format!("Pinned {} action reference(s) to SHA", total_pins).bold()
            );
        }
        if errors > 0 {
            println!("{}", format!("{} error(s) occurred", errors).red());
        }
    }

    if errors > 0 {
        3
    } else {
        // Exit 0 for pin command (unlike check/update which exit 1 on dry-run changes)
        // Pin is a deliberate action, not a check for compliance
        0
    }
}
