//! The shared documentation model.
//!
//! Both introspectors (`introspect::cli`, `introspect::mdb`) produce values of
//! these types by source-parsing the sibling Cactup crate; the renderer
//! (`render`) consumes them to emit the generated reference sections that
//! hand-authored Markdown embeds via `{{cactup:...}}` tokens.
//!
//! Everything is `Serialize` so the renderer can hand a model straight to a
//! minijinja template and so a `--dump` mode can emit JSON for debugging.

use serde::Serialize;

/// The whole introspected surface, assembled by [`crate::introspect::all`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct DocModel {
    pub cli: CliModel,
    pub mdb: MdbModel,
}

// ---------------------------------------------------------------------------
// CLI (from src/args.rs — clap derive)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct CliModel {
    /// The root command (`cactup`) with the full subcommand tree.
    pub root: CliCommand,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CliCommand {
    /// The invocation token for this level, e.g. `cactup`, `sim`, `submit`.
    pub name: String,
    /// Full path from the root, e.g. `["cactup", "sim", "submit"]`.
    pub path: Vec<String>,
    /// First paragraph of the command's doc-comment / `about`.
    pub about: Option<String>,
    /// Remaining paragraphs (clap's long help), if any.
    pub long_about: Option<String>,
    /// Positional + optional arguments declared directly on this command
    /// (with any `#[clap(flatten)]` structs expanded inline).
    pub args: Vec<CliArg>,
    /// Nested subcommands.
    pub subcommands: Vec<CliCommand>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CliArg {
    /// Rust field name (also the value name for positionals when none is set).
    pub name: String,
    /// Short flag, e.g. `v` for `-v`, if any.
    pub short: Option<char>,
    /// Long flag (kebab-cased), e.g. `make-jobs` for `--make-jobs`, if any.
    pub long: Option<String>,
    /// Explicit `value_name = "..."`, if given.
    pub value_name: Option<String>,
    /// A bare positional argument (no `-`/`--`).
    pub positional: bool,
    /// Whether the argument must be supplied (non-`Option`, non-bool, no default).
    pub required: bool,
    /// Whether it takes a value at all (`false` for boolean flags).
    pub takes_value: bool,
    /// Whether it accepts many values (`Vec<T>`).
    pub multiple: bool,
    /// `default_value = "..."`, if given.
    pub default: Option<String>,
    /// `global = true`.
    pub global: bool,
    /// Short help (first paragraph of doc / `help = "..."`).
    pub help: Option<String>,
    /// Long help (remaining paragraphs), if any.
    pub long_help: Option<String>,
}

// ---------------------------------------------------------------------------
// MDB (from src/mdb/meta.rs + src/mdb/optionlist.rs — serde structs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct MdbModel {
    /// The `meta.toml` tables (one per modeled serde struct).
    pub meta_tables: Vec<MdbTable>,
    /// The optionlist `[cactup]` header table.
    pub optionlist_header: MdbTable,
    /// The `@VAR@` template tokens available to submit/run/optionlist templates.
    pub template_vars: Vec<TemplateVar>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MdbTable {
    /// Rust struct name, e.g. `Meta`, `Queue`, `OptionlistHeader`.
    pub struct_name: String,
    /// The TOML table header this struct maps to, e.g. `[machine]`,
    /// `[queues.<name>]`, if the introspector could determine one.
    pub toml_path: Option<String>,
    /// Struct-level doc-comment.
    pub doc: Option<String>,
    pub fields: Vec<MdbField>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MdbField {
    /// TOML key (after applying `rename_all = "kebab-case"` / `rename`).
    pub toml_key: String,
    /// Original Rust field name.
    pub rust_field: String,
    /// Human-readable type, e.g. `string`, `integer`, `boolean`,
    /// `list of string`, `table (Queue)`.
    pub type_desc: String,
    /// Whether the key may be omitted (`Option<T>` or `#[serde(default)]`).
    pub optional: bool,
    /// A note about the default/omission behavior, if derivable.
    pub default_note: Option<String>,
    /// Field doc-comment (the exposition authors care most about).
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TemplateVar {
    /// Token spelling without the `@`, e.g. `NODES` for `@NODES@`.
    pub name: String,
    /// Human description (from `data/template-vars.toml`, if present).
    pub description: Option<String>,
    /// Rough phase/where-set hint, if provided in the data file.
    pub scope: Option<String>,
}
