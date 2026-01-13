//! Update command - update actions to their latest versions

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use actup_core::{discover_files, extract_uses_from_yaml, parse_uses, Config, ProviderRegistry};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;

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

    let mut total_updates = 0;
    let mut errors = 0;

    for file in &discovered {
        let content = match fs::read_to_string(&file.path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "{}: {} - {}",
                    "Warning".yellow(),
                    file.path.display(),
                    e
                );
                continue;
            }
        };

        let uses_locations = extract_uses_from_yaml(&content);

        // Collect updates needed for this file: line -> (old_ref, new_ref)
        let mut updates: HashMap<usize, (String, String)> = HashMap::new();

        for loc in &uses_locations {
            let action_ref = match parse_uses(&loc.value) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(_) => continue,
            };

            // Skip SHAs
            if action_ref.is_sha() {
                continue;
            }

            let provider = match registry.find(action_ref.host.as_deref()) {
                Some(p) => p,
                None => continue,
            };

            let repo_id = action_ref.to_repo_id();
            let versions = match provider.get_versions(&repo_id).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                    errors += 1;
                    continue;
                }
            };

            if let Some(latest) = versions.latest.as_ref() {
                if latest.version.to_string() != action_ref.ref_str {
                    updates.insert(
                        loc.line,
                        (action_ref.ref_str.clone(), latest.version.to_string()),
                    );
                }
            }
        }

        if updates.is_empty() {
            continue;
        }

        // Read file and apply updates
        let lines: Vec<&str> = content.lines().collect();
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());

        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1; // 1-indexed
            if let Some((old_ref, new_ref)) = updates.get(&line_num) {
                let new_line = line.replace(old_ref, new_ref);
                new_lines.push(new_line);

                let file_str = file.path.strip_prefix(path).unwrap_or(&file.path).display();

                if matches!(format, OutputFormat::Human) {
                    println!(
                        "{}:{}: {} -> {}",
                        file_str,
                        line_num,
                        old_ref.red(),
                        new_ref.green()
                    );
                }
                total_updates += 1;
            } else {
                new_lines.push(line.to_string());
            }
        }

        // Preserve trailing newline if original had one
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
                format!("Would update {} action reference(s)", total_updates).bold()
            );
        } else {
            println!(
                "{}",
                format!("Updated {} action reference(s)", total_updates).bold()
            );
        }
        if errors > 0 {
            println!("{}", format!("{} error(s) occurred", errors).red());
        }
    }

    if errors > 0 {
        3
    } else if total_updates > 0 && dry_run {
        1 // Exit 1 if changes would be made (useful for CI)
    } else {
        0
    }
}
