//! Markdown helpers: front-matter parsing and Markdown → HTML rendering.

use anyhow::Result;
use pulldown_cmark::{html, Parser};
use std::collections::BTreeMap;

/// A parsed content page: front-matter key/values plus the Markdown body.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub front_matter: BTreeMap<String, String>,
    pub body: String,
}

/// Split optional `+++`-delimited TOML front matter from the body.
///
/// If the source starts with a line containing exactly `+++`, reads until the next `+++`
/// line, parses that block as TOML, and flattens it to string values. The rest becomes
/// the body.
pub fn parse_front_matter(source: &str) -> Page {
    let trimmed = source.trim_start();

    if !trimmed.starts_with("+++") {
        // No front matter
        return Page {
            front_matter: BTreeMap::new(),
            body: source.to_string(),
        };
    }

    // Find the first line break after the opening +++
    let after_first_delimiter = match trimmed.find('\n') {
        Some(idx) => &trimmed[idx + 1..],
        None => {
            // File is just "+++" with nothing after
            return Page {
                front_matter: BTreeMap::new(),
                body: source.to_string(),
            };
        }
    };

    // Find the closing +++
    match after_first_delimiter.find("+++") {
        Some(closing_idx) => {
            let toml_block = &after_first_delimiter[..closing_idx].trim_end();
            let body_start = closing_idx + 3; // skip the closing +++
            let body = after_first_delimiter[body_start..].trim_start().to_string();

            // Parse TOML and flatten to strings
            let mut front_matter = BTreeMap::new();
            if let Ok(table) = toml::from_str::<toml::Table>(toml_block) {
                for (key, value) in table {
                    front_matter.insert(
                        key,
                        match value {
                            toml::Value::String(s) => s,
                            toml::Value::Integer(i) => i.to_string(),
                            toml::Value::Float(f) => f.to_string(),
                            toml::Value::Boolean(b) => b.to_string(),
                            _ => value.to_string(),
                        },
                    );
                }
            }

            Page { front_matter, body }
        }
        None => {
            // No closing +++, treat the whole thing as body
            Page {
                front_matter: BTreeMap::new(),
                body: source.to_string(),
            }
        }
    }
}

/// Render CommonMark (with tables/footnotes/strikethrough/tasklists) to an HTML fragment.
pub fn to_html(markdown: &str) -> Result<String> {
    let parser = Parser::new(markdown);
    let mut html = String::new();
    html::push_html(&mut html, parser);

    Ok(html)
}

/// Extract rough plain text from Markdown (concatenate Text/Code events).
///
/// Used for the search index to provide context without HTML markup.
pub fn to_plaintext(markdown: &str) -> String {
    let parser = Parser::new(markdown);
    let mut text = String::new();

    for event in parser {
        match event {
            pulldown_cmark::Event::Text(s) => {
                text.push_str(&s);
                text.push(' ');
            }
            pulldown_cmark::Event::Code(s) => {
                text.push_str(&s);
                text.push(' ');
            }
            pulldown_cmark::Event::SoftBreak | pulldown_cmark::Event::HardBreak => {
                text.push(' ');
            }
            _ => {}
        }
    }

    text.trim().to_string()
}
