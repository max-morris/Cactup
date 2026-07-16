//! Source-introspection of the sibling Cactup crate.
//!
//! We parse Rust source with `syn` rather than reflecting at runtime because
//! serde discards field doc-comments — and those `///` comments are exactly
//! the documentation we want to surface. This also means the generator needs
//! **no changes to the Cactup crate**: it only reads its source files,
//! relative to the repo root.

pub mod cli;
pub mod mdb;
pub mod template_vars;

use crate::model::DocModel;
use crate::Config;
use anyhow::Result;

/// Build the complete [`DocModel`] from the Cactup crate source.
pub fn all(cfg: &Config) -> Result<DocModel> {
    let cli = cli::introspect(&cfg.repo_root)?;
    let mut mdb = mdb::introspect(&cfg.repo_root)?;
    mdb.template_vars = template_vars::introspect(cfg)?;
    Ok(DocModel { cli, mdb })
}
