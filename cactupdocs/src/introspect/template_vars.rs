//! Template-variable catalog: scan the Cactup crate for `vars.set("NAME", …)`
//! calls and join them with descriptions from `data/template-vars.toml`.

use crate::model::TemplateVar;
use crate::Config;
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use walkdir::WalkDir;

#[derive(Deserialize)]
struct TemplateVarData {
    description: Option<String>,
    scope: Option<String>,
}

pub fn introspect(cfg: &Config) -> Result<Vec<TemplateVar>> {
    // Scan for .set("NAME") patterns
    let mut var_names = HashSet::new();
    scan_for_vars(&cfg.repo_root.join("src"), &mut var_names)?;

    // Load data file if present
    let mut var_data: HashMap<String, TemplateVarData> = HashMap::new();
    let data_file = cfg.data_dir.join("template-vars.toml");
    if data_file.exists() {
        let content = std::fs::read_to_string(&data_file)?;
        var_data = toml::from_str(&content).unwrap_or_default();
    }

    // Build TemplateVar entries
    let mut vars = Vec::new();
    for name in var_names {
        let data = var_data.get(&name);
        vars.push(TemplateVar {
            name,
            description: data.and_then(|d| d.description.clone()),
            scope: data.and_then(|d| d.scope.clone()),
        });
    }

    // Sort by name
    vars.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(vars)
}

/// Recursively scan .rs files for .set("NAME") patterns
fn scan_for_vars(dir: &Path, names: &mut HashSet<String>) -> Result<()> {
    for entry in WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let content = std::fs::read_to_string(entry.path())?;
        extract_var_names(&content, names);
    }
    Ok(())
}

/// Extract variable names from .set("NAME") patterns in source code
fn extract_var_names(source: &str, names: &mut HashSet<String>) {
    let pattern = ".set(\"";
    let mut pos = 0;

    while let Some(idx) = source[pos..].find(pattern) {
        let start = pos + idx + pattern.len();
        pos = start;

        // Read characters until we hit a closing quote
        let var_name: String = source.as_bytes()[start..]
            .iter()
            .map(|&b| b as char)
            .take_while(|&ch| ch != '"')
            .collect();

        // Check if var_name matches [A-Z][A-Z0-9_]*
        if is_valid_var_name(&var_name) {
            names.insert(var_name);
        }
    }
}

/// Check if a name is a valid template variable name: [A-Z][A-Z0-9_]*
fn is_valid_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return false;
    }

    for ch in chars {
        if !ch.is_ascii_uppercase() && !ch.is_ascii_digit() && ch != '_' {
            return false;
        }
    }

    true
}
