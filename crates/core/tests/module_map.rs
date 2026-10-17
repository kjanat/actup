//! Checks that `crates/core/AGENTS.md` has a Module Map row for every module.

use std::{fs, path::Path};

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

struct Section {
    heading: String,
    cells: Vec<String>,
}

fn sections(agents: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut fenced = false;
    for line in agents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            sections.push(Section {
                heading: line.to_owned(),
                cells: Vec::new(),
            });
        } else if !fenced
            && let Some(cell) = line
                .strip_prefix("| `")
                .and_then(|rest| rest.split_once('`'))
                .map(|(cell, _)| cell.to_owned())
            && let Some(section) = sections.last_mut()
        {
            section.cells.push(cell);
        }
    }
    sections
}

fn table<'a>(sections: &'a [Section], dir: &str) -> Option<&'a [String]> {
    sections
        .iter()
        .find(|section| {
            if dir.is_empty() {
                section.heading == "## Module Map"
            } else {
                section.heading.contains(&format!("(`{dir}/`)"))
            }
        })
        .map(|section| section.cells.as_slice())
}

fn missing_rows(agents: &str, src: &Path) -> TestResult<Vec<String>> {
    let sections = sections(agents);
    let mut missing = Vec::new();
    walk(&sections, src, "", &mut missing)?;
    Ok(missing)
}

fn walk(sections: &[Section], src: &Path, dir: &str, missing: &mut Vec<String>) -> TestResult<()> {
    let shown = if dir.is_empty() {
        "src/".to_owned()
    } else {
        format!("src/{dir}/")
    };
    let rows = table(sections, dir);
    if rows.is_none() {
        missing.push(format!("{shown} has no table"));
    }
    let listed = |cell: &str| rows.is_some_and(|rows| rows.iter().any(|row| row == cell));

    let mut entries: Vec<_> = fs::read_dir(src.join(dir))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            let child = if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            };
            if !listed(&format!("{name}/")) {
                missing.push(format!("src/{child}/"));
            }
            walk(sections, src, &child, missing)?;
        } else if let Some(module) = name.strip_suffix(".rs") {
            let crate_root = dir.is_empty() && module == "lib";
            let cell = if dir.is_empty() { module } else { name };
            if !crate_root && !listed(cell) {
                missing.push(format!("{shown}{name}"));
            }
        }
    }
    Ok(())
}

#[test]
fn agents_module_map_lists_every_module() -> TestResult<()> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agents = fs::read_to_string(crate_dir.join("AGENTS.md"))?;
    let missing = missing_rows(&agents, &crate_dir.join("src"))?;
    assert!(
        missing.is_empty(),
        "crates/core/AGENTS.md has no Module Map row for {missing:?}"
    );
    Ok(())
}

#[test]
fn module_map_check_reads_each_directory_table_at_every_level() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path();
    for dir in ["extra", "config/nested"] {
        fs::create_dir_all(src.join(dir))?;
    }
    for file in [
        "lib.rs",
        "a.rs",
        "extra/mod.rs",
        "extra/resolution.rs",
        "config/mod.rs",
        "config/resolution.rs",
        "config/nested/deep.rs",
    ] {
        fs::write(src.join(file), "")?;
    }
    let agents = "## Module Map\n\n| Module | Purpose |\n| --- | --- |\n| `a` | A |\n| `extra/` | Extra |\n| `config/` | Config |\n\n### Config (`config/`)\n\n| File | Purpose |\n| --- | --- |\n| `mod.rs` | Mod |\n| `resolution.rs` | Resolution |\n";

    assert_eq!(
        missing_rows(agents, src)?,
        [
            "src/config/nested/",
            "src/config/nested/ has no table",
            "src/config/nested/deep.rs",
            "src/extra/ has no table",
            "src/extra/mod.rs",
            "src/extra/resolution.rs",
        ]
    );

    let complete = format!(
        "{agents}| `nested/` | Nested |\n\n### Nested (`config/nested/`)\n\n| File | Purpose |\n| --- | --- |\n| `deep.rs` | Deep |\n\n### Extra (`extra/`)\n\n| File | Purpose |\n| --- | --- |\n| `mod.rs` | Mod |\n| `resolution.rs` | Resolution |\n"
    );
    assert_eq!(missing_rows(&complete, src)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn module_map_check_skips_fenced_code_blocks() -> TestResult<()> {
    let temp = tempfile::tempdir()?;
    let src = temp.path();
    fs::create_dir(src.join("config"))?;
    for file in ["lib.rs", "a.rs", "b.rs", "config/mod.rs"] {
        fs::write(src.join(file), "")?;
    }
    let agents = "## Module Map\n\n| Module | Purpose |\n| --- | --- |\n| `a` | A |\n| `config/` | Config |\n\n```markdown\n| `b` | B |\n### Config (`config/`)\n| `mod.rs` | Mod |\n```\n";

    assert_eq!(
        missing_rows(agents, src)?,
        ["src/b.rs", "src/config/ has no table", "src/config/mod.rs"]
    );
    Ok(())
}
