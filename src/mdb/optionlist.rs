//! Optionlist TOML format & the render step to Cactus's native
//! `NAME = value` format (spec §7.8, D9).
//!
//! Only parse + render live here. The build-flag injection (rule 5) and the
//! post-render `@NAME@` templating (rule 4) are applied by the CFG stream;
//! the rebuild decision diffs the verbatim TOML `source` kept on the struct.

use crate::Res;
use anyhow::{bail, Context};
use indexmap::IndexMap;
use serde::Deserialize;
use std::fmt::Write;
use std::path::Path;

/// The `[cactup]` header: cactup-only metadata, never emitted to the native
/// file (§7.8, D12, §4.4, §4.8).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OptionlistHeader {
    /// Binary capability (D12); cross-checked against the queue's gpu flag.
    #[serde(default)]
    pub gpu: bool,
    /// Queues this build may be submitted to (D12).
    #[serde(default)]
    pub compatible_queues: Vec<String>,
    /// Marks the implicit choice when a machine lists several optionlist
    /// variants (§4.4), mirroring the script variants' `default = true`.
    #[serde(default)]
    pub default: bool,
    /// Free-form, informational description of this variant. Purely
    /// documentary — never emitted to the native file.
    pub description: Option<String>,
    /// Build this variant inside this universe (§4.8 step 3).
    pub universe: Option<String>,
    /// Sims of this config default to running in `universe` too (§4.8 step 2);
    /// false opts out of the build-universe coercion.
    #[serde(default = "default_true")]
    pub coerce_run_universe: bool,
    /// Per-variant thorn toggles (§7.5), applied ON TOP of the machine-level
    /// `[build].enabled-thorns`/`disabled-thorns`. This is how one machine
    /// carries build flavors that differ in which thorns compile — e.g. a CUDA
    /// (nvcc) variant disabling thorns the CPU variant keeps. Merged with the
    /// machine lists at build time (the variant augments the machine).
    #[serde(default)]
    pub enabled_thorns: Vec<String>,
    #[serde(default)]
    pub disabled_thorns: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// One `[options]` value. Floats are rejected at load (§7.8 rule 3).
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    Str(String),
    Bool(bool),
    Int(i64),
}

impl OptionValue {
    /// The native-file spelling: strings verbatim (unquoted), booleans as
    /// Cactus's yes/no, integers as plain decimal.
    pub fn render(&self) -> String {
        match self {
            OptionValue::Str(s) => s.clone(),
            OptionValue::Bool(true) => "yes".to_owned(),
            OptionValue::Bool(false) => "no".to_owned(),
            OptionValue::Int(v) => v.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawOptionlist {
    #[serde(default)]
    cactup: OptionlistHeader,
    options: IndexMap<String, toml::Value>,
}

/// A parsed optionlist (`mdb/<m>/optionlists/<variant>.toml`).
#[derive(Debug, Clone)]
pub struct Optionlist {
    pub header: OptionlistHeader,
    /// The `[options]` table in TOML document order.
    pub options: IndexMap<String, OptionValue>,
    /// The verbatim TOML text; the §7.8 rebuild trigger diffs this against
    /// the copy snapshotted at build time.
    pub source: String,
}

/// Which of the three accepted spellings a user-supplied optionlist is in
/// (§7.8's mdb TOML, plus the two looser forms `build --optionlist` accepts
/// for a file that didn't come out of the mdb).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionlistFormat {
    /// The existing shape: a `[cactup]` header plus an `[options]` table.
    MdbToml,
    /// Only the option key/values, as TOML, with or without a literal
    /// `[options]` header line.
    OptionsToml,
    /// A native Cactus optionlist: `NAME = value` lines whose values are not
    /// string-quoted.
    Cfg,
}

/// Classify a user-supplied optionlist's text as one of the three accepted
/// spellings, without fully parsing it — `parse_any` dispatches on this.
///
/// Walks the text line by line. Each `KEY = VALUE` line's value is bucketed
/// as looking like TOML (quoted string, `true`/`false`), looking like native
/// Cactus (anything else, including empty), or neutral (a bare integer,
/// which means and renders the same in both families and so discriminates
/// nothing). A file must not mix the TOML and native buckets — that's the
/// one thing that would make the three forms ambiguous to tell apart.
pub fn sniff(source: &str) -> Res<OptionlistFormat> {
    let lines: Vec<&str> = source.lines().collect();

    let mut seen_cactup_header = false;
    let mut seen_options_header = false;
    let mut any_option_line = false;
    // First offending line of each kind, kept for the mixing error.
    let mut toml_line: Option<(usize, String)> = None;
    let mut cfg_line: Option<(usize, String)> = None;

    let mut i = 0;
    while i < lines.len() {
        let lineno = i + 1;
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim();
            match name {
                "cactup" => seen_cactup_header = true,
                "options" => seen_options_header = true,
                other => bail!(
                    "unknown table header [{other}] at line {lineno}; only [cactup] and \
                     [options] are recognized in an optionlist"
                ),
            }
            i += 1;
            continue;
        }

        let Some(eq_pos) = trimmed.find('=') else {
            bail!("line {lineno} is neither a table header nor a KEY = VALUE line: {trimmed:?}");
        };
        any_option_line = true;
        let value_part = trimmed[eq_pos + 1..].trim_start();

        // A TOML multi-line string can span past this line; find where it
        // closes (possibly on this same line) rather than misreading its
        // interior — a `#` or `=` inside the string body isn't a comment or
        // a new key.
        if let Some(delim) = triple_quote_delim(value_part) {
            toml_line.get_or_insert_with(|| (lineno, trimmed.to_owned()));
            if !value_part[3..].contains(delim) {
                i += 1;
                while i < lines.len() && !lines[i].contains(delim) {
                    i += 1;
                }
            }
            i += 1;
            continue;
        }

        // A TOML array can span multiple lines too (real mdb optionlists do
        // this for `disabled-thorns`/`enabled-thorns`, with `#`-commented
        // entries in between) — the same deal as a multi-line string: track
        // bracket depth across lines rather than choke on an interior line
        // that has no `=`. Cactus's native format has no array syntax, so a
        // bracketed value can only ever mean TOML, same as a quoted string.
        if value_part.starts_with('[') {
            toml_line.get_or_insert_with(|| (lineno, trimmed.to_owned()));
            let mut in_dquote = false;
            let mut in_squote = false;
            let mut depth = 0i32;
            scan_brackets(value_part, &mut in_dquote, &mut in_squote, &mut depth);
            while depth > 0 && i + 1 < lines.len() {
                i += 1;
                scan_brackets(lines[i], &mut in_dquote, &mut in_squote, &mut depth);
            }
            i += 1;
            continue;
        }

        let value = strip_trailing_comment(value_part).trim();

        if value.starts_with('"')
            || value.starts_with('\'')
            // TOML's boolean literals; Cactus's own spelling is yes/no, so
            // these can only mean TOML.
            || value == "true"
            || value == "false"
        {
            toml_line.get_or_insert_with(|| (lineno, trimmed.to_owned()));
        } else if looks_like_integer(value) {
            // Neutral: an integer means and renders the same either way.
        } else {
            cfg_line.get_or_insert_with(|| (lineno, trimmed.to_owned()));
        }

        i += 1;
    }

    if let (Some((tln, ttext)), Some((cln, ctext))) = (&toml_line, &cfg_line) {
        bail!(
            "optionlist mixes TOML and native-Cactus value spellings (line {tln}: \"{ttext}\" \
             looks like TOML, line {cln}: \"{ctext}\" looks like native Cactus); the three \
             accepted forms — mdb TOML, options-only TOML, and a native .cfg — may not be \
             intermixed in one file"
        );
    }

    if !any_option_line {
        bail!("optionlist declares no options");
    }

    if seen_cactup_header {
        if !seen_options_header {
            bail!("[cactup] header present but no [options] table; an mdb-style optionlist needs both");
        }
        return Ok(OptionlistFormat::MdbToml);
    }

    if cfg_line.is_some() {
        if seen_options_header {
            bail!(
                "a [options] TOML table header appears over unquoted, native-Cactus-looking \
                 values; the three accepted forms — mdb TOML, options-only TOML, and a native \
                 .cfg — may not be intermixed in one file"
            );
        }
        return Ok(OptionlistFormat::Cfg);
    }

    Ok(OptionlistFormat::OptionsToml)
}

fn triple_quote_delim(value: &str) -> Option<&'static str> {
    if value.starts_with("\"\"\"") {
        Some("\"\"\"")
    } else if value.starts_with("'''") {
        Some("'''")
    } else {
        None
    }
}

/// Updates `depth` (net `[`/`]` nesting) for one line of a possibly
/// multi-line TOML array, carrying quote state in from the previous line.
/// A `[`/`]` inside a quoted string (`"a]b"`) or after an unquoted `#`
/// (a commented-out `]`) doesn't count — only structural brackets do.
fn scan_brackets(line: &str, in_dquote: &mut bool, in_squote: &mut bool, depth: &mut i32) {
    for ch in line.chars() {
        if *in_dquote {
            if ch == '"' {
                *in_dquote = false;
            }
            continue;
        }
        if *in_squote {
            if ch == '\'' {
                *in_squote = false;
            }
            continue;
        }
        match ch {
            '"' => *in_dquote = true,
            '\'' => *in_squote = true,
            '#' => break, // rest of the line is a comment
            '[' => *depth += 1,
            ']' => *depth -= 1,
            _ => {}
        }
    }
}

/// Strips a trailing `# comment`: only a `#` that starts the value or
/// follows whitespace, and that's outside any quotes, counts — so
/// `CFLAGS = "a#b"` keeps its literal `#` and a dash-flag value isn't cut on
/// an unrelated `#` glued to other text.
fn strip_trailing_comment(value: &str) -> &str {
    let mut in_dquote = false;
    let mut in_squote = false;
    let mut prev_was_ws = true; // the start of the value counts as "after whitespace"
    for (i, ch) in value.char_indices() {
        match ch {
            '"' if !in_squote => in_dquote = !in_dquote,
            '\'' if !in_dquote => in_squote = !in_squote,
            '#' if !in_dquote && !in_squote && prev_was_ws => return &value[..i],
            _ => {}
        }
        prev_was_ws = ch.is_whitespace();
    }
    value
}

/// `^[+-]?[0-9][0-9_]*$` — deliberately looser than TOML's own integer
/// grammar (no `0x`/`0o`/`0b`, no double-underscore rule): in an optionlist
/// value this shape means the same thing to Cactus either way, so it's
/// classified as neutral rather than parsed precisely.
fn looks_like_integer(value: &str) -> bool {
    let digits = value.strip_prefix(|c: char| c == '+' || c == '-').unwrap_or(value);
    let mut chars = digits.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_digit() || c == '_')
}

/// Whether a literal `[options]` header line is present, distinguishing a
/// bare `[options]`-less options-only file from one that spells it out.
fn has_literal_options_header(source: &str) -> bool {
    source.lines().any(|line| line.trim() == "[options]")
}

/// Parse a native Cactus optionlist (`NAME = value`, unquoted). Never
/// produces `OptionValue::Bool`/`Int` — a `.cfg` is already native text, and
/// `render()` must reproduce it verbatim (`DEBUG = no` must stay `no`, never
/// become `false`/`no` via a bool round-trip).
fn parse_cfg(source: &str) -> Res<Optionlist> {
    let mut options: IndexMap<String, OptionValue> = IndexMap::new();

    for (idx, line) in source.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // sniff should already have rejected this shape; defensive only.
            bail!(
                "line {lineno} is a TOML-style table header, not valid in a native .cfg \
                 optionlist: {trimmed:?}"
            );
        }
        let Some(eq_pos) = trimmed.find('=') else {
            bail!("line {lineno} is not a KEY = VALUE line: {trimmed:?}");
        };
        let key = trimmed[..eq_pos].trim().to_owned();
        let value = strip_trailing_comment(&trimmed[eq_pos + 1..]).trim().to_owned();
        // Real Cactus .cfg files do repeat keys (TOML forbids it, which is
        // why this path exists at all): last value wins, but IndexMap's
        // `insert` updates a repeated key in place, so it keeps the FIRST
        // occurrence's position in document order.
        options.insert(key, OptionValue::Str(value));
    }

    // Same requirement as the TOML path, but this file has no [options] table
    // to name — say "optionlist" instead of pointing at a section that isn't
    // there. (§7.8)
    if !options.contains_key("VERSION") {
        bail!("optionlist must declare VERSION (emitted first; a change forces a full rebuild)");
    }

    Ok(Optionlist {
        header: OptionlistHeader::default(),
        options,
        source: source.to_owned(),
    })
}

impl Optionlist {
    pub fn load(path: &Path) -> Res<Optionlist> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read optionlist {}", path.display()))?;
        Self::parse(&source).with_context(|| format!("invalid optionlist {}", path.display()))
    }

    /// Load a user-supplied optionlist in any of the three accepted forms
    /// (`OptionlistFormat`), auto-detected via `sniff`.
    pub fn load_any(path: &Path) -> Res<Optionlist> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read optionlist {}", path.display()))?;
        Self::parse_any(&source).with_context(|| format!("invalid optionlist {}", path.display()))
    }

    /// `load_any`'s string-level half.
    pub fn parse_any(source: &str) -> Res<Optionlist> {
        match sniff(source)? {
            OptionlistFormat::MdbToml => Self::parse(source),
            OptionlistFormat::OptionsToml => {
                if has_literal_options_header(source) {
                    Self::parse(source)
                } else {
                    let wrapped = format!("[options]\n{source}");
                    let mut ol = Self::parse(&wrapped)?;
                    // `source` must stay byte-identical to the user's file —
                    // the §7.8 rebuild decision diffs against it verbatim.
                    ol.source = source.to_owned();
                    Ok(ol)
                }
            }
            OptionlistFormat::Cfg => parse_cfg(source),
        }
    }

    pub fn parse(source: &str) -> Res<Optionlist> {
        let raw: RawOptionlist = toml::from_str(source).context("Failed to parse optionlist TOML")?;

        let mut options = IndexMap::new();
        for (key, value) in raw.options {
            let value = match value {
                toml::Value::String(s) => OptionValue::Str(s),
                toml::Value::Boolean(b) => OptionValue::Bool(b),
                toml::Value::Integer(v) => OptionValue::Int(v),
                toml::Value::Float(_) => bail!(
                    "[options].{key} is a float; Cactus options are never floats — \
                     quote it as a string if the dotted value is intended" // §7.8
                ),
                other => bail!(
                    "[options].{key} must be a string, boolean, or integer (got {})",
                    other.type_str()
                ),
            };
            options.insert(key, value);
        }

        if !options.contains_key("VERSION") {
            bail!("[options] must declare VERSION (emitted first; a change forces a full rebuild)");
        }

        Ok(Optionlist {
            header: raw.cactup,
            options,
            source: source.to_owned(),
        })
    }

    /// Render to the native Cactus optionlist: `VERSION` first, then the
    /// remaining keys in document order (§7.8 rule 2). The result still
    /// carries `@NAME@` tokens; templating happens after render (rule 4).
    pub fn render(&self) -> String {
        let mut out = String::new();
        let version = &self.options["VERSION"];
        writeln!(out, "VERSION = {}", version.render()).unwrap();
        for (key, value) in &self.options {
            if key != "VERSION" {
                writeln!(out, "{key} = {}", value.render()).unwrap();
            }
        }
        out
    }
}

/// Parse only the `[cactup]` header — used to pick among a machine's
/// optionlist variants (§4.4) without demanding every listed file parses in
/// full.
pub fn load_header(path: &Path) -> Res<OptionlistHeader> {
    #[derive(Deserialize)]
    struct HeaderOnly {
        #[serde(default)]
        cactup: OptionlistHeader,
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read optionlist {}", path.display()))?;
    let raw: HeaderOnly = toml::from_str(&source)
        .with_context(|| format!("invalid optionlist {}", path.display()))?;
    Ok(raw.cactup)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = r#"
        [cactup]
        gpu = true
        compatible-queues = ["gpu"]
        universe = "et-sif"

        [options]
        VERSION = "2024-06-01"
        CC = "gcc"
        CFLAGS = "-O2 -g @SOME_TEMPLATED_VALUE@"
        DEBUG = false
        VECTORISE = true
        MAX_THINGS = 42
    "#;

    #[test]
    fn parses_header_and_renders_native_format() {
        let ol = Optionlist::parse(BASIC).unwrap();
        assert!(ol.header.gpu);
        assert_eq!(ol.header.compatible_queues, ["gpu"]);
        assert!(!ol.header.default);
        assert_eq!(ol.header.universe.as_deref(), Some("et-sif"));
        assert!(ol.header.coerce_run_universe); // defaults true
        assert!(ol.header.enabled_thorns.is_empty() && ol.header.disabled_thorns.is_empty());

        assert_eq!(
            ol.render(),
            "VERSION = 2024-06-01\n\
             CC = gcc\n\
             CFLAGS = -O2 -g @SOME_TEMPLATED_VALUE@\n\
             DEBUG = no\n\
             VECTORISE = yes\n\
             MAX_THINGS = 42\n"
        );
    }

    #[test]
    fn version_is_emitted_first_regardless_of_position() {
        let ol = Optionlist::parse(
            "[options]\nCC = \"gcc\"\nVERSION = \"2020-01-01\"\n",
        )
        .unwrap();
        assert!(ol.render().starts_with("VERSION = 2020-01-01\nCC = gcc\n"));
        // And [cactup] may be omitted entirely (all defaults).
        assert!(!ol.header.gpu && ol.header.compatible_queues.is_empty());
    }

    #[test]
    fn header_carries_per_variant_thorn_toggles() {
        // §7.8: an optionlist variant may disable/enable thorns on top of the
        // machine lists (e.g. a CUDA variant dropping thorns nvcc can't build).
        let ol = Optionlist::parse(
            "[cactup]\ngpu = true\n\
             disabled-thorns = [\"ExternalLibraries/LORENE\", \"EinsteinInitialData/Meudon_Bin_BH\"]\n\
             enabled-thorns = [\"ExternalLibraries/OpenBLAS\"]\n\
             [options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        assert_eq!(
            ol.header.disabled_thorns,
            ["ExternalLibraries/LORENE", "EinsteinInitialData/Meudon_Bin_BH"]
        );
        assert_eq!(ol.header.enabled_thorns, ["ExternalLibraries/OpenBLAS"]);
        // The toggles are not emitted to the native file.
        assert!(!ol.render().contains("LORENE"));
    }

    #[test]
    fn rejects_floats_and_missing_version() {
        let err = Optionlist::parse("[options]\nVERSION = \"x\"\nBAD = 1.5\n").unwrap_err();
        assert!(format!("{err:#}").contains("float"), "{err:#}");

        let err = Optionlist::parse("[options]\nCC = \"gcc\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("VERSION"), "{err:#}");
    }

    #[test]
    fn parses_the_real_mel5_optionlists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mdb/mel5/optionlists");
        let default = Optionlist::load(&root.join("default.toml")).unwrap();
        assert!(default.header.default, "mel5 default.toml is the implicit choice (§4.4)");
        assert!(!default.header.gpu);
        assert_eq!(default.header.compatible_queues, ["local"]);
        assert!(default.render().starts_with("VERSION = 2018-12-13\n"));

        let debug = load_header(&root.join("debug.toml")).unwrap();
        assert!(!debug.default, "mel5 debug.toml needs an explicit --variant");
    }

    const OPTIONS_ONLY_HEADER: &str = r#"
        [options]
        VERSION = "2024-06-01"
        CC = "gcc"
    "#;

    const OPTIONS_ONLY_NO_HEADER: &str = r#"
        VERSION = "2024-06-01"
        CC = "gcc"
    "#;

    const CFG_SNIPPET: &str = r#"
        # Generated by configure
        # do not edit by hand
        VERSION = 2024-06-01
        CC = gcc
        CXXFLAGS = -g -std=gnu++17
        DEBUG = no
        CPPFLAGS =
        CC = gcc-12  # override the compiler
    "#;

    #[test]
    fn sniff_detects_all_three_forms() {
        assert_eq!(sniff(BASIC).unwrap(), OptionlistFormat::MdbToml);
        assert_eq!(sniff(OPTIONS_ONLY_HEADER).unwrap(), OptionlistFormat::OptionsToml);
        assert_eq!(sniff(OPTIONS_ONLY_NO_HEADER).unwrap(), OptionlistFormat::OptionsToml);
        assert_eq!(sniff(CFG_SNIPPET).unwrap(), OptionlistFormat::Cfg);
    }

    #[test]
    fn options_only_matches_mdb_toml_and_gets_default_header() {
        let mdb_equivalent = Optionlist::parse(OPTIONS_ONLY_HEADER).unwrap();
        let with_header = Optionlist::parse_any(OPTIONS_ONLY_HEADER).unwrap();
        let no_header = Optionlist::parse_any(OPTIONS_ONLY_NO_HEADER).unwrap();

        assert_eq!(with_header.render(), mdb_equivalent.render());
        assert_eq!(no_header.render(), mdb_equivalent.render());
        assert_eq!(with_header.render(), "VERSION = 2024-06-01\nCC = gcc\n");

        // No [cactup] header in either input -> all defaults.
        assert!(!with_header.header.gpu && with_header.header.compatible_queues.is_empty());
        assert!(!no_header.header.gpu && no_header.header.compatible_queues.is_empty());

        // `source` is the verbatim input, not the header-prepended rewrite
        // used internally to parse the headerless form.
        assert_eq!(with_header.source, OPTIONS_ONLY_HEADER);
        assert_eq!(no_header.source, OPTIONS_ONLY_NO_HEADER);
    }

    #[test]
    fn parses_a_realistic_cfg_snippet() {
        let ol = Optionlist::parse_any(CFG_SNIPPET).unwrap();
        assert!(!ol.header.gpu && ol.header.compatible_queues.is_empty()); // default header
        assert_eq!(
            ol.render(),
            "VERSION = 2024-06-01\n\
             CC = gcc-12\n\
             CXXFLAGS = -g -std=gnu++17\n\
             DEBUG = no\n\
             CPPFLAGS = \n"
        );
        // §7.8: DEBUG must stay the literal `no`, never round-trip through a
        // bool and come back as `false`.
        assert!(ol.render().contains("DEBUG = no"));
        assert_eq!(ol.source, CFG_SNIPPET);
    }

    #[test]
    fn mdb_toml_source_is_byte_identical_to_input() {
        let ol = Optionlist::parse_any(BASIC).unwrap();
        assert_eq!(ol.source, BASIC);
    }

    #[test]
    fn mixing_toml_and_cfg_values_is_rejected() {
        let mixed = "CC = \"gcc\"\nCFLAGS = -O2\n";
        let err = Optionlist::parse_any(mixed).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");
        assert!(msg.contains("CFLAGS = -O2"), "{msg}");
        assert!(msg.contains("not be intermixed") || msg.contains("not intermixed"), "{msg}");
    }

    #[test]
    fn cactup_header_over_unquoted_values_errors() {
        // sniff itself accepts this as MdbToml (a literal [cactup] + literal
        // [options] header always means MdbToml, per the detection rules);
        // the bare, unquoted option values then fail the real TOML parser.
        let src = "[cactup]\n[options]\nVERSION = 2024-06-01\nCC = gcc\n";
        assert!(Optionlist::parse_any(src).is_err());
    }

    #[test]
    fn unknown_table_header_is_rejected_and_named() {
        let err = sniff("[machine]\nVERSION = \"1\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("machine"));
    }

    #[test]
    fn cfg_missing_version_errors() {
        let err = Optionlist::parse_any("CC = gcc\nDEBUG = no\n").unwrap_err();
        assert!(format!("{err:#}").contains("VERSION"));
    }

    #[test]
    fn empty_optionlist_is_rejected() {
        let err = sniff("   \n# just a comment\n").unwrap_err();
        assert!(format!("{err:#}").contains("no options"));
        let err = Optionlist::parse_any("   \n# just a comment\n").unwrap_err();
        assert!(format!("{err:#}").contains("no options"));
    }

    // Regression for a real shape (mdb/graham/optionlists/gpu.toml): a
    // [cactup] array value that spans several lines, with #-commented
    // entries interleaved. sniff must not choke on an interior line that has
    // neither a table header nor a `=`.
    const MULTILINE_ARRAY: &str = r#"
        [cactup]
        gpu = true
        disabled-thorns = [
            # CTThorns: code is CUDA-aware but lacks __host__ annotations
            "CTThorns/CT_Analytic",
            "CTThorns/CT_MultiLevel",
            # LORENE does not link
            "ExternalLibraries/LORENE",
        ]

        [options]
        VERSION = "graham-gpu-2021-03-11"
        CC = "gcc"
    "#;

    #[test]
    fn multiline_array_values_are_handled() {
        assert_eq!(sniff(MULTILINE_ARRAY).unwrap(), OptionlistFormat::MdbToml);

        let ol = Optionlist::parse_any(MULTILINE_ARRAY).unwrap();
        assert!(ol.header.gpu);
        assert_eq!(
            ol.header.disabled_thorns,
            ["CTThorns/CT_Analytic", "CTThorns/CT_MultiLevel", "ExternalLibraries/LORENE"]
        );
        assert_eq!(ol.render(), "VERSION = graham-gpu-2021-03-11\nCC = gcc\n");
        assert_eq!(ol.source, MULTILINE_ARRAY);
    }

    #[test]
    fn every_real_mdb_optionlist_is_a_legal_optionlist_flag_argument() {
        // Anything the mdb ships must be usable verbatim as `--optionlist`:
        // sweep mdb/*/optionlists/*.toml and require each to sniff as one of
        // the TOML forms (never Cfg — these are all TOML) and load cleanly
        // through the same entry point `--optionlist` uses.
        let mdb_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mdb");
        let mut checked = 0;
        for machine_entry in std::fs::read_dir(&mdb_root).unwrap() {
            let machine_dir = machine_entry.unwrap().path();
            if !machine_dir.is_dir() {
                continue;
            }
            let optionlists_dir = machine_dir.join("optionlists");
            let Ok(entries) = std::fs::read_dir(&optionlists_dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                let format = sniff(&source)
                    .unwrap_or_else(|e| panic!("{} failed to sniff: {e:#}", path.display()));
                assert!(
                    matches!(format, OptionlistFormat::MdbToml | OptionlistFormat::OptionsToml),
                    "{} sniffed as {format:?}, expected a TOML form",
                    path.display()
                );
                Optionlist::load_any(&path)
                    .unwrap_or_else(|e| panic!("{} failed to load_any: {e:#}", path.display()));
                checked += 1;
            }
        }
        assert!(checked > 0, "no optionlists found under {}", mdb_root.display());
    }
}
