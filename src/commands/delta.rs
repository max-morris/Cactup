//! `cactup installation delta` and `cactup config delta` (§7.4): what the
//! source trees look like now, against a recorded baseline.
//!
//! Two different baselines, one machine:
//!
//! - `installation delta` compares against **the last fetch**
//!   (`<root>/.cactup/fetch-state.toml`) — "what have I changed since cactup
//!   put these sources here".
//! - `config delta` compares against **the last build of that config**
//!   (`ConfigMeta.sources`) — "what would rebuilding pick up", which is
//!   exactly the input `build::rebuild_decision` acts on.
//!
//! Both are strictly read-only: no locks, no writes, no network.

use crate::build::{self, ConfigMeta, SourceDelta};
use crate::commands::Ctx;
use crate::fetch::{self, git::SourceDiff, FetchState};
use crate::installation::Installation;
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::path::Path;

/// How many modified paths to print per repo before summarizing. `--verbose`
/// prints all of them.
const PATHS_SHOWN: usize = 6;

/// `cactup installation delta [ALIAS]` — divergence from the last fetch.
pub fn installation_delta(ctx: &Ctx, alias: Option<String>) -> Res<()> {
    let inst = match alias {
        // A named installation need not be the active one — this is a
        // read-only view, so there is no reason to make the user `use` it.
        Some(alias) => {
            let db = ctx.db.read()?;
            let entry = db
                .installations
                .get(&alias)
                .ok_or_else(|| anyhow::anyhow!("no installation named \"{alias}\" (see `cactup list`)"))?;
            Installation::new(alias, &entry.path)
        }
        None => Installation::resolve(ctx)?,
    };
    let list = read_live_thornlist(&inst)?;
    let root = list.root().to_owned();
    let repos_dir = inst.root.join(&root).join("repos");

    // The baseline: what the last fetch left behind. Absent for an
    // installation created before the native fetcher, which is worth saying
    // out loud rather than rendering as "everything diverged".
    let state = FetchState::read(&inst.root)?;
    println!("{} — divergence from the last fetch", inst.alias.bold());
    if state.is_none() {
        println!(
            "{} no fetch record ({}); cactup did not fetch this tree, so there is no \
             baseline to compare commits against. Local modifications are still reported.",
            "note:".yellow().bold(),
            format!("{}/.cactup/fetch-state.toml", inst.root.display()).dimmed()
        );
    }
    let recorded = state.map(|s| s.repos).unwrap_or_default();

    // Every repo the thornlist names, plus anything the fetch recorded that
    // the thornlist has since dropped (those are real divergence too).
    let mut names: Vec<String> = list.components().iter().map(|c| c.repo.clone()).collect();
    names.extend(recorded.keys().cloned());
    names.sort();
    names.dedup();

    let mut clean = 0usize;
    let mut reported = 0usize;
    for repo in &names {
        let dir = repos_dir.join(repo);
        if !dir.is_dir() {
            if recorded.contains_key(repo) {
                reported += 1;
                println!("  {} — {}", repo.bold(), "fetched, but no longer on disk".yellow());
            }
            continue;
        }
        let diff = match fetch::git::source_diff(&dir) {
            Ok(diff) => diff,
            Err(e) => {
                reported += 1;
                println!("  {} — {} {e:#}", repo.bold(), "could not inspect:".yellow());
                continue;
            }
        };
        let record = recorded.get(repo);
        let moved = record.is_some_and(|r| r.head != diff.head.to_string());
        let branch_changed = record
            .and_then(|r| r.branch.as_deref())
            .is_some_and(|b| diff.branch.as_deref() != Some(b));
        if !moved && !branch_changed && diff.modified.is_empty() && diff.untracked.is_empty() {
            clean += 1;
            continue;
        }
        reported += 1;
        println!("  {}", repo.bold());
        if branch_changed {
            println!(
                "    branch:   {} (fetched on {})",
                diff.branch.as_deref().unwrap_or("(detached)").yellow(),
                record.and_then(|r| r.branch.as_deref()).unwrap_or("?")
            );
        }
        if moved {
            println!(
                "    commit:   {} (fetched {})",
                short(&diff.head.to_string()).yellow(),
                record.map(|r| short(&r.head)).unwrap_or_else(|| "?".into())
            );
        }
        print_modified(&diff, ctx.globals.verbose);
        // Untracked files never affect a build (nothing compiles them until
        // they are listed in a tracked make.code.defn) but they do block
        // `--prune`, so this view is where they belong.
        if !diff.untracked.is_empty() {
            println!(
                "    untracked: {} file(s) {}",
                diff.untracked.len(),
                "— ignored by builds; blocks --prune".dimmed()
            );
            if ctx.globals.verbose {
                for path in &diff.untracked {
                    println!("      {path}");
                }
            }
        }
    }

    // Without a fetch record there is nothing to have diverged *from*, so the
    // summary must not claim repos "match the last fetch".
    let baseline = if recorded.is_empty() { "are clean" } else { "match the last fetch exactly" };
    if reported == 0 {
        println!("  {}", format!("all {clean} repo(s) {baseline}.").green());
    } else {
        println!("\n  {clean} repo(s) {baseline}.");
        println!(
            "  {}",
            "`cactup inst refetch -n` shows what a refetch would do with this state.".dimmed()
        );
    }
    Ok(())
}

/// `cactup config delta [NAME]` — divergence from the last build.
pub fn config_delta(inst: &Installation, name: Option<String>, verbose: bool) -> Res<()> {
    let cactus_root = inst.cactus_root();
    let name = match name {
        Some(name) => name,
        None => inst.meta()?.active_config.ok_or_else(|| {
            anyhow::anyhow!(
                "no active config; name one, or run `cactup config use <name>` (see \
                 `cactup config list`)"
            )
        })?,
    };
    let Some(meta) = ConfigMeta::load(&cactus_root, &name)? else {
        bail!("config \"{name}\" has never been built (no cactup-config.toml)");
    };

    println!(
        "{} — divergence from the last build{}",
        name.bold(),
        meta.built.map(|t| format!(" ({})", t.format("%Y-%m-%d %H:%M UTC"))).unwrap_or_default()
    );

    // The config's *processed* thornlist is the authority on what it builds,
    // and it is what the rebuild decision reads.
    let processed = cactus_root.join("configs").join(&name).join(build::THORNLIST_PROCESSED);
    let text = std::fs::read_to_string(&processed)
        .with_context(|| format!("Failed to read {}", processed.display()))?;
    let live = crate::thornlist::parse(&text)
        .ok()
        .and_then(|list| fetch::source_heads(&inst.root, &list).ok().flatten());

    let (delta, change) = build::source_delta(meta.sources.as_ref(), live.as_ref());
    if delta == SourceDelta::Unknown {
        println!(
            "{} no source baseline recorded for this config{}. Run `cactup build {name}` to \
             establish one; after that, every later change is reported here.",
            "note:".yellow().bold(),
            if meta.sources.is_none() { " (built before source tracking)" } else { "" }
        );
        return Ok(());
    }

    let repos_dir = inst.root.join("Cactus").join("repos");
    for repo in &change.moved {
        println!("  {} — {}", repo.bold(), "now on a different commit".yellow());
        if let Ok(diff) = fetch::git::source_diff(&repos_dir.join(repo)) {
            println!(
                "    commit:   {} (built {})",
                short(&diff.head.to_string()),
                meta.sources
                    .as_ref()
                    .and_then(|s| s.get(repo))
                    .map(|s| short(fetch::committed(s)))
                    .unwrap_or_else(|| "?".into())
            );
            print_modified(&diff, verbose);
        }
    }
    for repo in &change.edited {
        println!("  {} — {}", repo.bold(), "locally edited since the build".yellow());
        if let Ok(diff) = fetch::git::source_diff(&repos_dir.join(repo)) {
            print_modified(&diff, verbose);
        }
    }

    match delta {
        SourceDelta::Unchanged => {
            println!("  {}", "the source tree matches what this config was built from.".green());
        }
        SourceDelta::Flesh => println!(
            "\n  {} the Cactus flesh moved, so `cactup build {name}` rebuilds from scratch.",
            "→".bold()
        ),
        SourceDelta::Thorns | SourceDelta::Edited => println!(
            "\n  {} `cactup build {name}` reconfigures and rebuilds what this affects.",
            "→".bold()
        ),
        SourceDelta::Unknown => unreachable!("handled above"),
    }
    Ok(())
}

/// A non-blocking notice for `sim submit` / `sim run` (§8.3): the source tree
/// no longer matches what this config's executable was built from.
///
/// Deliberately *only* a warning. Starting a run is not the place to
/// recompile — a rebuild happens when the user asks for one, with `cactup
/// build` — and a run whose sources have moved is often exactly what was
/// intended (the executable is already built and frozen per simulation).
/// Suppressed by `-s/--silent`, and any inspection failure is swallowed:
/// nothing here may stand between the user and their job.
pub fn warn_if_sources_diverged(inst: &Installation, meta: &ConfigMeta, silent: bool) {
    if silent || meta.sources.is_none() {
        return;
    }
    let processed =
        inst.cactus_root().join("configs").join(&meta.name).join(build::THORNLIST_PROCESSED);
    let Ok(text) = std::fs::read_to_string(&processed) else { return };
    let live = crate::thornlist::parse(&text)
        .ok()
        .and_then(|list| fetch::source_heads(&inst.root, &list).ok().flatten());
    let (delta, change) = build::source_delta(meta.sources.as_ref(), live.as_ref());
    if matches!(delta, SourceDelta::Unknown | SourceDelta::Unchanged) {
        return;
    }
    let mut what = Vec::new();
    if !change.moved.is_empty() {
        what.push(format!("{} on a different commit", change.moved.len()));
    }
    if !change.edited.is_empty() {
        what.push(format!("{} locally edited", change.edited.len()));
    }
    println!(
        "{} the source tree has moved since config {} was built ({}). This run uses the \
         executable as built; `cactup config delta {}` shows what, `cactup build {}` \
         recompiles, `-s` silences this.",
        "note:".yellow().bold(),
        meta.name.bold(),
        what.join(", "),
        meta.name,
        meta.name
    );
}

/// Shared "what changed in the worktree" block.
fn print_modified(diff: &SourceDiff, verbose: bool) {
    if diff.modified.is_empty() {
        return;
    }
    println!(
        "    modified: {} file(s)",
        diff.modified.len().to_string().yellow()
    );
    let shown = if verbose { diff.modified.len() } else { PATHS_SHOWN.min(diff.modified.len()) };
    for path in &diff.modified[..shown] {
        println!("      {path}");
    }
    if shown < diff.modified.len() {
        println!(
            "      {} (pass --verbose for all)",
            format!("+{} more", diff.modified.len() - shown).dimmed()
        );
    }
}

fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// The installation's live thornlist — the same file `build::resolve_thornlist`
/// treats as the default, falling back to the pristine as-fetched copy.
fn read_live_thornlist(inst: &Installation) -> Res<crate::thornlist::Thornlist> {
    let live = inst.cactus_root().join("thornlists/einsteintoolkit.th");
    let pristine = inst.root.join("einsteintoolkit.th");
    let path: &Path = if live.is_file() { &live } else { &pristine };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read thornlist {}", path.display()))?;
    crate::thornlist::parse(&text)
        .with_context(|| format!("Failed to parse thornlist {}", path.display()))
}
