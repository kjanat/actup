//! Update command - update actions to their latest versions

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use actup_core::{
    Config, ProviderRegistry, UsesLocation, Version, discover_files, extract_uses_from_yaml,
    parse_uses,
};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;

/// Information about an update to apply
struct UpdateInfo {
    /// The UsesLocation for targeted replacement
    loc: UsesLocation,
    /// Old ref string
    old_ref: String,
    /// New ref string
    new_ref: String,
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

    let mut total_updates = 0;
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

        // Collect updates needed for this file: line -> UpdateInfo
        let mut updates: HashMap<usize, UpdateInfo> = HashMap::new();

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

            // Get effective policy for this action
            let policy = config.policy_for(&action_ref.owner, &action_ref.repo);
            let current_version = Version::parse(&action_ref.ref_str);

            // Find target version based on policy
            let target = versions.find_latest_for(
                &current_version,
                policy.allows_major(),
                config.allow_prerelease,
            );

            if let Some(latest) = target {
                // Check if update is needed (handles v4 == v4.1.7 case)
                let needs_update = match &current_version {
                    Version::Semver {
                        major,
                        minor: None,
                        patch: None,
                        ..
                    } => latest.version.major() != Some(*major),
                    _ => latest.version.to_string() != action_ref.ref_str,
                };

                if needs_update {
                    updates.insert(
                        loc.line,
                        UpdateInfo {
                            loc,
                            old_ref: action_ref.ref_str.clone(),
                            new_ref: latest.version.to_string(),
                        },
                    );
                }
            }
        }

        if updates.is_empty() {
            continue;
        }

        // Apply updates using targeted byte ranges to avoid corrupting comments
        let lines: Vec<&str> = content.lines().collect();
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());

        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1; // 1-indexed
            if let Some(update) = updates.get(&line_num) {
                // Use ref_range for targeted replacement (only replaces the @ref portion)
                let new_line = if let Some(range) = update.loc.ref_range() {
                    // Replace only the ref portion using byte range
                    let mut result = String::with_capacity(line.len());
                    result.push_str(&line[..range.start]);
                    result.push_str(&update.new_ref);
                    result.push_str(&line[range.end..]);
                    result
                } else {
                    // Fallback: replace first occurrence of old ref after @
                    let at_pattern = format!("@{}", update.old_ref);
                    let new_pattern = format!("@{}", update.new_ref);
                    line.replacen(&at_pattern, &new_pattern, 1)
                };
                new_lines.push(new_line);

                let file_str = file.path.strip_prefix(path).unwrap_or(&file.path).display();

                if matches!(format, OutputFormat::Human) {
                    println!(
                        "{}:{}: {} -> {}",
                        file_str,
                        line_num,
                        update.old_ref.red(),
                        update.new_ref.green()
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
