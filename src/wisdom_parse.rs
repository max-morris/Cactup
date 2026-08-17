// The wisdom-corpus parser (spec §16), shared between the crate and
// `build.rs` (which `include!`s this file to parse and validate
// `resources/wisdom.txt` at build time). Keep it dependency-free, and
// keep these comments non-doc (`//`, not `//!`): inner doc comments are
// invalid at `include!` position.

/// Parse the corpus format: entries separated by `%` lines, `#` lines are
/// comments, a leading `!zen` line marks a quote entry. Returns
/// `(tips, zens)`.
pub(crate) fn parse_wisdom(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut tips = Vec::new();
    let mut zens = Vec::new();
    for block in raw.split('\n').map(str::trim_end).fold(vec![Vec::new()], |mut acc, line| {
        if line == "%" {
            acc.push(Vec::new());
        } else if !line.trim_start().starts_with('#') {
            acc.last_mut().expect("fold starts non-empty").push(line);
        }
        acc
    }) {
        let mut lines = block.as_slice();
        while lines.first().is_some_and(|l| l.is_empty()) {
            lines = &lines[1..];
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines = &lines[..lines.len() - 1];
        }
        let zen = lines.first().copied() == Some("!zen");
        if zen {
            lines = &lines[1..];
        }
        if lines.is_empty() {
            continue;
        }
        let text = lines.join("\n");
        if zen { &mut zens } else { &mut tips }.push(text);
    }
    (tips, zens)
}
