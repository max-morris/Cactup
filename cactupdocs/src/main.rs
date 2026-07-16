//! CactupDocs — the cactup documentation-site generator.
//!
//! Introspects the sibling Cactup crate (clap CLI in `../src/args.rs`, serde
//! MDB structs in `../src/mdb/*.rs`), merges the result with hand-authored
//! Markdown under `content/`, and renders a static site.
//!
//! Designed to run via `cargo run -p CactupDocs` inside a clone of the whole
//! repository (it reads the sibling crate's source relative to
//! `CARGO_MANIFEST_DIR`), so it slots directly into a GitHub Pages CI job.

mod introspect;
mod markdown;
mod model;
mod render;
mod serve;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cactupdocs", version, about = "Generate the cactup documentation site")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the static documentation site into an output directory
    Build {
        /// Output directory for the generated site.
        #[arg(long, default_value = "site")]
        out: PathBuf,
        /// Base URL path the site is served under (e.g. `/Cactup/` on
        /// project GitHub Pages, `/` on a user/root site).
        #[arg(long, default_value = "/")]
        base_url: String,
    },
    /// Build the site and serve it locally for preview
    Serve {
        /// Address to bind the preview server to.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
        /// Output directory for the generated site.
        #[arg(long, default_value = "site")]
        out: PathBuf,
    },
    /// Introspect the Cactup crate and print the raw doc model as JSON
    Dump,
}

/// Resolved paths and settings for one generator run.
pub struct Config {
    /// This crate's directory (`<repo>/cactupdocs`).
    pub manifest_dir: PathBuf,
    /// The repository root (`<repo>`) — where the Cactup crate's `src/` lives.
    pub repo_root: PathBuf,
    /// Hand-authored Markdown + nav config.
    pub content_dir: PathBuf,
    /// minijinja HTML templates.
    pub templates_dir: PathBuf,
    /// Static assets (CSS/JS) copied verbatim.
    pub assets_dir: PathBuf,
    /// Auxiliary data (e.g. template-var descriptions).
    pub data_dir: PathBuf,
    /// Output directory.
    pub out_dir: PathBuf,
    /// Base URL path.
    pub base_url: String,
}

impl Config {
    fn new(out_dir: PathBuf, base_url: String) -> Result<Config> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .context("CactupDocs must live one directory below the repo root")?
            .to_path_buf();
        Ok(Config {
            content_dir: manifest_dir.join("content"),
            templates_dir: manifest_dir.join("templates"),
            assets_dir: manifest_dir.join("assets"),
            data_dir: manifest_dir.join("data"),
            manifest_dir,
            repo_root,
            out_dir,
            base_url,
        })
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build { out, base_url } => {
            let cfg = Config::new(out, base_url)?;
            render::build_site(&cfg)?;
            println!("Built docs → {}", cfg.out_dir.display());
        }
        Command::Serve { addr, out } => {
            let cfg = Config::new(out, "/".to_string())?;
            render::build_site(&cfg)?;
            serve::serve(&cfg.out_dir, &addr)?;
        }
        Command::Dump => {
            let cfg = Config::new(PathBuf::from("site"), "/".to_string())?;
            let model = introspect::all(&cfg)?;
            println!("{}", serde_json::to_string_pretty(&model)?);
        }
    }
    Ok(())
}
