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
use crate::commands::build as build_cmd;
use crate::commands::Ctx;
use crate::fetch::{self, git::SourceDiff, link::LinkState, FetchState};
use crate::installation::Installation;
use crate::mdb::Machine;
use crate::thornlist::ComponentType;
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

    // Walk first, print after: every repo gets a full gix status walk, so the
    // walks run on the parallel pool behind a phase-scoped renderer (the same
    // "looks hung without progress" duration `fetch::plan` pays), and the
    // report below then prints in name order from the finished results.
    // `None` = not on disk.
    // Only git components own an arrangement symlink — downloads and external
    // checkouts land straight under their `!TARGET` (see `fetch::plan`, whose
    // `links` list is built in the git arm alone).
    let linked: Vec<&crate::thornlist::Component> =
        list.components().iter().filter(|c| c.ty == ComponentType::Git).collect();

    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let probing = progress.add_child("probe sources");
    probing.init(Some(names.len()), Some(prodash::unit::label("repos")));
    let probing = std::sync::Mutex::new(probing);
    let diffs: Res<Vec<Option<Res<SourceDiff>>>> = crate::par::parallel_map(&names, |repo| {
        let dir = repos_dir.join(repo);
        let diff = dir.is_dir().then(|| {
            let current =
                probing.lock().expect("delta progress poisoned").add_child(repo.clone());
            let diff = fetch::git::source_diff(&dir);
            drop(current);
            diff
        });
        probing.lock().expect("delta progress poisoned").inc();
        diff
    });
    drop(probing);
    // Second phase under the same renderer: resolving one thorn link walks the
    // path component by component with a `canonicalize` at each step, so ~400
    // of them is well past the "looks hung" threshold on a network filesystem.
    let checking = progress.add_child("probe thorn links");
    checking.init(Some(linked.len()), Some(prodash::unit::label("thorns")));
    let checking = std::sync::Mutex::new(checking);
    let states: Res<Vec<Res<LinkState>>> = crate::par::parallel_map(&linked, |c| {
        let current =
            checking.lock().expect("delta progress poisoned").add_child(c.checkout.clone());
        let state = fetch::link::inspect_link(&inst.root, &root, c);
        drop(current);
        checking.lock().expect("delta progress poisoned").inc();
        state
    });
    drop(checking);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    let diffs = diffs?;
    let states = states?;

    let mut clean = 0usize;
    let mut reported = 0usize;
    for (repo, diff) in names.iter().zip(diffs) {
        let diff = match diff {
            None => {
                if recorded.contains_key(repo) {
                    reported += 1;
                    println!("  {} — {}", repo.bold(), "fetched, but no longer on disk".yellow());
                }
                continue;
            }
            Some(Err(e)) => {
                reported += 1;
                println!("  {} — {} {e:#}", repo.bold(), "could not inspect:".yellow());
                continue;
            }
            Some(Ok(diff)) => diff,
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

    // The thorn-link view. A repo can be a pristine git checkout while the
    // arrangement entry that puts its thorn into the build is a hand-placed
    // directory of someone's own source — the repo walk above cannot see that,
    // because the divergence is not *in* any repo.
    let mut sound = 0usize;
    let mut diverged: Vec<(&str, LinkState)> = Vec::new();
    let mut unresolvable: Vec<(&str, String)> = Vec::new();
    for (c, state) in linked.iter().zip(states) {
        match state {
            Ok(state) if state.is_divergence() => diverged.push((c.checkout.as_str(), state)),
            Ok(_) => sound += 1,
            Err(e) => unresolvable.push((c.checkout.as_str(), format!("{e:#}"))),
        }
    }
    diverged.sort_by(|a, b| a.0.cmp(b.0));
    unresolvable.sort();
    if !diverged.is_empty() {
        println!(
            "\n  {}",
            format!("{} thorn(s) are not linked the way the thornlist says:", diverged.len())
                .yellow()
                .bold()
        );
        for (checkout, state) in &diverged {
            let (what, detail) = match state {
                // Spelled out rather than left as a path: this is the case a
                // user reaches by hand and then forgets about.
                LinkState::Replaced { existing } => (
                    "a real directory, not a link into repos/ — built as-is, and no \
                     refetch will replace it",
                    Some(existing.clone()),
                ),
                LinkState::Foreign { target } => {
                    ("links outside repos/ — left untouched by a refetch", Some(target.clone()))
                }
                LinkState::Misdirected { target } => {
                    ("links to the wrong thorn — a refetch would repoint it", Some(target.clone()))
                }
                LinkState::Dangling { target } => {
                    ("links to something that is not there", Some(target.clone()))
                }
                LinkState::Missing => ("not linked into the build at all", None),
                LinkState::Linked => unreachable!("filtered by is_divergence"),
            };
            println!("    {} — {}", checkout.bold(), what.yellow());
            if let Some(detail) = detail {
                println!("      {}", detail.display().to_string().dimmed());
            }
        }
    }
    if !unresolvable.is_empty() {
        println!(
            "\n  {}",
            format!("{} thorn(s) could not be resolved to a link path:", unresolvable.len()).yellow()
        );
        for (checkout, e) in &unresolvable {
            println!("    {} — {e}", checkout.bold());
        }
    }

    // Without a fetch record there is nothing to have diverged *from*, so the
    // summary must not claim repos "match the last fetch".
    let baseline = if recorded.is_empty() { "are clean" } else { "match the last fetch exactly" };
    let all_sound = reported == 0 && diverged.is_empty() && unresolvable.is_empty();
    if all_sound {
        println!(
            "  {}",
            format!("all {clean} repo(s) {baseline}, and all {sound} thorn(s) link into them.")
                .green()
        );
    } else {
        println!("\n  {clean} repo(s) {baseline}; {sound} thorn(s) linked as expected.");
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
        // No machine at hand here (this command never resolves one) — the
        // attempt's own recorded metadata is what answers §7.9's "in
        // flight?" question instead of a scheduler round-trip.
        let config_dir = cactus_root.join("configs").join(&name);
        if let Some(phrase) = build_cmd::in_flight_build(&config_dir, &name, None) {
            bail!("{phrase} — wait for it, or check `cactup build show {name}`");
        }
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
        .and_then(|list| fetch::source_heads_with_progress(&inst.root, &list).ok().flatten());

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

    let repos_dir = inst.cactus_root().join("repos");
    // The per-repo detail (which commit, which files) is a second status walk
    // of just the changed repos; after a big refetch that can be most of the
    // tree, so it runs on the parallel pool before any of it is printed.
    let changed: Vec<String> =
        change.moved.iter().chain(change.edited.iter()).cloned().collect();
    let details: std::collections::BTreeMap<String, SourceDiff> =
        crate::par::parallel_map(&changed, |repo| {
            fetch::git::source_diff(&repos_dir.join(repo)).ok().map(|d| (repo.clone(), d))
        })?
        .into_iter()
        .flatten()
        .collect();
    for repo in &change.moved {
        println!("  {} — {}", repo.bold(), "now on a different commit".yellow());
        if let Some(diff) = details.get(repo) {
            println!(
                "    commit:   {} (built {})",
                short(&diff.head.to_string()),
                meta.sources
                    .as_ref()
                    .and_then(|s| s.get(repo))
                    .map(|s| short(fetch::committed(s)))
                    .unwrap_or_else(|| "?".into())
            );
            print_modified(diff, verbose);
        }
    }
    for repo in &change.edited {
        println!("  {} — {}", repo.bold(), "locally edited since the build".yellow());
        if let Some(diff) = details.get(repo) {
            print_modified(diff, verbose);
        }
    }
    // No `details` entry is possible for these — that they cannot be inspected
    // as git repos is the whole finding.
    for repo in &change.vanished {
        let dir = repos_dir.join(repo);
        println!(
            "  {} — {}",
            repo.bold(),
            if dir.is_dir() {
                "no longer a git repo; cactup cannot tell what this builds from".yellow()
            } else {
                "gone from disk since the build".yellow()
            }
        );
        println!("    {}", dir.display().to_string().dimmed());
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
        .and_then(|list| fetch::source_heads_with_progress(&inst.root, &list).ok().flatten());
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
    if !change.vanished.is_empty() {
        what.push(format!("{} no longer inspectable", change.vanished.len()));
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

/// A build is not portable across machines (§7.4): refuse to use `what` (a
/// config or simulation whose metadata records `recorded` as its machine)
/// while resolved to a different one, unless the caller was told to ignore
/// the mismatch — then say so and proceed. Every path that turns a config
/// into a job (sim create/submit/run, test run/submit, build of an existing
/// config) goes through this, before anything is written.
pub fn check_machine(current: &Machine, recorded: &str, what: &str, ignore: bool) -> Res<()> {
    if recorded == current.name {
        return Ok(());
    }
    if !ignore {
        bail!(
            "{what} was built for machine \"{recorded}\" but this is machine \"{}\"; \
             pass --ignore-machine (or -f) to use it anyway, at your own risk",
            current.name
        );
    }
    println!(
        "{} {what} was built for machine {} but this is machine {}; proceeding as asked.",
        "note:".yellow().bold(),
        recorded.bold(),
        current.name.bold()
    );
    Ok(())
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
    let live = inst.live_thornlist_to_read();
    let pristine = inst.source_thornlist_to_read();
    let path: &Path = if live.is_file() { &live } else { &pristine };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read thornlist {}", path.display()))?;
    crate::thornlist::parse(&text)
        .with_context(|| format!("Failed to parse thornlist {}", path.display()))
}
