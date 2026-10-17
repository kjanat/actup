//! Checks that `crates/core/AGENTS.md` has a Module Map row for every module.

use std::{fs, path::Path};

fn rust_files(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "rs")
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn subdirectories(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

#[test]
fn agents_module_map_lists_every_module() -> Result<(), Box<dyn std::error::Error>> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agents = fs::read_to_string(crate_dir.join("AGENTS.md"))?;
    let has_row = |cell: &str| {
        agents
            .lines()
            .any(|line| line.starts_with(&format!("| `{cell}` ")))
    };
    let src = crate_dir.join("src");

    let mut missing = Vec::new();
    for file in rust_files(&src)? {
        let module = file.trim_end_matches(".rs");
        if module != "lib" && !has_row(module) {
            missing.push(format!("src/{file}"));
        }
    }
    for dir in subdirectories(&src)? {
        if !has_row(&format!("{dir}/")) {
            missing.push(format!("src/{dir}/"));
        }
        for file in rust_files(&src.join(&dir))? {
            if !has_row(&file) {
                missing.push(format!("src/{dir}/{file}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "crates/core/AGENTS.md has no Module Map row for {missing:?}"
    );
    Ok(())
}
