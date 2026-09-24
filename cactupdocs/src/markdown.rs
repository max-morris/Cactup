//! Markdown helpers: front-matter parsing and Markdown → HTML rendering.

use anyhow::Result;
use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};
use std::collections::{BTreeMap, HashMap};

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

/// The CommonMark extensions every page is parsed with. `to_html` and
/// `to_plaintext` must agree, or a table would render as a table but be
/// indexed for search as one pipe-riddled paragraph.
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
}

/// Render CommonMark (with tables/footnotes/strikethrough/tasklists) to an HTML fragment.
///
/// Every heading gets a GitHub-style `id` (see `slugify`), so pages can link
/// to a section as `page.html#section-title`.
pub fn to_html(markdown: &str) -> Result<String> {
    let mut events: Vec<Event> = Parser::new_ext(markdown, options()).collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for i in 0..events.len() {
        let Event::Start(Tag::Heading { id: None, .. }) = &events[i] else { continue };
        let mut title = String::new();
        for ev in &events[i + 1..] {
            match ev {
                Event::End(TagEnd::Heading(_)) => break,
                Event::Text(s) | Event::Code(s) => title.push_str(s),
                _ => {}
            }
        }
        let base = slugify(&title);
        if base.is_empty() {
            continue;
        }
        // Repeated titles get -1, -2, … suffixes, as on GitHub.
        let n = seen.entry(base.clone()).or_insert(0);
        let slug = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        if let Event::Start(Tag::Heading { id, .. }) = &mut events[i] {
            *id = Some(CowStr::from(slug));
        }
    }
    let mut html = String::new();
    html::push_html(&mut html, events.into_iter());

    Ok(html)
}

/// GitHub's heading anchor: lowercase, spaces become `-`, and everything but
/// letters, digits, `-` and `_` is dropped.
fn slugify(title: &str) -> String {
    title
        .trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// Extract rough plain text from Markdown (concatenate Text/Code events).
///
/// Used for the search index to provide context without HTML markup.
pub fn to_plaintext(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, options());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_tables_render_as_tables() {
        let md = "| Key | Meaning |\n|-----|---------|\n| `a` | first |\n";
        let html = to_html(md).unwrap();
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<th>Key</th>"), "{html}");
        assert!(html.contains("<td><code>a</code></td>"), "{html}");
        assert!(!to_plaintext(md).contains('|'));
    }

    #[test]
    fn raw_html_blocks_pass_through_around_markdown() {
        // The home page's install box: a raw <div> around a fenced block.
        let md = "<div class=\"install-box\">\n\n```sh\necho hi\n```\n\n</div>\n";
        let html = to_html(md).unwrap();
        assert!(html.contains("<div class=\"install-box\">"), "{html}");
        assert!(html.contains("<pre><code class=\"language-sh\">echo hi"), "{html}");
        assert!(html.contains("</div>"), "{html}");
    }

    #[test]
    fn headings_get_github_style_ids() {
        let html = to_html("## Knobs\n\n### The `autoupdate` knob\n\n## Knobs\n").unwrap();
        assert!(html.contains("<h2 id=\"knobs\">Knobs</h2>"), "{html}");
        assert!(html.contains("<h3 id=\"the-autoupdate-knob\">"), "{html}");
        assert!(html.contains("<h2 id=\"knobs-1\">"), "{html}");
        assert_eq!(slugify("Offline hosts & firewalls: `mdb-url`"), "offline-hosts--firewalls-mdb-url");
    }

    #[test]
    fn strikethrough_and_tasklists_are_enabled() {
        let html = to_html("~~old~~\n\n- [x] done\n").unwrap();
        assert!(html.contains("<del>old</del>"), "{html}");
        assert!(html.contains("type=\"checkbox\""), "{html}");
    }
}
