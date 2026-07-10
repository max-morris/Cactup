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
/// file (§7.8, D12, §11.2, §4.8).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OptionlistHeader {
    /// Binary capability (D12); cross-checked against the queue's gpu flag.
    #[serde(default)]
    pub gpu: bool,
    /// Queues this build may be submitted to (D12).
    #[serde(default)]
    pub compatible_queues: Vec<String>,
    /// Marks a testsuite optionlist, considered only by `test build` (§11.2).
    #[serde(default)]
    pub test: bool,
    /// Build this variant inside this universe (§4.8 step 3).
    pub universe: Option<String>,
    /// Sims of this config default to running in `universe` too (§4.8 step 2);
    /// false opts out of the build-universe coercion.
    #[serde(default = "default_true")]
    pub coerce_run_universe: bool,
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

impl Optionlist {
    pub fn load(path: &Path) -> Res<Optionlist> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read optionlist {}", path.display()))?;
        Self::parse(&source).with_context(|| format!("invalid optionlist {}", path.display()))
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
                     quote it as a string if the dotted value is intended (§7.8)"
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

/// Parse only the `[cactup]` header — used to partition a machine's optionlist
/// variants (§11.2) without demanding every listed file parses in full.
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
        assert!(!ol.header.test);
        assert_eq!(ol.header.universe.as_deref(), Some("et-sif"));
        assert!(ol.header.coerce_run_universe); // defaults true

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
        assert!(!default.header.test && !default.header.gpu);
        assert_eq!(default.header.compatible_queues, ["local"]);
        assert!(default.render().starts_with("VERSION = 2018-12-13\n"));

        let test = load_header(&root.join("test.toml")).unwrap();
        assert!(test.test, "mel5 test.toml must be test-marked (§11.2)");
    }
}
