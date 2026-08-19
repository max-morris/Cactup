//! `cactup installation refetch` — re-run the component fetch (spec §3.2):
//! update repos, adopt a new thornlist or release, add new thorns — never
//! clobbering hand-modified thorns unless forced.
//!
//! Locking: holds only the per-installation *fetch* lock
//! (`<installation home>/.cactup/.cactup-fetch.lock`, §2.3 item 6) with a
//! heartbeat, so a long fetch never blocks `sim create`/`config use` (which take the
//! installation lock). Nothing here may call `Installation::locked()` or
//! `ensure_meta` (non-reentrant); the only other lock taken is the global DB
//! lock, briefly, inside `ctx.db.update` at the very end.

use super::{prompt_with_default, Ctx};
use crate::args::RefetchArgs;
use crate::commands::installation::Tone;
use crate::database::{UnfetchedReason, UnfetchedRepo};
use crate::fetch::{self, link::LinkOutcome, GitAction};
use crate::installation::{validate_root_dir, Installation};
use crate::lock::LinkLock;
use crate::thornlist::{self, Thornlist};
use crate::{manifest, shell, Res};
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use indexmap::IndexMap;
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
    let overwrite_names = overwrite_selection(&args.overwrite);

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

    // A thornlist that omits !DEFINE ROOT (or names it ".") would otherwise
    // reach the mismatch check below as "." and get the misleading "source
    // tree cannot move" message; reject it with the honest reason first.
    // Same before-any-mutation / --dry-run-included placement as that check.
    validate_root_dir(list.root())?;

    // The root guard: `fetch::plan` derives the fetch root from `list.root()`
    // (§3.2), so a thornlist naming a different `!DEFINE ROOT` than the one
    // this installation was fetched into would fetch a second source tree
    // alongside the recorded one instead of updating it in place. Checked
    // before any mutation or fetching — including under --dry-run, which
    // must report this rather than silently planning to fetch into the wrong
    // directory.
    let recorded_root =
        inst.meta()?.root_dir.unwrap_or_else(|| crate::installation::DEFAULT_ROOT_DIR.to_owned());
    check_root_unchanged(list.root(), &recorded_root)?;

    // The in-place-edit guard (§3.2): an explicit source is about to replace
    // the live thornlist. If the live copy diverged from the pristine
    // as-fetched copy, that is a hand edit (e.g. a hand-added thorn) and we
    // refuse to destroy it silently. Compared as parsed component sets, not
    // text — generated headers differ on every pre-existing installation.
    // Both copies are read under whichever name they carry and written under
    // the current one: the guard must still see hand edits in a file left
    // under the pre-rename name (§3.2), or an installation the name migration
    // could not reach would have those edits silently overwritten.
    let live_path = inst.live_thornlist();
    let live_read = inst.live_thornlist_to_read();
    let pristine_path = inst.source_thornlist();
    let pristine_read = inst.source_thornlist_to_read();
    let divergence = if source.is_explicit() { live_divergence(&live_read, &pristine_read)? } else { Vec::new() };
    if !divergence.is_empty() && !replace_thornlist && !args.dry_run {
        println!(
            "{}",
            format!(
                "{} has hand edits not present in the pristine as-fetched copy ({}):",
                live_read.display(),
                pristine_read.display()
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

    // Resolve --overwrite against the plan before a word is printed about
    // it: the block below must know which of the skipped repos this very run
    // is going to fetch over, or it announces "local state preserved" for
    // repos it is about to overwrite. Resolving here also means `-n` catches
    // a typo'd name — a hard error, since silently ignoring it would mean
    // the flag did nothing and the user finds out only when the repo is
    // skipped anyway.
    let selected = if overwrite_names.is_empty() {
        BTreeSet::new()
    } else {
        let resolved = resolve_overwrite_selection(&plan, &overwrite_names)?;
        if overwrite_modified {
            // -f/--overwrite-modified already subsumes any --overwrite
            // selection; naming both is not a conflict, just redundant.
            println!(
                "{}",
                "--overwrite-modified (or -f) already forces every skipped repo; the \
                 --overwrite name(s) add nothing."
                    .yellow()
            );
        }
        resolved
    };
    // The broad flag wins: it forces everything, superseding any narrower
    // --overwrite selection.
    let forced_repo_names: BTreeSet<String> =
        if overwrite_modified { plan.skipped.iter().map(|s| s.repo.clone()).collect() } else { selected };

    // The skip/force block: before fetching, so it is seen up front, and the
    // same lines again in the summary. Under -s only the one-line counts
    // survive — silence the block, never the fact.
    print_skip_status(&plan.skipped, &forced_repo_names, overwrite_modified, &args, SkipPhase::BeforeFetch);

    // Orphans and config interactions are computed up front too — dry-run
    // prints all of it, and the real run needs them after the fetch anyway.
    let orphans = enumerate_orphans(&inst, &list, &plan)?;
    let configs = crate::commands::config::list_configs(&inst.cactus_root())?;

    if args.dry_run {
        print_dry_run(&inst, &plan, &orphans, &configs, &divergence, &args, &forced_repo_names);
        return Ok(());
    }

    // Back the dirty repos actually being forced up outside the installation
    // (uninstall deletes the installation), then fetch them like clean repos.
    if !forced_repo_names.is_empty() {
        let to_force: Vec<&fetch::SkippedRepo> =
            plan.skipped.iter().filter(|s| forced_repo_names.contains(&s.repo)).collect();
        if let Some(backup_root) = backup_dirty(&inst, &to_force)? {
            println!(
                "Backed up locally-modified files to {} — restore by copying them back.",
                backup_root.display().to_string().bold()
            );
        }
        let moved = if overwrite_modified {
            plan.force()
        } else {
            plan.force_where(|s| forced_repo_names.contains(&s.repo))
        };
        // "local modifications" would be wrong for a repo forced for a
        // changed remote URL, a detached HEAD, or local commits — none of
        // which are modified files. "skipped" would be wrong too: these are
        // the repos this run is *not* skipping.
        println!("Overwriting local state in {} repo(s): {}.", moved.len(), moved.join(", ").bold());
    }

    // The symlink pass runs for every git component regardless of whether its
    // repo was skipped (it targets the checkout, not the repo dir), and it
    // can fail on its own (a blocked/foreign path becomes a `Failure`) — so
    // it is part of the fetch's effect on disk and counts as work. Without
    // this, "every git repo skipped AND one symlink errored" left both
    // `fetch_ok` and `had_work` false, so the whole block below silently
    // skipped writing the thornlist or updating the DB.
    let had_work = !plan.git.is_empty()
        || !plan.downloads.is_empty()
        || !plan.external.is_empty()
        || !plan.links.is_empty();
    let report = fetch::execute(&plan, &inst.root)?;

    // Post-pass bookkeeping: record what we fetched (prune safety + the
    // future rebuild-decision hookup), then report.
    if !report.repos.is_empty() {
        fetch::FetchState::record(&inst.root, &report.records())?;
    }
    // Ctrl-C during the fetch: what completed is recorded above; everything
    // else (the summary, thornlist adoption, config reports) belongs to a
    // finished refetch, not an aborted one.
    if gix::interrupt::is_triggered() {
        bail!(
            "interrupted after fetching {} repo(s) and {} download(s); the remaining \
             component(s) were not fetched",
            report.repos.len(),
            report.downloads.len()
        );
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
    // Unconditional, not gated on --verbose: rewriting a user's `origin` is a
    // persistent mutation of their repo, not routine fetch chatter.
    let repointed: Vec<(&str, &str)> = report
        .repos
        .iter()
        .filter_map(|r| r.retargeted.as_deref().map(|url| (r.repo.as_str(), url)))
        .collect();
    if !repointed.is_empty() {
        println!("{}", format!("{} repo(s) had their origin re-pointed:", repointed.len()).bold());
        for (repo, url) in &repointed {
            println!("  {repo}: origin now points at {}", url.bold());
        }
    }
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
    // Forced repos left `plan.skipped` when `force`/`force_where` moved them
    // into the fetchable set above, so this reports only the repos this run
    // genuinely left alone.
    print_skip_status(&plan.skipped, &forced_repo_names, overwrite_modified, &args, SkipPhase::Summary);

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
    let mut unfetched: IndexMap<String, UnfetchedRepo> = IndexMap::new();
    if fetch_ok || had_work {
        if source.is_explicit() {
            snapshot_live(&inst, &live_read)?;
            fs::write(&live_path, source.bytes())
                .with_context(|| format!("Failed to write {}", live_path.display()))?;
            // Writing the current name beside a pre-rename copy would leave two
            // thornlists where reads only ever consult one — exactly the
            // ambiguity the rename removes. The old copy was just snapshotted,
            // so dropping it loses nothing; best-effort, since failing to
            // delete it is not worth failing a completed refetch over.
            if live_read != live_path {
                let _ = fs::remove_file(&live_read);
            }
            // The pristine as-fetched baseline tracks the last *adopted
            // official source* only. A no-argument refetch must NOT touch it
            // — hand edits in the live file stay visible as divergence, so
            // the guard above keeps protecting them on the next explicit
            // refetch even after they have been fetched once.
            fs::write(&pristine_path, source.bytes())
                .with_context(|| format!("Failed to write {}", pristine_path.display()))?;
            if pristine_read != pristine_path {
                let _ = fs::remove_file(&pristine_read);
            }
        }

        // DB provenance (§2.1): the thornlist is adopted on disk above
        // regardless of whether every repo behind it was actually fetched
        // (a dirty repo is skipped, not blocked on), so the DB now records
        // that adoption unconditionally too — asserting anything less would
        // just be a second, independently-stale copy of what's on disk.
        // What it also records, separately, is which repos were left
        // unfetched (skipped as dirty, or failed) and the thorns they back:
        // `unfetched_repos`, non-empty exactly when the tree only
        // *partially* conforms to the thornlist just recorded. Any refetch
        // that fetches every repo the list names — e.g. `refetch -f`, which
        // drains `plan.skipped` via `Plan::force` before we get here —
        // naturally computes an empty map and clears the marker.
        unfetched = unfetched_repos(&plan, &report);
        let explicit = match &source {
            Source::Release { tag, .. } => Some((Some(tag.clone()), None::<String>)),
            Source::File { path, .. } => Some((None::<String>, Some(path.display().to_string()))),
            Source::Live { .. } => None,
        };
        let alias = inst.alias.clone();
        let unfetched_for_db = unfetched.clone();
        let was_partial = ctx.db.update(move |db| {
            let entry = db
                .installations
                .get_mut(&alias)
                .ok_or_else(|| anyhow!("installation \"{alias}\" vanished from the database"))?;
            let was_partial = !entry.unfetched_repos.is_empty();
            if let Some((current_release, current_thornlist)) = explicit {
                entry.current_release = current_release;
                entry.current_thornlist = current_thornlist;
            }
            entry.unfetched_repos = unfetched_for_db;
            Ok(was_partial)
        })?;
        recorded = source.is_explicit();

        let clears_partial = was_partial && unfetched.is_empty();
        match &source {
            Source::Release { tag, .. } => println!(
                "This installation is now on {}.{}",
                tag.bold(),
                if clears_partial { " (this clears the previous partial-adoption warning)" } else { "" }
            ),
            Source::File { path, .. } => println!(
                "This installation now tracks the custom thornlist {}.{}",
                path.display().to_string().bold(),
                if clears_partial { " (this clears the previous partial-adoption warning)" } else { "" }
            ),
            Source::Live { .. } if clears_partial => println!(
                "This installation is now fully in sync with the live thornlist."
            ),
            Source::Live { .. } => {}
        }
        if !unfetched.is_empty() {
            let thorn_count = unfetched.values().map(|r| r.thorns.len()).sum::<usize>();
            let header_line = if source.is_explicit() {
                format!(
                    "Partial adoption: the thornlist above is now recorded, but {} repo(s) \
                     were not fetched, so {thorn_count} thorn(s) on disk still hold their \
                     previous contents.",
                    unfetched.len()
                )
            } else {
                format!(
                    "{} repo(s) were not fetched, so {thorn_count} thorn(s) on disk do not \
                     match the live thornlist.",
                    unfetched.len()
                )
            };
            print_partial_adoption(&header_line, &unfetched);
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
            "The recorded thornlist {}.",
            if recorded {
                if unfetched.is_empty() {
                    "WAS updated".to_owned()
                } else {
                    "WAS updated (see the partial-adoption note above for what these \
                     failures leave unfetched)"
                        .to_owned()
                }
            } else {
                "did not need to be updated".to_owned()
            }
        );
        bail!("refetch completed with {} failure(s)", report.failures.len());
    }

    Ok(())
}

/// A thornlist's `!DEFINE ROOT` may never differ from the `root-dir` this
/// installation was fetched into: an installation's source tree cannot move,
/// so a mismatch here is always a hard error, with a fresh install as the
/// remedy.
fn check_root_unchanged(list_root: &str, recorded: &str) -> Res<()> {
    if list_root != recorded {
        bail!(
            "this thornlist's !DEFINE ROOT is \"{list_root}\", but this installation's source \
             tree lives at \"{recorded}\"; an installation's source tree cannot move — install \
             the new thornlist as a fresh installation instead"
        );
    }
    Ok(())
}

/// §3.2 source precedence: `--release TAG` → positional THORNLIST → the
/// live `Cactus/thornlists/installation-default.th` (falling back to the
/// pristine root copy on very old trees).
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
    let live = inst.live_thornlist_to_read();
    let path = if live.exists() { live } else { inst.source_thornlist_to_read() };
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

/// Split every raw `--overwrite` value on whitespace or commas (so
/// `"SpacetimeX Cottonmouth"` and two separate `--overwrite` occurrences mean
/// the same thing), drop empty tokens, and dedupe while keeping the order
/// names were first seen in.
fn overwrite_selection(raw: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for entry in raw {
        for name in entry.split([' ', ',', '\t']).map(str::trim).filter(|s| !s.is_empty()) {
            if seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Does `name` (a `--overwrite` argument) pick out this repo? Matches the
/// repo name itself, one of its checkouts in full (`SpacetimeX/WeylScal4`),
/// or just a checkout's bare thorn name (`WeylScal4`) — all case-insensitive,
/// since these are proper nouns a user is typing from memory.
fn matches_name(repo: &str, checkouts: &[String], name: &str) -> bool {
    repo.eq_ignore_ascii_case(name)
        || checkouts.iter().any(|c| {
            c.eq_ignore_ascii_case(name)
                || c.rsplit('/').next().is_some_and(|thorn| thorn.eq_ignore_ascii_case(name))
        })
}

/// Resolve `--overwrite` names against the plan. A name matching a skipped
/// repo goes into the returned set (to be forced); a name matching only a
/// repo the plan is already going to fetch cleanly is reported (nothing to
/// overwrite there) but is not an error; a name matching neither is a typo,
/// and *all* such names are collected into a single hard error — checked
/// before anything is fetched (including under `--dry-run`) so a typo is
/// never discovered only after the rest of the fetch already ran.
fn resolve_overwrite_selection(plan: &fetch::Plan, names: &[String]) -> Res<BTreeSet<String>> {
    let mut forced = BTreeSet::new();
    let mut unmatched = Vec::new();
    for name in names {
        let hit_skipped: Vec<&str> = plan
            .skipped
            .iter()
            .filter(|s| matches_name(&s.repo, &s.checkouts, name))
            .map(|s| s.repo.as_str())
            .collect();
        if !hit_skipped.is_empty() {
            forced.extend(hit_skipped.into_iter().map(str::to_string));
            continue;
        }
        let hits_fetchable = plan.git.iter().any(|g| matches_name(&g.repo, &g.checkouts, name));
        // Downloads and svn/cvs/hg/darcs components are in the thornlist but
        // have no worktree to preserve, so they are never skipped and never
        // need overwriting — naming one is pointless, not a typo, and must
        // not be reported as "not found in this thornlist".
        let hits_other = plan
            .downloads
            .iter()
            .chain(&plan.external)
            .any(|c| matches_name("", std::slice::from_ref(&c.checkout), name));
        if hits_fetchable || hits_other {
            println!("{}", format!("nothing to overwrite for {name}: it is not skipped.").yellow());
        } else {
            unmatched.push(name.clone());
        }
    }
    if !unmatched.is_empty() {
        let available = if plan.skipped.is_empty() {
            "none".to_string()
        } else {
            plan.skipped.iter().map(|s| s.repo.as_str()).collect::<Vec<_>>().join(", ")
        };
        bail!(
            "--overwrite name(s) not found in this thornlist: {} (skipped repos available to \
             overwrite: {available})",
            unmatched.join(", ")
        );
    }
    Ok(forced)
}

/// Which of the two printings of the block this is: the one before the fetch
/// (what is about to happen) or the one in the closing summary (what did).
#[derive(Clone, Copy)]
enum SkipPhase {
    BeforeFetch,
    Summary,
}

/// The skip/force block.
///
/// `skipped` is the *probe's* classification, not this run's verdict: the
/// repos named in `forced` were classified dirty but the flags say to fetch
/// over them anyway. Announcing those as "skipped (local state preserved)"
/// and only later admitting they will be overwritten is the contradiction
/// this split exists to prevent — a dirty repo appears under exactly one
/// headline, and the one it appears under is what actually happens to it.
fn skip_status_lines(
    skipped: &[fetch::SkippedRepo],
    forced: &BTreeSet<String>,
    overwrite_modified: bool,
    silent: bool,
    dry_run: bool,
    phase: SkipPhase,
) -> Vec<(Tone, String)> {
    let (forced_repos, kept): (Vec<&fetch::SkippedRepo>, Vec<&fetch::SkippedRepo>) =
        skipped.iter().partition(|s| forced.contains(&s.repo));
    let mut lines = Vec::new();

    let detail = |lines: &mut Vec<(Tone, String)>, s: &fetch::SkippedRepo| {
        lines.push((Tone::Plain, format!("  {} — {}", s.repo.bold(), s.reason.describe())));
        lines.push((Tone::Plain, format!("    thorns: {}", s.checkouts.join(", "))));
        if !s.untracked.is_empty() {
            lines.push((
                Tone::Plain,
                format!("    ({} untracked file(s), which never block a fetch)", s.untracked.len()),
            ));
        }
    };

    // Alarm, not Warn: this is the destructive half, and it must not read as
    // a milder variant of the "preserved" headline below it. In the summary
    // the forced repos have already left `skipped` (and are reported as
    // fetched), so this group only ever fires in the pre-fetch printing.
    if !forced_repos.is_empty() {
        let verb = if dry_run { "would be fetched over" } else { "will be fetched over" };
        let flag = if overwrite_modified { "--overwrite-modified / -f" } else { "--overwrite" };
        lines.push((
            Tone::Alarm,
            format!(
                "{} repo(s) with local state {verb} anyway ({flag}) — local state NOT preserved.",
                forced_repos.len()
            ),
        ));
        if !silent {
            for s in &forced_repos {
                detail(&mut lines, s);
            }
            lines.push((
                Tone::Plain,
                format!(
                    "  Modified files are copied to {} first; local commits and detached \
                     HEADs stay in the repo's reflog.",
                    "~/.cactup/refetch-backups/<alias>/".bold()
                ),
            ));
        }
    }

    if !kept.is_empty() {
        let verb = match (phase, dry_run) {
            (SkipPhase::BeforeFetch, true) => "would be skipped",
            (SkipPhase::BeforeFetch, false) => "will be skipped",
            (SkipPhase::Summary, _) => "skipped",
        };
        lines.push((Tone::Warn, format!("{} repo(s) {verb} (local state preserved).", kept.len())));
        if !silent {
            for s in &kept {
                detail(&mut lines, s);
            }
            // --overwrite-modified forces every skipped repo, so a non-empty
            // `kept` means it was not passed: the remedy below always applies
            // to the repos just listed, and `kept[0]` is a real example name.
            lines.push((
                Tone::Plain,
                format!(
                    "  Pass {} to fetch over all of these anyway (modified files are backed up first).",
                    "--overwrite-modified / -f".bold()
                ),
            ));
            lines.push((
                Tone::Plain,
                format!(
                    "  Or {} to overwrite just one or a few (e.g. {}), or {} to hide this list.",
                    "--overwrite <name>".bold(),
                    format!("--overwrite {}", kept[0].repo).bold(),
                    "-s/--silent".bold()
                ),
            ));
        }
    }
    lines
}

fn print_skip_status(
    skipped: &[fetch::SkippedRepo],
    forced: &BTreeSet<String>,
    overwrite_modified: bool,
    args: &RefetchArgs,
    phase: SkipPhase,
) {
    for (tone, line) in
        skip_status_lines(skipped, forced, overwrite_modified, args.silent, args.dry_run, phase)
    {
        match tone {
            Tone::Alarm => println!("{}", line.bright_red().bold()),
            Tone::Warn => println!("{}", line.yellow().bold()),
            Tone::Plain => println!("{line}"),
        }
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
        if path.is_dir() && !path.is_symlink() && let Ok(inner) = fs::read_dir(&path) {
            candidates.extend(inner.flatten().map(|e| e.path()));
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
    forced_repo_names: &BTreeSet<String>,
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
        // A forced repo is not a skip. Tagging one "SKIP:" and appending
        // "would be fetched" to the same line is the contradiction the
        // split above removes, so the label itself carries the verdict.
        if forced_repo_names.contains(&s.repo) {
            println!(
                "  {} — {} fetch over local state ({})",
                s.repo.bold(),
                "FORCE:".bright_red(),
                s.reason.describe()
            );
        } else {
            println!("  {} — {} {}", s.repo.bold(), "SKIP:".yellow(), s.reason.describe());
        }
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

/// Copy the modified files (per the probe) of only the repos about to be
/// forced out of the installation, before fetching over them. Both
/// `WorktreeModified` and `RemoteUrlChanged` carry file contents worth
/// saving here — a retargeted repo can also be locally edited (the
/// fork-adoption workflow is exactly "edit a thorn, then repoint origin at
/// your fork"), and forcing replaces the whole worktree either way. Outside
/// the installation because `uninstall` deletes the installation directory.
/// `Ok(None)` when none of `skipped` actually has file contents to save (e.g.
/// a --overwrite selection that only hit local-commit/detached-HEAD repos) —
/// callers must not report a backup path, or create a backup dir, in that case.
fn backup_dirty(inst: &Installation, skipped: &[&fetch::SkippedRepo]) -> Res<Option<PathBuf>> {
    // Local commits / detached HEADs stay recoverable through the repo's own
    // reflog (align force-moves refs with a reflog entry); only
    // WorktreeModified and RemoteUrlChanged have file contents worth copying
    // out.
    let files: Vec<(&fetch::SkippedRepo, &String)> = skipped
        .iter()
        .filter_map(|s| match &s.reason {
            fetch::git::DirtyReason::WorktreeModified(paths) => Some((*s, paths)),
            fetch::git::DirtyReason::RemoteUrlChanged { modified, .. } => Some((*s, modified)),
            _ => None,
        })
        .flat_map(|(s, paths)| paths.iter().map(move |rel| (s, rel)))
        .filter(|(s, rel)| s.dir.join(rel).is_file())
        .collect();
    if files.is_empty() {
        return Ok(None);
    }

    let root = backup_dir(&inst.alias)?;
    for (s, rel) in files {
        let from = s.dir.join(rel);
        let to = root.join(&s.repo).join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::copy(&from, &to).with_context(|| format!("Failed to back up {}", from.display()))?;
    }
    Ok(Some(root))
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

/// Join names for a one-line message, capping the tail at `shown`. Mirrors
/// `build::summarize` (private to that module) — kept in sync by hand since
/// there is no shared, public helper to call instead.
fn summarize_names(names: &[String], shown: usize) -> String {
    let head = names.iter().take(shown).cloned().collect::<Vec<_>>().join(", ");
    match names.len().checked_sub(shown) {
        Some(rest) if rest > 0 => format!("{head}, +{rest} more"),
        _ => head,
    }
}

/// The repos an `installation refetch` did not fetch — skipped as dirty, or
/// failed — mapped to why and the thorns they back (§2.1's
/// `unfetched_repos`). A skip is a supported workflow (the user may have
/// local work there); a failure is an error the user asked to avoid and did
/// not, so failures are inserted last and unconditionally: `plan.skipped`
/// and `report.failures` cannot name the same repo in practice (a skipped
/// repo never reaches `execute`), but this order — skips via `entry`/
/// `or_insert`, failures via a plain `insert` — is used anyway so a future
/// change to that invariant fails safe: the error, the more important fact,
/// wins rather than being silently shadowed by a stale skip entry.
fn unfetched_repos(plan: &fetch::Plan, report: &fetch::ExecReport) -> IndexMap<String, UnfetchedRepo> {
    let mut out: IndexMap<String, UnfetchedRepo> = IndexMap::new();
    for s in &plan.skipped {
        out.entry(s.repo.clone()).or_insert_with(|| UnfetchedRepo {
            reason: UnfetchedReason::Skipped,
            thorns: s.checkouts.clone(),
            detail: Some(s.reason.describe()),
        });
    }
    for f in &report.failures {
        // A failed git repo is named by `f.what == item.repo` (both the clone
        // /fetch failure and the post-fetch HEAD-mismatch check), and backs
        // every thorn in that repo's plan entry. A failed download,
        // external-tool, or symlink component is named by its own checkout
        // instead, and backs no thorn but itself — so that checkout name is
        // also its own fallback "thorn" (otherwise the repo count and thorn
        // count would disagree: one failed download would read as "1 repo(s)
        // ... so 0 thorn(s)").
        let thorns = plan
            .git
            .iter()
            .find(|g| g.repo == f.what)
            .map(|g| g.checkouts.clone())
            .unwrap_or_else(|| vec![f.what.clone()]);
        out.insert(
            f.what.clone(),
            UnfetchedRepo { reason: UnfetchedReason::Failed, thorns, detail: Some(f.error.clone()) },
        );
    }
    out
}

/// The "partial adoption" block printed after a refetch that leaves some
/// repos unfetched: `header_line` names the count/cause (adoption vs.
/// live-thornlist mismatch — the two callers word it differently). This runs
/// right after the detailed skip/failure blocks above it, so it stays tight —
/// what this means for provenance, not a re-listing of detail — just the
/// repo names, grouped by whether each is an error (angrier: bright red,
/// bold) or a supported choice (yellow), and how to act on each.
fn print_partial_adoption(header_line: &str, unfetched: &IndexMap<String, UnfetchedRepo>) {
    const NAMES_SHOWN: usize = 8;
    println!("{}", header_line.yellow().bold());

    let failed: Vec<String> = unfetched
        .iter()
        .filter(|(_, r)| r.reason == UnfetchedReason::Failed)
        .map(|(repo, _)| repo.clone())
        .collect();
    let skipped: Vec<String> = unfetched
        .iter()
        .filter(|(_, r)| r.reason == UnfetchedReason::Skipped)
        .map(|(repo, _)| repo.clone())
        .collect();

    if !failed.is_empty() {
        println!(
            "{}",
            format!(
                "  FAILED: {} — an error; retry with `cactup inst refetch`",
                summarize_names(&failed, NAMES_SHOWN)
            )
            .bright_red()
            .bold()
        );
    }
    if !skipped.is_empty() {
        println!(
            "{}",
            format!(
                "  skipped: {} — local state preserved; `cactup inst refetch -f` fetches over them",
                summarize_names(&skipped, NAMES_SHOWN)
            )
            .yellow()
        );
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
    let live_path = inst.live_thornlist_to_read();
    let live_list = fs::read_to_string(&live_path)
        .ok()
        .and_then(|text| thornlist::parse_with_base(&text, live_path.parent()).ok());
    let fresh_providers = live_list.as_ref().map(|list| list.thorn_providers());
    // The per-thorn shape fingerprints, read off the tree the fetch just
    // left behind. Same best-effort caveat as the providers above, plus one
    // of its own: this walks every thorn (~0.2 s on the real list), which is
    // nothing next to the fetch that just ran.
    let fresh_shapes =
        live_list.as_ref().map(|list| crate::build::thorn_shapes(&inst.cactus_root(), list));

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
                        summarize_names(&changed, 8)
                    );
                }
                // The sibling case: same provider, but the fetched content
                // changed what that thorn compiles (a `.ccl` edit, a source
                // file added or removed). Reported separately from the
                // provider swap above, because the two are diagnosed
                // differently even though the remedy is identical.
                let reshaped = crate::build::shape_delta(m.thorn_shapes.as_ref(), fresh_shapes.as_ref());
                let reshaped: Vec<String> =
                    reshaped.into_iter().filter(|t| !changed.contains(t)).collect();
                if !reshaped.is_empty() {
                    println!(
                        "      note: thorn(s) {} changed shape (files added/removed, or a \
                         .ccl/make.code.defn edited); `cactup build {name}` removes their \
                         stale per-thorn build state before compiling.",
                        summarize_names(&reshaped, 8)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overwrite_selection_splits_on_whitespace_and_commas_and_dedupes() {
        assert_eq!(
            overwrite_selection(&["SpacetimeX Cottonmouth".to_string()]),
            vec!["SpacetimeX".to_string(), "Cottonmouth".to_string()]
        );
        assert_eq!(
            overwrite_selection(&["SpacetimeX".to_string(), "Cottonmouth".to_string()]),
            vec!["SpacetimeX".to_string(), "Cottonmouth".to_string()]
        );
        // Commas, repeated separators, and stray whitespace are all accepted.
        assert_eq!(
            overwrite_selection(&["A, B ,,C".to_string()]),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        // A name repeated across occurrences collapses to one, in first-seen order.
        assert_eq!(
            overwrite_selection(&["A B".to_string(), "B A".to_string()]),
            vec!["A".to_string(), "B".to_string()]
        );
        assert!(overwrite_selection(&[]).is_empty());
        assert!(overwrite_selection(&["   ".to_string()]).is_empty());
    }

    #[test]
    fn matches_name_checks_repo_full_checkout_and_bare_thorn_case_insensitively() {
        let checkouts = vec!["SpacetimeX/WeylScal4".to_string(), "SpacetimeX/NewRad".to_string()];
        assert!(matches_name("SpacetimeX", &checkouts, "spacetimex"), "repo name, case-insensitive");
        assert!(matches_name("SpacetimeX", &checkouts, "SpacetimeX/WeylScal4"), "full checkout string");
        assert!(matches_name("SpacetimeX", &checkouts, "weylscal4"), "bare thorn name, case-insensitive");
        assert!(!matches_name("SpacetimeX", &checkouts, "Cottonmouth"), "no match");
    }

    fn skipped(repo: &str, checkouts: &[&str]) -> fetch::SkippedRepo {
        fetch::SkippedRepo {
            repo: repo.to_string(),
            dir: PathBuf::from(format!("/nonexistent/{repo}")),
            url: "https://example.invalid/repo.git".to_string(),
            branch: Some("main".to_string()),
            reason: fetch::git::DirtyReason::WorktreeModified(vec!["src/foo.c".to_string()]),
            untracked: Vec::new(),
            checkouts: checkouts.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn fetchable_repo(repo: &str, checkouts: &[&str]) -> fetch::GitRepoPlan {
        fetch::GitRepoPlan {
            repo: repo.to_string(),
            dir: PathBuf::from(format!("/nonexistent/{repo}")),
            url: "https://example.invalid/repo.git".to_string(),
            branch: Some("main".to_string()),
            action: GitAction::Update,
            checkouts: checkouts.iter().map(|s| s.to_string()).collect(),
            forced: None,
            retarget: None,
        }
    }

    fn plan_with(skipped_repos: Vec<fetch::SkippedRepo>, fetchable_repos: Vec<fetch::GitRepoPlan>) -> fetch::Plan {
        fetch::Plan {
            git: fetchable_repos,
            skipped: skipped_repos,
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        }
    }

    #[test]
    fn resolve_overwrite_selection_forces_a_skipped_repo_matched_by_bare_thorn_name() {
        let plan = plan_with(
            vec![skipped("SpacetimeX", &["SpacetimeX/WeylScal4"])],
            vec![fetchable_repo("Cottonmouth", &["Cottonmouth/Foo"])],
        );
        let forced = resolve_overwrite_selection(&plan, &["WeylScal4".to_string()]).unwrap();
        assert_eq!(forced, BTreeSet::from(["SpacetimeX".to_string()]));
    }

    #[test]
    fn resolve_overwrite_selection_reports_but_does_not_error_on_an_already_fetchable_repo() {
        let plan = plan_with(
            vec![skipped("SpacetimeX", &["SpacetimeX/WeylScal4"])],
            vec![fetchable_repo("Cottonmouth", &["Cottonmouth/Foo"])],
        );
        // Cottonmouth is clean and already planned to fetch — nothing to
        // overwrite there, but that is not a typo, so no error.
        let forced = resolve_overwrite_selection(&plan, &["Cottonmouth".to_string()]).unwrap();
        assert!(forced.is_empty());
    }

    /// Every line of the block, joined — for "this name is never mentioned"
    /// assertions, which have to look at the whole block, not one line.
    fn joined(lines: &[(Tone, String)]) -> String {
        lines.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Index of the one line at `tone`; panics if there is not exactly one.
    fn only_at(lines: &[(Tone, String)], tone: Tone) -> usize {
        let mut hits = lines.iter().enumerate().filter(|(_, (t, _))| *t == tone);
        let (i, _) = hits.next().unwrap_or_else(|| panic!("no {tone:?} line in {:?}", joined(lines)));
        assert!(hits.next().is_none(), "more than one {tone:?} line in {:?}", joined(lines));
        i
    }

    #[test]
    fn skip_status_lines_never_promises_preserved_state_for_a_forced_repo() {
        let repos = vec![
            skipped("SpacetimeX", &["SpacetimeX/WeylScal4"]),
            skipped("Cottonmouth", &["Cottonmouth/Foo"]),
        ];
        let forced = BTreeSet::from(["SpacetimeX".to_string()]);
        let lines = skip_status_lines(&repos, &forced, false, false, true, SkipPhase::BeforeFetch);

        // Two headlines, each counting only its own half: the forced repo is
        // never included in the "local state preserved" promise.
        let alarm = only_at(&lines, Tone::Alarm);
        let warn = only_at(&lines, Tone::Warn);
        assert!(lines[alarm].1.contains("1 repo(s)"), "{}", lines[alarm].1);
        assert!(lines[alarm].1.contains("NOT preserved"), "{}", lines[alarm].1);
        assert!(lines[warn].1.contains("1 repo(s) would be skipped (local state preserved)"), "{}", lines[warn].1);

        // And each repo is listed under exactly the headline that tells the
        // truth about it: the forced one above, the kept one below.
        let at = |name: &str| lines.iter().position(|(_, l)| l.contains(name)).unwrap();
        assert!(alarm < at("SpacetimeX") && at("SpacetimeX") < warn, "{:?}", joined(&lines));
        assert!(warn < at("Cottonmouth"), "{:?}", joined(&lines));
    }

    #[test]
    fn skip_status_lines_under_overwrite_modified_report_no_skips_at_all() {
        let repos = vec![
            skipped("SpacetimeX", &["SpacetimeX/WeylScal4"]),
            skipped("Cottonmouth", &["Cottonmouth/Foo"]),
        ];
        let forced: BTreeSet<String> = repos.iter().map(|s| s.repo.clone()).collect();
        let lines = skip_status_lines(&repos, &forced, true, false, true, SkipPhase::BeforeFetch);

        let alarm = only_at(&lines, Tone::Alarm);
        assert!(lines[alarm].1.contains("2 repo(s)"), "{}", lines[alarm].1);
        assert!(lines[alarm].1.contains("--overwrite-modified / -f"), "{}", lines[alarm].1);
        // -n phrases it as a hypothetical; nothing claims a skip, and the
        // "pass -f to fetch over these" remedy would be nonsense here.
        assert!(lines[alarm].1.contains("would be fetched over"), "{}", lines[alarm].1);
        assert!(!lines.iter().any(|(t, _)| *t == Tone::Warn), "{:?}", joined(&lines));
        assert!(!joined(&lines).contains("(local state preserved)"), "{:?}", joined(&lines));
        assert!(!joined(&lines).contains("Pass"), "{:?}", joined(&lines));
    }

    #[test]
    fn skip_status_lines_in_the_summary_report_the_kept_repos_in_the_past_tense() {
        let repos = vec![skipped("SpacetimeX", &["SpacetimeX/WeylScal4"])];
        let lines = skip_status_lines(&repos, &BTreeSet::new(), false, false, false, SkipPhase::Summary);

        let warn = only_at(&lines, Tone::Warn);
        assert_eq!(lines[warn].1, "1 repo(s) skipped (local state preserved).");
        // The remedy names a repo that really is still skipped.
        assert!(joined(&lines).contains("--overwrite SpacetimeX"), "{:?}", joined(&lines));
    }

    #[test]
    fn skip_status_lines_under_silent_keep_the_counts_and_drop_the_detail() {
        let repos = vec![
            skipped("SpacetimeX", &["SpacetimeX/WeylScal4"]),
            skipped("Cottonmouth", &["Cottonmouth/Foo"]),
        ];
        let forced = BTreeSet::from(["SpacetimeX".to_string()]);
        let lines = skip_status_lines(&repos, &forced, false, true, false, SkipPhase::BeforeFetch);

        assert_eq!(lines.len(), 2, "{:?}", joined(&lines));
        assert_eq!(lines[0].0, Tone::Alarm);
        assert_eq!(lines[1].0, Tone::Warn);
        assert!(!joined(&lines).contains("thorns:"), "{:?}", joined(&lines));
    }

    #[test]
    fn resolve_overwrite_selection_errors_on_a_name_matching_nothing_in_the_plan() {
        let plan = plan_with(vec![skipped("SpacetimeX", &["SpacetimeX/WeylScal4"])], vec![]);
        let err = resolve_overwrite_selection(&plan, &["Typo".to_string()]).unwrap_err();
        // The error names the typo and lists what is actually available.
        assert!(err.to_string().contains("Typo"));
        assert!(err.to_string().contains("SpacetimeX"));
    }

    #[test]
    fn check_root_unchanged_passes_when_the_roots_are_equal() {
        assert!(check_root_unchanged("Cactus", "Cactus").is_ok());
        assert!(check_root_unchanged("MyTree", "MyTree").is_ok());
    }

    #[test]
    fn check_root_unchanged_fails_and_names_both_roots_when_they_differ() {
        let err = check_root_unchanged("MyTree", "Cactus").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MyTree"), "{msg}");
        assert!(msg.contains("Cactus"), "{msg}");
        assert!(msg.contains("cannot move"), "{msg}");
    }
}
