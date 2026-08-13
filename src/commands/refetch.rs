//! `cactup installation refetch` — re-run the component fetch (spec §3.2):
//! update repos, adopt a new thornlist or release, add new thorns — never
//! clobbering hand-modified thorns unless forced.
//!
//! Locking: holds only the per-installation *fetch* lock
//! (`<root>/.cactup/.cactup-fetch.lock`, §2.3 item 6) with a heartbeat, so a
//! long fetch never blocks `sim create`/`config use` (which take the
//! installation lock). Nothing here may call `Installation::locked()` or
//! `ensure_meta` (non-reentrant); the only other lock taken is the global DB
//! lock, briefly, inside `ctx.db.update` at the very end.

use super::{prompt_with_default, Ctx};
use crate::args::RefetchArgs;
use crate::fetch::{self, link::LinkOutcome, GitAction};
use crate::installation::Installation;
use crate::lock::LinkLock;
use crate::thornlist::{self, Thornlist};
use crate::{manifest, shell, Res};
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Where the thornlist being fetched came from; decides which files are
/// rewritten and what the DB records (§3.2).
enum Source {
    /// `--release TAG`: bytes read from the manifest tag.
    Release { tag: String, bytes: String },
    /// Positional THORNLIST file.
    File { path: PathBuf, bytes: String },
    /// No argument: the installation's own live thornlist.
    Live { path: PathBuf, bytes: String },
}

impl Source {
    fn bytes(&self) -> &str {
        match self {
            Source::Release { bytes, .. } | Source::File { bytes, .. } | Source::Live { bytes, .. } => bytes,
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::Release { tag, .. } => format!("release {tag}"),
            Source::File { path, .. } => format!("thornlist {}", path.display()),
            Source::Live { path, .. } => format!("live thornlist {}", path.display()),
        }
    }

    /// Explicit sources replace the live thornlist; a no-argument refetch
    /// reads it as its own source, so there is nothing to replace.
    fn is_explicit(&self) -> bool {
        !matches!(self, Source::Live { .. })
    }
}

pub fn dispatch(ctx: &Ctx, args: RefetchArgs) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let overwrite_modified = args.overwrite_modified || args.force;
    let replace_thornlist = args.replace_thornlist || args.force;

    // §2.3 item 6: the fetch lock. Dry-run stays read-only but still probes
    // it — classifying 81 worktrees mid-checkout would be garbage.
    let lock_path = inst.cactup_dir().join(".cactup-fetch.lock");
    fs::create_dir_all(inst.cactup_dir())
        .with_context(|| format!("Failed to create {}", inst.cactup_dir().display()))?;
    let _lock = if args.dry_run {
        match LinkLock::try_acquire(&lock_path)? {
            Some(lock) => Some(lock.with_heartbeat()),
            None => {
                println!(
                    "{}",
                    "another refetch holds the fetch lock; this classification may be stale"
                        .yellow()
                );
                None
            }
        }
    } else {
        Some(LinkLock::acquire(&lock_path)?.with_heartbeat())
    };

    // Thornlist source, in precedence order (§3.2).
    let source = resolve_source(ctx, &inst, &args)?;
    println!("Refetching {} from {}.", inst.alias.bold(), source.describe());

    let include_base = match &source {
        Source::File { path, .. } | Source::Live { path, .. } => path.parent().map(Path::to_path_buf),
        Source::Release { .. } => None,
    };
    let list = thornlist::parse_with_base(source.bytes(), include_base.as_deref())
        .with_context(|| format!("Failed to parse {}", source.describe()))?;
    for w in list.warnings() {
        println!("{}", format!("thornlist warning: {w}").yellow());
    }

    // The in-place-edit guard (§3.2): an explicit source is about to replace
    // the live thornlist. If the live copy diverged from the pristine
    // as-fetched copy, that is a hand edit (e.g. a hand-added thorn) and we
    // refuse to destroy it silently. Compared as parsed component sets, not
    // text — generated headers differ on every pre-existing installation.
    let live_path = inst.live_thornlist();
    let pristine_path = inst.root.join("einsteintoolkit.th");
    let divergence = if source.is_explicit() { live_divergence(&live_path, &pristine_path)? } else { Vec::new() };
    if !divergence.is_empty() && !replace_thornlist && !args.dry_run {
        println!(
            "{}",
            format!(
                "{} has hand edits not present in the pristine as-fetched copy ({}):",
                live_path.display(),
                pristine_path.display()
            )
            .bright_red()
        );
        for line in &divergence {
            println!("  {line}");
        }
        bail!(
            "refusing to replace a hand-edited thornlist; re-run with \
             --replace-thornlist (or -f) to overwrite it — a snapshot will be \
             kept under {}",
            inst.cactup_dir().join("thornlists").display()
        );
    }

    // Pure, read-only classification. Phase-scoped renderer: probing every
    // repo (a gix status walk each) can take a while on ~80 repos, and
    // without a bar that looks like a hang before anything else appears.
    // The renderer draws on stderr while the rest of this command prints to
    // stdout, so it must be shut down before any of that printing starts.
    let (progress, renderer) = manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let mut plan = fetch::plan(&list, &inst.root, &mut classify)?;
    drop(classify);
    renderer.shutdown_and_wait();

    // The skip warning block: before fetching, so it is seen up front, and
    // the same lines again in the summary. Under -s only the one-line count
    // survives — silence the block, never the fact.
    if !plan.skipped.is_empty() {
        println!(
            "{}",
            format!("{} repo(s) will be skipped (local state preserved).", plan.skipped.len()).yellow().bold()
        );
        if !args.silent {
            print_skip_block(&plan.skipped, overwrite_modified);
        }
    }

    // Orphans and config interactions are computed up front too — dry-run
    // prints all of it, and the real run needs them after the fetch anyway.
    let orphans = enumerate_orphans(&inst, &list, &plan)?;
    let configs = crate::commands::config::list_configs(&inst.cactus_root())?;

    if args.dry_run {
        print_dry_run(&inst, &plan, &orphans, &configs, &divergence, &args, overwrite_modified);
        return Ok(());
    }

    // --overwrite-modified: back the dirty repos' modified files up outside
    // the installation (uninstall deletes the installation), then fetch them
    // like clean repos.
    if overwrite_modified && !plan.skipped.is_empty() {
        let backup_root = backup_dirty(&inst, &plan.skipped)?;
        println!(
            "Backed up locally-modified files to {} — restore by copying them back.",
            backup_root.display().to_string().bold()
        );
        plan.force();
    }

    let had_work = !plan.git.is_empty() || !plan.downloads.is_empty() || !plan.external.is_empty();
    let report = fetch::execute(&plan, &inst.root)?;

    // Post-pass bookkeeping: record what we fetched (prune safety + the
    // future rebuild-decision hookup), then report.
    if !report.repos.is_empty() {
        fetch::FetchState::record(&inst.root, &report.records())?;
    }
    let changed = report.repos.iter().filter(|r| r.changed).count();
    let up_to_date = report.repos.len() - changed;
    println!(
        "Fetched {} repo(s): {} updated, {} already up to date; {} download(s).",
        report.repos.len(),
        changed,
        up_to_date,
        report.downloads.len()
    );
    if ctx.globals.verbose {
        for r in &report.repos {
            let mark = if r.changed { "updated" } else { "up to date" };
            let forced = r.forced.as_deref().map(|f| format!(" (forced: {f})")).unwrap_or_default();
            println!("  {} @ {} — {mark}{forced}", r.repo, r.branch.as_deref().unwrap_or("<default>"));
        }
    }
    let blocked: Vec<_> = report
        .links
        .iter()
        .filter_map(|(checkout, o)| match o {
            LinkOutcome::Blocked { existing } => Some((checkout, existing)),
            _ => None,
        })
        .collect();
    if !blocked.is_empty() {
        println!("{}", format!("{} symlink(s) not touched (real files or foreign links):", blocked.len()).yellow());
        for (checkout, existing) in blocked {
            println!("  {checkout}: {}", existing.display());
        }
    }
    if !plan.skipped.is_empty() {
        println!(
            "{}",
            format!("{} repo(s) skipped (local state preserved).", plan.skipped.len()).yellow().bold()
        );
        if !args.silent {
            print_skip_block(&plan.skipped, overwrite_modified);
        }
    }

    // Orphan handling: always reported; removed only under --prune.
    report_orphans(&orphans);
    if args.prune && !orphans.is_empty() {
        prune_orphans(&inst, &orphans, &configs, &args)?;
    }

    // Thornlist writes. The pristine as-fetched copy always tracks what was
    // fetched; the live copy is only replaced by an explicit source (and the
    // old live copy is snapshotted first).
    let fetch_ok = report.failures.is_empty();
    let mut recorded = false;
    if fetch_ok || had_work {
        if source.is_explicit() {
            snapshot_live(&inst, &live_path)?;
            fs::write(&live_path, source.bytes())
                .with_context(|| format!("Failed to write {}", live_path.display()))?;
            // The pristine as-fetched baseline tracks the last *adopted
            // official source* only. A no-argument refetch must NOT touch it
            // — hand edits in the live file stay visible as divergence, so
            // the guard above keeps protecting them on the next explicit
            // refetch even after they have been fetched once.
            fs::write(&pristine_path, source.bytes())
                .with_context(|| format!("Failed to write {}", pristine_path.display()))?;
        }

        // DB provenance (§2.1): only for explicit sources, and only when the
        // tree fully matches the new list (no skips, unless they were forced
        // in and fetched).
        if source.is_explicit() && plan.skipped.is_empty() && fetch_ok {
            let (current_release, current_thornlist) = match &source {
                Source::Release { tag, .. } => (Some(tag.clone()), None),
                Source::File { path, .. } => (None, Some(path.display().to_string())),
                Source::Live { .. } => unreachable!(),
            };
            let alias = inst.alias.clone();
            ctx.db.update(move |db| {
                let entry = db
                    .installations
                    .get_mut(&alias)
                    .ok_or_else(|| anyhow!("installation \"{alias}\" vanished from the database"))?;
                entry.current_release = current_release.clone();
                entry.current_thornlist = current_thornlist.clone();
                Ok(())
            })?;
            recorded = true;
            match &source {
                Source::Release { tag, .. } => println!(
                    "This installation is now on {} (its install-time provenance is preserved; \
                     `cactup list` shows both).",
                    tag.bold()
                ),
                Source::File { path, .. } => println!(
                    "This installation now tracks the custom thornlist {} (install-time \
                     provenance preserved).",
                    path.display().to_string().bold()
                ),
                Source::Live { .. } => unreachable!(),
            }
        }
    }

    // Config interaction (§7.5): a refetch does not, by itself, cause any
    // config to rebuild — it defers to build-time detection
    // (`rebuild_decision`), which now sees both the per-repo source HEADs
    // (§7.4) and each thorn's recorded provider, so `report_configs`'s claim
    // that a plain `cactup build` does the right thing holds even when the
    // refetched thornlist re-points a thorn *name* at a different provider.
    if changed > 0 && !configs.is_empty() {
        report_configs(&inst, &configs, matches!(source, Source::Release { .. }));
    }

    if !report.failures.is_empty() {
        println!("{}", format!("{} fetch failure(s):", report.failures.len()).bright_red().bold());
        for f in &report.failures {
            println!("  {}: {}", f.what.bold(), f.error);
        }
        println!(
            "The recorded thornlist {} updated.",
            if source.is_explicit() {
                if recorded { "WAS" } else { "was written to disk but NOT recorded in the database; re-run refetch to finish" }
            } else {
                "did not need to be"
            }
        );
        bail!("refetch completed with {} failure(s)", report.failures.len());
    }

    Ok(())
}

/// §3.2 source precedence: `--release TAG` → positional THORNLIST → the
/// live `Cactus/thornlists/einsteintoolkit.th` (falling back to the pristine
/// root copy on very old trees).
fn resolve_source(ctx: &Ctx, inst: &Installation, args: &RefetchArgs) -> Res<Source> {
    if let Some(tag_name) = &args.release {
        let repo = manifest::ensure_manifest_repo(&crate::CACTUP_ROOT, &ctx.globals.manifest_url)?;
        let tags = manifest::get_tags(&repo)?;
        let tag = manifest::find_tag(&tags, tag_name)
            .ok_or_else(|| anyhow!("{tag_name} is not a valid release (see `cactup releases`)"))?;
        let bytes = tag
            .read_file("einsteintoolkit.th")
            .with_context(|| format!("release {tag_name} has no einsteintoolkit.th"))?;
        let bytes = String::from_utf8(bytes)
            .with_context(|| format!("release {tag_name}'s thornlist is not UTF-8"))?;
        return Ok(Source::Release { tag: tag_name.clone(), bytes });
    }
    if let Some(path) = &args.thornlist {
        let expanded = shell::expand_path(&super::p2s(path.clone())?);
        let expanded = PathBuf::from(&expanded);
        let path = fs::canonicalize(&expanded).unwrap_or(expanded);
        let bytes =
            fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        return Ok(Source::File { path, bytes });
    }
    let live = inst.live_thornlist();
    let path = if live.exists() { live } else { inst.root.join("einsteintoolkit.th") };
    let bytes =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(Source::Live { path, bytes })
}

/// Parse both the live and the pristine thornlist and diff their component
/// sets and #DISABLED sets. Empty = no hand edits (or nothing to compare —
/// a missing/unparsable pristine copy can't witness an edit; the refetch
/// will write one, arming the guard for next time).
fn live_divergence(live: &Path, pristine: &Path) -> Res<Vec<String>> {
    let (Ok(live_src), Ok(pristine_src)) = (fs::read_to_string(live), fs::read_to_string(pristine))
    else {
        return Ok(Vec::new());
    };
    let (Ok(live_list), Ok(pristine_list)) = (thornlist::parse(&live_src), thornlist::parse(&pristine_src))
    else {
        return Ok(Vec::new());
    };

    let key = |c: &crate::thornlist::Component| format!("{}/{} <- {}", c.target, c.checkout, c.url.as_deref().unwrap_or(""));
    let live_set: BTreeSet<String> = live_list.components().iter().map(key).collect();
    let pristine_set: BTreeSet<String> = pristine_list.components().iter().map(key).collect();
    let live_disabled: BTreeSet<&String> = live_list.disabled_thorns().iter().collect();
    let pristine_disabled: BTreeSet<&String> = pristine_list.disabled_thorns().iter().collect();

    let mut out = Vec::new();
    for added in live_set.difference(&pristine_set) {
        out.push(format!("added:   {added}"));
    }
    for removed in pristine_set.difference(&live_set) {
        out.push(format!("removed: {removed}"));
    }
    for d in live_disabled.symmetric_difference(&pristine_disabled) {
        out.push(format!("#DISABLED toggled: {d}"));
    }
    Ok(out)
}

fn print_skip_block(skipped: &[fetch::SkippedRepo], overwrite_modified: bool) {
    for s in skipped {
        println!("  {} — {}", s.repo.bold(), s.reason.describe());
        println!("    thorns: {}", s.checkouts.join(", "));
        if !s.untracked.is_empty() {
            println!("    ({} untracked file(s), which never block a fetch)", s.untracked.len());
        }
    }
    if !overwrite_modified {
        println!(
            "  Pass {} to fetch over these anyway (modified files are backed up \
             first), or {} to hide this list.",
            "--overwrite-modified / -f".bold(),
            "-s/--silent".bold()
        );
    }
}

/// A repo under `<root>/repos/` (or an arrangement symlink) the new
/// thornlist no longer accounts for.
struct Orphan {
    repo: String,
    dir: PathBuf,
    /// Arrangement symlinks pointing into this repo.
    symlinks: Vec<PathBuf>,
    /// Prunable = cactup's own fetch-state records having fetched it.
    /// Everything else is report-only, always.
    prunable: bool,
}

/// The "still referenced" set is the thornlist's repos UNION the repos that
/// back its `#DISABLED` thorns (a machine's enabled-thorns can re-enable
/// those at build time, so their repos are not garbage).
fn enumerate_orphans(inst: &Installation, list: &Thornlist, plan: &fetch::Plan) -> Res<Vec<Orphan>> {
    let repos_dir = inst.root.join(&plan.root).join("repos");
    if !repos_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut referenced: BTreeSet<String> = list.components().iter().map(|c| c.repo.clone()).collect();
    // Resolve each #DISABLED thorn's arrangement symlink to the repo backing
    // it, and keep that repo too.
    let arrangements = inst.cactus_root().join("arrangements");
    for thorn in list.disabled_thorns() {
        let link = arrangements.join(thorn);
        if let Ok(target) = fs::read_link(&link)
            && let Some(repo) = repo_of_link_target(&link, &target, &repos_dir)
        {
            referenced.insert(repo);
        }
    }

    let state = fetch::FetchState::read(&inst.root)?;
    let mut orphans = Vec::new();
    for entry in fs::read_dir(&repos_dir).with_context(|| format!("Failed to list {}", repos_dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let repo = entry.file_name().to_string_lossy().into_owned();
        if referenced.contains(&repo) {
            continue;
        }
        let prunable = state.as_ref().is_some_and(|s| s.repos.contains_key(&repo));
        orphans.push(Orphan {
            symlinks: symlinks_into(&arrangements, &repos_dir, &repo)?,
            dir: entry.path(),
            repo,
            prunable,
        });
    }
    Ok(orphans)
}

/// Which repo (immediate child of `repos_dir`) a symlink target resolves
/// into, if any. Lexical only.
fn repo_of_link_target(link: &Path, target: &Path, repos_dir: &Path) -> Option<String> {
    let base = link.parent()?;
    let joined = if target.is_absolute() { target.to_path_buf() } else { base.join(target) };
    let mut parts = Vec::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str()),
        }
    }
    let normalized: PathBuf = parts.iter().collect();
    let rel = normalized.strip_prefix(repos_dir).ok()?;
    rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())
}

/// All symlinks under `arrangements/` (two levels deep — arrangement dirs
/// and their thorn entries) that point into `repos_dir/<repo>`.
fn symlinks_into(arrangements: &Path, repos_dir: &Path, repo: &str) -> Res<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(top) = fs::read_dir(arrangements) else { return Ok(out) };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in top.flatten() {
        let path = entry.path();
        candidates.push(path.clone());
        if path.is_dir() && !path.is_symlink() {
            if let Ok(inner) = fs::read_dir(&path) {
                candidates.extend(inner.flatten().map(|e| e.path()));
            }
        }
    }
    for path in candidates {
        if let Ok(target) = fs::read_link(&path)
            && repo_of_link_target(&path, &target, repos_dir).as_deref() == Some(repo)
        {
            out.push(path);
        }
    }
    Ok(out)
}

fn report_orphans(orphans: &[Orphan]) {
    if orphans.is_empty() {
        return;
    }
    println!("{}", format!("{} repo(s) on disk are not in the thornlist:", orphans.len()).yellow().bold());
    for o in orphans {
        let tag = if o.prunable {
            "prunable with --prune"
        } else {
            "not created by cactup — report-only, never pruned"
        };
        println!("  {} ({tag}; {} symlink(s))", o.repo.bold(), o.symlinks.len());
    }
}

/// `--prune`: remove prunable orphans and their arrangement symlinks, behind
/// a single confirmation. Only `-f` bypasses the confirmation (never `-s`).
fn prune_orphans(
    inst: &Installation,
    orphans: &[Orphan],
    configs: &[(String, Option<crate::build::ConfigMeta>)],
    args: &RefetchArgs,
) -> Res<()> {
    // §11.5: test runs execute from the live source tree; pruning deletes
    // reference data they need. Follow the config-delete precedent: refuse
    // without -f while any test run is registered.
    if let Ok(tests) = inst.tests()
        && !tests.tests.is_empty()
        && !args.force
    {
        println!(
            "{}",
            format!(
                "not pruning: {} registered test run(s) execute from this source tree \
                 (pass -f to prune anyway)",
                tests.tests.len()
            )
            .yellow()
        );
        return Ok(());
    }

    // A repo that supplies a thorn in some config's processed thornlist is
    // never pruned — that config's build would fail outright.
    let mut in_use: BTreeSet<String> = BTreeSet::new();
    for (name, _) in configs {
        let processed = inst.cactus_root().join("configs").join(name).join("cactup-thornlist.th");
        if let Ok(text) = fs::read_to_string(&processed) {
            for orphan in orphans {
                for link in &orphan.symlinks {
                    // arrangements/<Arrangement>/<Thorn> → "Arrangement/Thorn"
                    let thorn: Vec<_> = link
                        .components()
                        .rev()
                        .take(2)
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect();
                    if thorn.len() == 2 {
                        let thorn = format!("{}/{}", thorn[1], thorn[0]);
                        if text.lines().any(|l| l.trim() == thorn) {
                            in_use.insert(orphan.repo.clone());
                        }
                    }
                }
            }
        }
    }

    let mut removable: Vec<&Orphan> = Vec::new();
    for o in orphans {
        if !o.prunable {
            continue;
        }
        if in_use.contains(&o.repo) {
            println!(
                "{}",
                format!("not pruning {}: a built config's thornlist still names its thorns", o.repo).yellow()
            );
            continue;
        }
        // Untracked or modified content blocks prune (the ET test harness
        // leaves output in the source tree); -f prunes with a full backup.
        let state = fetch::FetchState::read(&inst.root)?;
        let record = state.as_ref().and_then(|s| s.repos.get(&o.repo));
        let probe = fetch::git::probe(
            &o.dir,
            record.map(|r| r.url.as_str()).unwrap_or(""),
            record.and_then(|r| r.branch.as_deref()).unwrap_or(""),
        );
        let dirty = !matches!(probe.state, fetch::git::RepoState::Clean { .. }) || !probe.untracked.is_empty();
        if dirty && !args.force {
            println!(
                "{}",
                format!(
                    "not pruning {}: it has local modifications or untracked files (pass -f \
                     to prune with a backup)",
                    o.repo
                )
                .yellow()
            );
            continue;
        }
        removable.push(o);
    }

    if removable.is_empty() {
        println!("Nothing prunable.");
        return Ok(());
    }

    let n_links: usize = removable.iter().map(|o| o.symlinks.len()).sum();
    println!("{}", format!("--prune will remove {} repo(s) and {} symlink(s):", removable.len(), n_links).bold());
    for o in &removable {
        println!("  {}", o.dir.display());
        for link in &o.symlinks {
            println!("    {}", link.display());
        }
    }
    if !args.force {
        let answer = prompt_with_default("Remove them?", "no")?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("Not pruning.");
            return Ok(());
        }
    }

    for o in &removable {
        // -f on a dirty orphan: whole-repo backup before deletion (the same
        // guarantee as --overwrite-modified, scaled to "everything").
        let state = fetch::FetchState::read(&inst.root)?;
        let record = state.as_ref().and_then(|s| s.repos.get(&o.repo));
        let probe = fetch::git::probe(
            &o.dir,
            record.map(|r| r.url.as_str()).unwrap_or(""),
            record.and_then(|r| r.branch.as_deref()).unwrap_or(""),
        );
        let dirty = !matches!(probe.state, fetch::git::RepoState::Clean { .. }) || !probe.untracked.is_empty();
        if dirty {
            let backup = backup_dir(&inst.alias)?.join(&o.repo);
            copy_tree(&o.dir, &backup)?;
            println!("Backed up {} to {}.", o.repo, backup.display());
        }
        // Symlinks first, then the repo they point into.
        for link in &o.symlinks {
            fs::remove_file(link).with_context(|| format!("Failed to remove {}", link.display()))?;
        }
        fs::remove_dir_all(&o.dir).with_context(|| format!("Failed to remove {}", o.dir.display()))?;
        println!("Pruned {}.", o.repo.bold());
    }
    Ok(())
}

fn print_dry_run(
    inst: &Installation,
    plan: &fetch::Plan,
    orphans: &[Orphan],
    configs: &[(String, Option<crate::build::ConfigMeta>)],
    divergence: &[String],
    args: &RefetchArgs,
    overwrite_modified: bool,
) {
    println!("{}", "Dry run — nothing will be touched.".bold());
    for item in &plan.git {
        let action = match item.action {
            GitAction::Clone => "clone",
            GitAction::Update => "fetch + fast-forward",
            GitAction::Align => "switch branch to",
        };
        println!(
            "  {} — {action} {} ({} thorn(s))",
            item.repo.bold(),
            item.branch.as_deref().unwrap_or("<default branch>"),
            item.checkouts.len()
        );
    }
    for c in &plan.downloads {
        println!("  {} — download", c.checkout.bold());
    }
    for c in &plan.external {
        println!("  {} — {:?} via system tool", c.checkout.bold(), c.ty);
    }
    for s in &plan.skipped {
        let forced = if overwrite_modified { " (would be fetched: forced)" } else { "" };
        println!("  {} — {} {}{forced}", s.repo.bold(), "SKIP:".yellow(), s.reason.describe());
    }
    if plan.git.is_empty() && plan.skipped.is_empty() && plan.downloads.is_empty() && plan.external.is_empty() {
        println!("  everything is up to date");
    }
    report_orphans(orphans);
    if args.prune {
        println!("  (--prune would remove only the repos marked prunable above, after confirmation)");
    }
    if !divergence.is_empty() {
        println!("{}", "The live thornlist has hand edits vs. the pristine copy:".yellow());
        for d in divergence {
            println!("  {d}");
        }
        if !args.force && !args.replace_thornlist {
            println!("  (an explicit-source refetch would refuse without --replace-thornlist/-f)");
        }
    }
    if !configs.is_empty() {
        // Configs with no recorded source HEADs have no baseline to diff, so
        // they need one `-f` rebuild before a refetch can invalidate them.
        let (pre, tracked): (Vec<_>, Vec<_>) = configs
            .iter()
            .map(|(n, m)| (n.as_str(), m))
            .partition(|(_, m)| m.as_ref().is_some_and(|m| m.sources.is_none()));
        let names = |v: &[(&str, _)]| {
            v.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        };
        if !tracked.is_empty() {
            println!(
                "After a real refetch, `cactup build <name>` would recompile the new sources \
                 for config(s) {}.",
                names(&tracked)
            );
        }
        if !pre.is_empty() {
            println!(
                "Config(s) {} were built before source tracking and have no baseline to \
                 compare against — each needs `cactup build <name> -f` once, after which \
                 refetches are picked up automatically.",
                names(&pre)
            );
        }
    }
    let _ = inst;
}

/// Copy each dirty repo's modified files (per the probe) out of the
/// installation before fetching over them. Outside the installation because
/// `uninstall` deletes the installation directory.
fn backup_dirty(inst: &Installation, skipped: &[fetch::SkippedRepo]) -> Res<PathBuf> {
    let root = backup_dir(&inst.alias)?;
    for s in skipped {
        match &s.reason {
            fetch::git::DirtyReason::WorktreeModified(paths) => {
                for rel in paths {
                    let from = s.dir.join(rel);
                    if from.is_file() {
                        let to = root.join(&s.repo).join(rel);
                        if let Some(parent) = to.parent() {
                            fs::create_dir_all(parent)
                                .with_context(|| format!("Failed to create {}", parent.display()))?;
                        }
                        fs::copy(&from, &to)
                            .with_context(|| format!("Failed to back up {}", from.display()))?;
                    }
                }
            }
            // Local commits / detached HEADs stay recoverable through the
            // repo's own reflog (align force-moves refs with a reflog
            // entry); other reasons have no file contents to save.
            _ => {}
        }
    }
    Ok(root)
}

fn backup_dir(alias: &str) -> Res<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let dir = crate::CACTUP_ROOT.join("refetch-backups").join(alias).join(ts.to_string());
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    Ok(dir)
}

fn copy_tree(from: &Path, to: &Path) -> Res<()> {
    fs::create_dir_all(to).with_context(|| format!("Failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("Failed to list {}", from.display()))? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree(&src, &dst)?;
        } else if ty.is_symlink() {
            let target = fs::read_link(&src)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dst).ok();
        } else {
            fs::copy(&src, &dst).with_context(|| format!("Failed to copy {}", src.display()))?;
        }
    }
    Ok(())
}

/// Snapshot the live thornlist to `<root>/.cactup/thornlists/<ts>.th` before
/// an explicit source overwrites it.
fn snapshot_live(inst: &Installation, live: &Path) -> Res<()> {
    let Ok(bytes) = fs::read(live) else { return Ok(()) };
    let dir = inst.cactup_dir().join("thornlists");
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{ts}.th"));
    fs::write(&path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    println!("Snapshotted the previous thornlist to {}.", path.display());
    Ok(())
}

/// Join names for a one-line message, capping the tail. Mirrors
/// `build::summarize` (private to that module) — kept in sync by hand since
/// there is no shared, public helper to call instead.
fn summarize_names(names: &[String]) -> String {
    const SHOWN: usize = 8;
    let head = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    match names.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{head}, +{rest} more"),
        _ => head,
    }
}

/// §7.4: `rebuild_decision` now diffs the per-repo HEADs `fetch-state.toml`
/// records against the ones each config stored at its last build, so a plain
/// `cactup build` after a refetch does the right thing on its own. Report what
/// each config will do, and name the two cases that still need a flag.
fn report_configs(
    inst: &Installation,
    configs: &[(String, Option<crate::build::ConfigMeta>)],
    release_bump: bool,
) {
    // Best-effort provider map for the just-refetched live thornlist. This is
    // the RAW list — machine thorn toggles are applied at build time and no
    // machine is known here, so this is only an approximation; the build
    // itself makes the authoritative call via `provider_delta`. Any parse
    // failure silently disables the note below — a report must never fail
    // the refetch.
    let live_path = inst.live_thornlist();
    let fresh_providers = fs::read_to_string(&live_path)
        .ok()
        .and_then(|text| thornlist::parse_with_base(&text, live_path.parent()).ok())
        .map(|list| list.thorn_providers());

    println!("{}", "Existing configs pick the refetched sources up on their next build:".bold());
    for (name, meta) in configs {
        // No recorded HEADs = built before source tracking landed, so there is
        // no baseline to diff and the build would read as up to date. One `-f`
        // rebuild establishes the baseline; every refetch after that is
        // detected automatically.
        if meta.as_ref().is_some_and(|m| m.sources.is_none()) {
            println!(
                "  {} — {} built before source tracking, so it has no baseline to compare \
                 against; run `cactup build {name} -f` once and later refetches are picked \
                 up on their own.",
                name.bold(),
                "needs -f:".yellow()
            );
            continue;
        }
        let custom = meta.as_ref().is_some_and(|m| !inst.is_live_thornlist(&m.thornlist));
        if custom {
            println!(
                "  {} — `cactup build {name}` recompiles the refetched sources. Note its thorn \
                 *set* is unchanged: it builds from its own thornlist ({}), which this refetch \
                 did not touch — pass --thornlist to adopt the refetched list.",
                name.bold(),
                meta.as_ref().map(|m| m.thornlist.as_str()).unwrap_or("?")
            );
        } else {
            println!("  {} — run `cactup build {name}`.", name.bold());
            if let Some(m) = meta {
                let changed =
                    crate::build::provider_delta(m.thorn_providers.as_ref(), fresh_providers.as_ref());
                if !changed.is_empty() {
                    println!(
                        "      note: thorn name(s) {} now come from a different provider; \
                         `cactup build {name}` removes their stale per-thorn build state \
                         before compiling.",
                        summarize_names(&changed)
                    );
                }
            }
        }
    }
    if release_bump {
        println!(
            "  (A release change moves the Cactus flesh, which `cactup build` classifies as a \
             from-scratch rebuild by itself — no -f needed.)"
        );
    }
}
