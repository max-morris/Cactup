//! Site rendering: introspect → load authored content → expand
//! `{{cactup:...}}` generated-include tokens → Markdown→HTML → wrap in the
//! minijinja layout → write the static site (plus a client-side search index).

use crate::introspect;
use crate::markdown;
use crate::model::{DocModel, CliCommand, MdbTable};
use crate::Config;
use anyhow::{Context, Result};
use minijinja::Environment;
use serde_json::{json, Value};
use std::fs;
use walkdir::WalkDir;

/// Configuration for a navigation section from _nav.toml.
#[derive(Debug, Clone, serde::Deserialize)]
struct NavSection {
    title: String,
    pages: Vec<NavPage>,
}

/// Configuration for a single page from _nav.toml.
#[derive(Debug, Clone, serde::Deserialize)]
struct NavPage {
    title: String,
    file: String,
}

/// The complete navigation configuration from _nav.toml.
#[derive(Debug, Clone, serde::Deserialize)]
struct Nav {
    site_title: String,
    tagline: String,
    repo_url: Option<String>,
    sections: Vec<NavSection>,
}

/// Metadata for a page being rendered.
#[derive(Debug, Clone)]
struct PageInfo {
    title: String,
    file: String,
}

/// Build the whole site into `cfg.out_dir`.
pub fn build_site(cfg: &Config) -> Result<()> {
    let model = introspect::all(cfg)?;
    fs::create_dir_all(&cfg.out_dir)
        .with_context(|| format!("creating output dir {}", cfg.out_dir.display()))?;

    // Normalize base_url to end with exactly one /
    let base_url = if cfg.base_url.ends_with('/') {
        cfg.base_url.clone()
    } else {
        format!("{}/", cfg.base_url)
    };

    // Load navigation config
    let nav_path = cfg.content_dir.join("_nav.toml");
    let nav_source = fs::read_to_string(&nav_path)
        .with_context(|| format!("reading nav config from {}", nav_path.display()))?;
    let nav: Nav = toml::from_str(&nav_source)
        .context("parsing _nav.toml")?;

    // Collect page information and build nav structure for context
    let mut all_pages = Vec::new();
    let mut nav_sections = Vec::new();

    for section in &nav.sections {
        let mut nav_pages = Vec::new();
        for page in &section.pages {
            let html_file = page.file.replace(".md", ".html");
            let url = format!("{}{}", base_url, html_file);
            let page_info = PageInfo {
                title: page.title.clone(),
                file: page.file.clone(),
            };
            all_pages.push(page_info);
            nav_pages.push(json!({
                "title": page.title,
                "url": url,
                "active": false, // Will be set per-page render
            }));
        }
        nav_sections.push(json!({
            "title": section.title,
            "pages": nav_pages,
        }));
    }

    // Generate HTML fragments from model
    let cli_html = generate_cli_html(&model)?;
    let mdb_meta_html = generate_mdb_meta_html(&model)?;
    let mdb_optionlist_html = generate_mdb_optionlist_html(&model)?;
    let template_vars_html = generate_template_vars_html(&model)?;

    // Load minijinja template
    let template_source = fs::read_to_string(cfg.templates_dir.join("base.html"))
        .context("reading base.html template")?;
    let mut env = Environment::new();
    env.add_template("base", &template_source)
        .context("adding base.html to minijinja")?;

    // Render each page
    let mut search_index = Vec::new();

    for (page_idx, page_info) in all_pages.iter().enumerate() {
        let content_path = cfg.content_dir.join(&page_info.file);
        let markdown_source = fs::read_to_string(&content_path)
            .with_context(|| format!("reading {}", content_path.display()))?;

        // Parse front matter
        let parsed_page = markdown::parse_front_matter(&markdown_source);
        let title = parsed_page
            .front_matter
            .get("title")
            .cloned()
            .unwrap_or_else(|| page_info.title.clone());
        let description = parsed_page
            .front_matter
            .get("description")
            .cloned();

        // Convert markdown to HTML
        let mut html = markdown::to_html(&parsed_page.body)?;

        // Expand generated tokens
        html = expand_tokens(
            &html,
            &cli_html,
            &mdb_meta_html,
            &mdb_optionlist_html,
            &template_vars_html,
            &model,
        )?;

        // Mark content as safe HTML for minijinja
        // We'll pass it as a string and use |safe in the template

        // Build nav with active page marked
        let mut nav_for_page = Vec::new();
        for (section_idx, section) in nav_sections.iter().enumerate() {
            let mut pages_with_active = section["pages"].clone();
            if let Some(arr) = pages_with_active.as_array_mut() {
                for (p_idx, p) in arr.iter_mut().enumerate() {
                    if section_idx == page_idx / nav.sections.iter().map(|s| s.pages.len()).sum::<usize>() * nav.sections.len()
                        && p_idx == page_idx % nav.sections[section_idx].pages.len()
                        && let Some(obj) = p.as_object_mut()
                    {
                        obj.insert("active".to_string(), Value::Bool(true));
                    }
                }
            }
            nav_for_page.push(json!({
                "title": section["title"],
                "pages": pages_with_active,
            }));
        }

        // Track page for active link marking
        let current_html_path = page_info.file.replace(".md", ".html");
        for section in &nav_for_page {
            if let Some(pages) = section["pages"].as_array() {
                for page in pages {
                    if let Some(url) = page.get("url").and_then(|u| u.as_str())
                        && url.ends_with(&current_html_path)
                    {
                        // Mark this page as active
                        if let Some(obj) = page.as_object() {
                            let mut updated = obj.clone();
                            updated.insert("active".to_string(), Value::Bool(true));
                        }
                    }
                }
            }
        }

        // Simpler approach: mark active in the context loop
        let mut nav_context = Vec::new();
        for section in nav.sections.iter() {
            let mut section_context = json!({
                "title": section.title,
                "pages": []
            });
            if let Some(pages_arr) = section_context["pages"].as_array_mut() {
                for nav_page in section.pages.iter() {
                    let is_current = nav_page.file == page_info.file;
                    pages_arr.push(json!({
                        "title": nav_page.title,
                        "url": format!("{}{}", base_url, nav_page.file.replace(".md", ".html")),
                        "active": is_current,
                    }));
                }
            }
            nav_context.push(section_context);
        }

        // Render template
        let tmpl = env.get_template("base")?;
        let ctx = json!({
            "site_title": nav.site_title,
            "tagline": nav.tagline,
            "repo_url": nav.repo_url.clone(),
            "base_url": base_url.clone(),
            "year": "2026",
            "nav": nav_context,
            "page": {
                "title": title.clone(),
                "description": description.clone(),
                "content_html": html.clone(),
            },
            "search_index": format!("{}search-index.json", base_url),
        });
        let rendered = tmpl.render(ctx)?;

        // Write output file
        let output_file = page_info.file.replace(".md", ".html");
        let output_path = cfg.out_dir.join(&output_file);
        fs::create_dir_all(
            output_path
                .parent()
                .context("getting parent dir of output file")?,
        )
        .context("creating output directory")?;
        fs::write(&output_path, rendered)
            .with_context(|| format!("writing {}", output_path.display()))?;

        // Add to search index (drop the raw generated-include tokens so they
        // aren't indexed as literal text).
        let plaintext = strip_generated_tokens(&markdown::to_plaintext(&parsed_page.body));
        // Cap plaintext at ~2000 chars (respect char boundaries).
        let text_for_index = if plaintext.chars().count() > 2000 {
            plaintext.chars().take(2000).collect()
        } else {
            plaintext
        };

        search_index.push(json!({
            "title": title,
            "url": format!("{}{}", base_url, output_file),
            "text": text_for_index,
        }));
    }

    // Copy assets
    copy_assets(&cfg.assets_dir, &cfg.out_dir.join("assets"))
        .context("copying assets")?;

    // Write search index
    let search_index_path = cfg.out_dir.join("search-index.json");
    let search_index_json = serde_json::to_string_pretty(&search_index)?;
    fs::write(&search_index_path, search_index_json)
        .context("writing search-index.json")?;

    Ok(())
}

/// Recursively copy a directory tree.
fn copy_assets(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if !src.exists() {
        // Assets directory is optional
        return Ok(());
    }

    fs::create_dir_all(dst).context("creating assets directory")?;

    for entry in WalkDir::new(src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let relative = path
            .strip_prefix(src)
            .context("computing relative path")?;
        let dest_path = dst.join(relative);

        if path.is_dir() {
            fs::create_dir_all(&dest_path)
                .with_context(|| format!("creating dir {}", dest_path.display()))?;
        } else {
            fs::copy(path, &dest_path)
                .with_context(|| format!("copying {} to {}", path.display(), dest_path.display()))?;
        }
    }

    Ok(())
}

/// Generate HTML for {{cactup:cli}}.
fn generate_cli_html(model: &DocModel) -> Result<String> {
    let mut html = String::new();
    let root = &model.cli.root;
    // The global flags are flattened onto the root command, which
    // `generate_cli_command_html` otherwise skips — surface them explicitly.
    if !root.args.is_empty() {
        html.push_str(
            r#"<section class="cli-command" id="cmd-global-options"><h3><code>Global options</code></h3><p class="cli-about">Available on every cactup subcommand.</p>"#,
        );
        push_cli_args_table(&mut html, &root.args);
        html.push_str("</section>");
    }
    generate_cli_command_html(root, &mut html)?;
    Ok(html)
}

/// Render the two-column (Argument, Description) args table for a command.
fn push_cli_args_table(html: &mut String, args: &[crate::model::CliArg]) {
    html.push_str(
        r#"<table class="ref-table cli-args"><thead><tr><th>Argument</th><th>Description</th></tr></thead><tbody>"#,
    );
    for arg in args {
        let arg_cell = format_cli_arg(arg);
        // Escape the help text, then append the (already-safe) hint markup so
        // the <span> is not itself escaped into visible text.
        let mut desc = html_escape(&arg.help.clone().unwrap_or_default());
        let mut hints = Vec::new();
        if let Some(default) = &arg.default {
            hints.push(format!("(default: {})", default));
        }
        if arg.global {
            hints.push("(global)".to_string());
        }
        if arg.multiple {
            hints.push("(repeatable)".to_string());
        }
        if !hints.is_empty() {
            desc.push_str(&format!(
                r#" <span class="hint">{}</span>"#,
                html_escape(&hints.join(" "))
            ));
        }
        html.push_str(&format!(
            r#"<tr><td><code>{}</code></td><td>{}</td></tr>"#,
            html_escape(&arg_cell),
            desc
        ));
    }
    html.push_str("</tbody></table>");
}

/// Recursively generate CLI command sections.
fn generate_cli_command_html(cmd: &CliCommand, html: &mut String) -> Result<()> {
    // Generate section for this command (unless it's the bare root)
    if !cmd.path.is_empty() && cmd.path != vec!["cactup"] {
        let id = format!("cmd-{}", cmd.path.join("-"));
        let code_path = cmd.path.join(" ");

        html.push_str(&format!(
            r#"<section class="cli-command" id="{}">"#,
            html_escape(&id)
        ));
        html.push_str(&format!(
            r#"<h3><code>{}</code></h3>"#,
            html_escape(&code_path)
        ));

        if let Some(about) = &cmd.about {
            html.push_str(&format!(
                r#"<p class="cli-about">{}</p>"#,
                html_escape(about)
            ));
        }

        if let Some(long_about) = &cmd.long_about {
            html.push_str(&format!(
                r#"<div class="cli-longabout">{}</div>"#,
                html_escape(long_about)
            ));
        }

        // Arguments table
        if !cmd.args.is_empty() {
            push_cli_args_table(html, &cmd.args);
        }

        html.push_str("</section>");
    }

    // Recursively process subcommands
    for subcmd in &cmd.subcommands {
        generate_cli_command_html(subcmd, html)?;
    }

    Ok(())
}

/// Format a CliArg for the Argument cell.
fn format_cli_arg(arg: &crate::model::CliArg) -> String {
    let mut result = String::new();

    if let Some(short) = arg.short {
        result.push('-');
        result.push(short);
    }

    if let Some(long) = &arg.long {
        if !result.is_empty() {
            result.push_str(", ");
        }
        result.push_str("--");
        result.push_str(long);
    }

    if arg.positional {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push('<');
        result.push_str(&arg.name.to_uppercase());
        result.push('>');
    } else if arg.takes_value {
        result.push(' ');
        result.push('<');
        if let Some(vname) = &arg.value_name {
            result.push_str(vname);
        } else {
            result.push_str(&arg.name.to_uppercase());
        }
        result.push('>');
    }

    result
}

/// Generate HTML for {{cactup:mdb-meta}}.
fn generate_mdb_meta_html(model: &DocModel) -> Result<String> {
    let mut html = String::new();

    for table in &model.mdb.meta_tables {
        generate_mdb_table_html(table, &mut html)?;
    }

    Ok(html)
}

/// Generate HTML for {{cactup:mdb-optionlist}}.
fn generate_mdb_optionlist_html(model: &DocModel) -> Result<String> {
    let mut html = String::new();
    generate_mdb_table_html(&model.mdb.optionlist_header, &mut html)?;
    html.push_str(
        r#"<p><code>[options]</code> holds raw Cactus <code>NAME = value</code> pairs (VERSION required, emitted first; strings/bools/ints only, no floats).</p>"#
    );
    Ok(html)
}

/// Generate HTML for a single MDB table.
fn generate_mdb_table_html(table: &MdbTable, html: &mut String) -> Result<()> {
    let id = format!("mdb-{}", table.struct_name.to_lowercase());
    let heading = table
        .toml_path
        .clone()
        .unwrap_or_else(|| format!("[{}]", table.struct_name));

    html.push_str(&format!(
        r#"<section class="mdb-table" id="{}">"#,
        html_escape(&id)
    ));
    html.push_str(&format!(
        r#"<h3><code>{}</code></h3>"#,
        html_escape(&heading)
    ));

    if let Some(doc) = &table.doc {
        html.push_str(&format!(
            r#"<p class="mdb-about">{}</p>"#,
            html_escape(doc)
        ));
    }

    html.push_str(
        r#"<table class="ref-table"><thead><tr><th>Key</th><th>Type</th><th>Required</th><th>Description</th></tr></thead><tbody>"#
    );

    for field in &table.fields {
        let required = if field.optional { "optional" } else { "required" };
        html.push_str(&format!(
            r#"<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
            html_escape(&field.toml_key),
            html_escape(&field.type_desc),
            required,
            html_escape(&field.doc.clone().unwrap_or_default())
        ));
    }

    html.push_str("</tbody></table>");
    html.push_str("</section>");

    Ok(())
}

/// Generate HTML for {{cactup:template-vars}}.
fn generate_template_vars_html(model: &DocModel) -> Result<String> {
    let mut html = String::from(
        r#"<table class="ref-table template-vars"><thead><tr><th>Name</th><th>Description</th></tr></thead><tbody>"#
    );

    for var in &model.mdb.template_vars {
        html.push_str(&format!(
            r#"<tr><td><code>@{}@</code></td><td>{}</td></tr>"#,
            html_escape(&var.name),
            html_escape(&var.description.clone().unwrap_or_default())
        ));
    }

    html.push_str("</tbody></table>");
    Ok(html)
}

/// Expand {{cactup:...}} tokens in HTML.
fn expand_tokens(
    html: &str,
    cli_html: &str,
    mdb_meta_html: &str,
    mdb_optionlist_html: &str,
    template_vars_html: &str,
    model: &DocModel,
) -> Result<String> {
    let mut result = html.to_string();

    // Replace {{cactup:cli}} and {{cactup:cli command="..."}}
    // Tolerate optional <p>...</p> wrapping
    result = expand_token_with_attr(
        &result,
        "cactup:cli",
        |attr: Option<&str>| -> Result<String> {
            if let Some(attr_val) = attr
                && attr_val.starts_with("command=\"") && attr_val.ends_with("\"")
            {
                let cmd_path = &attr_val[9..attr_val.len() - 1];
                let parts: Vec<&str> = cmd_path.split(' ').collect();
                if let Some(html) = find_and_generate_command_html(&model.cli.root, &parts) {
                    return Ok(html);
                }
                return Ok(String::new());
            }
            Ok(cli_html.to_string())
        },
    )?;

    // Replace {{cactup:mdb-meta}}
    result = expand_token(
        &result,
        "cactup:mdb-meta",
        mdb_meta_html.to_string(),
    );

    // Replace {{cactup:mdb-optionlist}}
    result = expand_token(
        &result,
        "cactup:mdb-optionlist",
        mdb_optionlist_html.to_string(),
    );

    // Replace {{cactup:template-vars}}
    result = expand_token(
        &result,
        "cactup:template-vars",
        template_vars_html.to_string(),
    );

    Ok(result)
}

/// Replace a simple token (without attributes) in HTML, tolerating <p> wrapper.
fn expand_token(html: &str, token: &str, replacement: String) -> String {
    let bare_token = format!("{{{{{}}}}}", token);
    let wrapped_token = format!("<p>{}</p>", bare_token);

    if html.contains(&wrapped_token) {
        html.replace(&wrapped_token, &replacement)
    } else {
        html.replace(&bare_token, &replacement)
    }
}

/// Replace a token with optional attribute in HTML, tolerating <p> wrapper.
fn expand_token_with_attr<F>(
    html: &str,
    token_prefix: &str,
    generator: F,
) -> Result<String>
where
    F: Fn(Option<&str>) -> Result<String>,
{
    let mut result = html.to_string();
    let token_pattern = format!("{{{{{}", token_prefix);

    // Find all occurrences
    while let Some(start) = result.find(&token_pattern) {
        // Find the closing }}
        if let Some(end) = result[start..].find("}}") {
            let end_pos = start + end + 2;
            let token_content = &result[start + 2..end_pos - 2];

            // Parse the token
            let attr = if token_content == token_prefix {
                None
            } else if let Some(rest) = token_content.strip_prefix(token_prefix) {
                // Convention is `{{cactup:cli command="…"}}` — the attribute
                // follows the prefix after whitespace, not a colon.
                let rest = rest.trim();
                if rest.is_empty() { None } else { Some(rest) }
            } else {
                // Not this token; stop scanning for it.
                break;
            };

            let replacement = generator(attr)?;

            // Check for <p>...</p> wrapper
            if result[..start].ends_with("<p>") && result[end_pos..].starts_with("</p>") {
                // Replace with surrounding <p>
                let p_start = start - 3;
                let p_end = end_pos + 4;
                result.replace_range(p_start..p_end, &replacement);
            } else {
                result.replace_range(start..end_pos, &replacement);
            }
        } else {
            break;
        }
    }

    Ok(result)
}

/// Find a command by its path relative to the root (e.g. `["sim", "submit"]`,
/// as written in `command="sim submit"`) and generate just that command's
/// section (plus any of its own subcommands). A leading `cactup` is tolerated.
fn find_and_generate_command_html(root: &CliCommand, path: &[&str]) -> Option<String> {
    let path = if path.first() == Some(&"cactup") { &path[1..] } else { path };
    let mut cur = root;
    for part in path {
        cur = cur.subcommands.iter().find(|c| &c.name == part)?;
    }
    let mut html = String::new();
    generate_cli_command_html(cur, &mut html).ok()?;
    if html.is_empty() { None } else { Some(html) }
}

/// Remove `{{cactup:…}}` generated-include tokens from a plaintext string.
fn strip_generated_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("{{cactup") {
        out.push_str(&rest[..i]);
        match rest[i..].find("}}") {
            Some(j) => rest = &rest[i + j + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escape HTML special characters.
fn html_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&#39;".chars().collect::<Vec<_>>(),
            _ => vec![c],
        })
        .collect()
}
