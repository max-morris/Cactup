//! `cactup sim export` (spec §8.9): a simulation as a `.sim` file, the
//! Cactup ecosystem's portable record of a parfile plus the exact sources and
//! build it ran on (format: `cactup_ecosystem_common::sim`).
//!
//! Everything is read from what the simulation froze at create (§8.2 step 6)
//! — the config record with its per-repo commits, the source thornlist, the
//! parfile — never from the installation as it is now, which may have moved
//! on since the binary was built. The thornlist and parfile go in verbatim;
//! the pins go in a per-repo table beside them.

use super::Simulation;
use crate::args::{SimDescribeArgs, SimExportArgs, SimPackArgs};
use crate::build::{self, ConfigMeta, OptionlistSource};
use crate::commands::Ctx;
use crate::database::KNOWN_KNOBS;
use crate::fetch::{self, FetchState};
use crate::installation::Installation;
use crate::template::{Syntax, VarSet};
use crate::thornlist::{self, ComponentType};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::sim::{
    Input, Parfile, ParfileKind, Provenance, RepoPin, RunHints, SimFile, SimMeta, ThornlistText,
};
use colored::Colorize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn export(ctx: &Ctx, args: SimExportArgs) -> Res<()> {
    export_from(ctx, &Installation::resolve(ctx)?, args)
}

pub fn export_from(ctx: &Ctx, inst: &Installation, args: SimExportArgs) -> Res<()> {
    let sim = Simulation::locate(inst, &args.sim)?;
    let file = args.file.clone().unwrap_or_else(|| PathBuf::from(format!("{}.sim", sim.name)));
    refuse_overwrite(&file, &args.describe)?;

    let config = frozen_config(inst, &sim)?;
    let cfg_dir = sim.cactup_dir().join("cfg");
    let source_text = std::fs::read_to_string(cfg_dir.join(build::THORNLIST_SNAPSHOT)).with_context(|| {
        format!("Simulation \"{}\" has no copy of the thornlist it was built from", sim.name)
    })?;
    let list = thornlist::parse(&source_text)
        .context("Export needs the thornlist as a GetComponents (CRL) list, with any !INCLUDE spliced in")?;

    // The pins, and the checks that decide whether anyone else could fetch them.
    let fetched = FetchState::read(&inst.root)?;
    let repos = repo_pins(&list, |repo| config.sources.as_ref().and_then(|s| s.get(repo)).cloned());
    let checks = check_repos(&sim.meta.sourcedir.join("repos"), &repos, fetched.as_ref())?;
    refuse_unfetchable(&repos, &checks, args.allow_dirty, args.allow_unpushed, "when the binary was built")?;

    let parfile = parfile_at(&sim.cactup_dir().join("par").join(&sim.meta.parfile))?;
    let processed = std::fs::read_to_string(cfg_dir.join(build::THORNLIST_PROCESSED)).unwrap_or_default();
    let provenance = provenance(&sim, &config, &list, &processed);
    let fallback_summary = format!("{} on {}", super::parfile_stem(&parfile.filename), sim.meta.machine);
    finish(
        ctx,
        &args.describe,
        &file,
        Parts {
            default_name: sim.name.clone(),
            fallback_summary,
            root: list.root().to_owned(),
            thornlist: ThornlistText { filename: thornlist_filename(&config.thornlist), text: source_text, untracked: Vec::new() },
            parfile,
            repos,
            run: run_hints(&sim),
            provenance,
        },
    )
}

/// What a `.sim` is made of before its descriptive fields are settled.
struct Parts {
    default_name: String,
    /// The summary when the parfile has no leading comment.
    fallback_summary: String,
    root: String,
    thornlist: ThornlistText,
    parfile: Parfile,
    repos: Vec<RepoPin>,
    run: Option<RunHints>,
    provenance: Provenance,
}

fn refuse_overwrite(file: &Path, describe: &SimDescribeArgs) -> Res<()> {
    if file.exists() && !describe.force {
        bail!("'{}' already exists (use -f to overwrite it)", file.display());
    }
    Ok(())
}

/// Refuse pins nobody else could reproduce: sources with uncommitted edits
/// (`when` says when they had them), and commits no remote has.
fn refuse_unfetchable(repos: &[RepoPin], published: &[bool], allow_dirty: bool, allow_unpushed: bool, when: &str) -> Res<()> {
    let dirty: Vec<&str> = repos.iter().filter(|r| r.dirty).map(|r| r.name.as_str()).collect();
    if !dirty.is_empty() && !allow_dirty {
        bail!(
            "These sources had uncommitted changes {when}, so no commit reproduces them: {}. Commit them (and \
             rebuild), or pass --allow-dirty to mark them dirty",
            dirty.join(", ")
        );
    }
    let unpushed: Vec<String> = repos
        .iter()
        .zip(published)
        .filter(|(_, published)| !**published)
        .map(|(r, _)| format!("{} ({})", r.name, short(r.commit.as_deref().unwrap_or(""))))
        .collect();
    if !unpushed.is_empty() && !allow_unpushed {
        bail!(
            "These commits are on no remote branch or tag this installation knows of, so nobody else could fetch \
             them: {}. Push them (and refetch, so cactup sees them), or pass --allow-unpushed",
            unpushed.join(", ")
        );
    }
    Ok(())
}

/// Settle the descriptive fields — flags, else defaults, offered for editing
/// on a terminal — then write the file and say what it holds.
fn finish(ctx: &Ctx, describe: &SimDescribeArgs, file: &Path, parts: Parts) -> Res<()> {
    let Parts { default_name, fallback_summary, root, thornlist, parfile, repos, run, provenance } = parts;
    warn_about_paths(&parfile);
    let (lead_summary, lead_description) = leading_comment(&parfile.text);
    let interactive = !describe.yes && std::io::stdin().is_terminal();
    let ask = |question: &str, given: &Option<String>, default: String| -> Res<String> {
        match given {
            Some(value) => Ok(value.clone()),
            None if interactive => crate::commands::prompt_with_default(question, &default),
            None => Ok(default),
        }
    };
    let name = ask("Name", &describe.name, default_name)?;
    let summary = ask("Summary", &describe.summary, lead_summary.unwrap_or(fallback_summary))?;
    let authors = match describe.authors.as_slice() {
        [] => {
            let default = git_user_name().unwrap_or_default();
            let given = ask("Authors (comma-separated)", &None, default)?;
            given.split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned).collect()
        }
        given => given.to_vec(),
    };
    let license_default = ctx.db.read().ok().and_then(|db| db.knob("default-license").map(str::to_owned));
    let license = ask("License (SPDX, empty for none)", &describe.license, license_default.unwrap_or_default())?;
    let mut inputs = inputs(&parfile);
    if interactive {
        for input in &mut inputs {
            let answer = crate::commands::prompt_with_default(
                &format!("What does the parfile need in knob {}? (a description for whoever imports it)", input.knob),
                "",
            )?;
            input.description = Some(answer).filter(|a| !a.is_empty());
        }
    }

    let sim_file = SimFile {
        format: cactup_ecosystem_common::SIM_FORMAT,
        sim: SimMeta {
            name,
            summary: Some(summary).filter(|s| !s.is_empty()),
            description: describe.description.clone().or(lead_description),
            authors,
            license: Some(license).filter(|l| !l.is_empty()),
            cactarium: None,
            root,
        },
        thornlist,
        parfile,
        inputs,
        repos,
        run,
        provenance: Some(provenance),
    };
    let text = sim_file.to_toml().map_err(|e| anyhow!("{e}"))?;
    std::fs::write(file, text).with_context(|| format!("Failed to write '{}'", file.display()))?;

    let pinned = sim_file.repos.iter().filter(|r| r.commit.is_some() || r.sha256.is_some()).count();
    println!(
        "Wrote '{}': {} with {pinned} of {} repo(s) pinned.",
        file.display(),
        sim_file.parfile.filename.bold(),
        sim_file.repos.len()
    );
    let missing = sim_file.unspecified();
    if !missing.is_empty() {
        let what: Vec<String> = missing.iter().map(ToString::to_string).collect();
        println!(
            "{}",
            format!("Not fully specified, so it cannot be published to the Cactarium: {}", what.join("; ")).yellow()
        );
    }
    Ok(())
}

/// `cactup sim pack` (§8.9): a `.sim` from a loose thornlist and parfile,
/// for whoever has those and no simulation. With `-I`, each repo is pinned
/// to what that installation has checked out (its live HEAD, checked like an
/// export's); without, to the commit its branch or tag names on the remote
/// today — a reproducible record of "the list as of now", and said so.
pub fn pack(ctx: &Ctx, args: SimPackArgs) -> Res<()> {
    let parfile = parfile_at(&args.parfile)?;
    let stem = super::parfile_stem(&parfile.filename).to_owned();
    let file = args.file.clone().unwrap_or_else(|| PathBuf::from(format!("{stem}.sim")));
    refuse_overwrite(&file, &args.describe)?;
    let text = std::fs::read_to_string(&args.thornlist)
        .with_context(|| format!("Failed to read '{}'", args.thornlist.display()))?;
    let list = thornlist::parse(&text).with_context(|| {
        format!("'{}' is not a GetComponents (CRL) list (with any !INCLUDE spliced in)", args.thornlist.display())
    })?;

    let (repos, published) = if ctx.globals.installation.is_some() {
        let inst = Installation::resolve(ctx)?;
        let repos_dir = inst.cactus_root().join("repos");
        let states = installed_states(&repos_dir, &list)?;
        let repos = repo_pins(&list, |repo| states.get(repo).cloned());
        let published = check_repos(&repos_dir, &repos, FetchState::read(&inst.root)?.as_ref())?;
        (repos, published)
    } else {
        let tips = remote_tips(&list)?;
        let repos = repo_pins(&list, |repo| tips.get(repo).map(ToString::to_string));
        println!(
            "{}",
            "Pinned each repo to the commit its branch or tag names today (pass -I to pin to what an installation \
             has instead)."
                .dimmed()
        );
        // Each commit is a remote tip by construction.
        let published = vec![true; repos.len()];
        (repos, published)
    };
    refuse_unfetchable(&repos, &published, args.allow_dirty, args.allow_unpushed, "in the installation")?;

    let thornlist_name = thornlist_filename(&args.thornlist.to_string_lossy());
    finish(
        ctx,
        &args.describe,
        &file,
        Parts {
            default_name: stem.clone(),
            fallback_summary: stem,
            root: list.root().to_owned(),
            thornlist: ThornlistText { filename: thornlist_name, text, untracked: Vec::new() },
            parfile,
            repos,
            run: None,
            provenance: exporter(),
        },
    )
}

/// Per git repo the list names, the source state (§7.4) of its checkout in
/// an installation — when it is a checkout of the very repo the list names.
/// One pointing elsewhere (a fork) is left out, so it is not pinned to a
/// commit of the wrong repo, and said so.
fn installed_states(repos_dir: &Path, list: &thornlist::Thornlist) -> Res<std::collections::BTreeMap<String, String>> {
    let repos = git_repos(list);
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("read installed repos");
    item.init(Some(repos.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let read = crate::par::parallel_map(&repos, |(repo, url, _, _)| {
        let current = item.lock().expect("pack progress poisoned").add_child(repo.clone());
        let dir = repos_dir.join(repo);
        let same_repo = gix::open(&dir)
            .ok()
            .and_then(|r| r.find_remote("origin").ok()?.url(gix::remote::Direction::Fetch).map(|u| u.to_bstring().to_string()))
            .is_some_and(|on_disk| fetch::git::normalize_url(&on_disk) == fetch::git::normalize_url(url));
        let state = if same_repo { fetch::git::source_state(&dir).ok() } else { None };
        drop(current);
        item.lock().expect("pack progress poisoned").inc();
        (repo.clone(), dir.exists() && !same_repo, state)
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    let mut states = std::collections::BTreeMap::new();
    for (repo, elsewhere, state) in read? {
        if elsewhere {
            println!("{}", format!("{repo}: the installation's checkout is of another repo; left unpinned").yellow());
        }
        if let Some(state) = state {
            states.insert(repo, state);
        }
    }
    Ok(states)
}

/// Per git repo the list names, the commit it pins (`!REPO_COMMIT`), else
/// the one its branch or tag (or default branch) names on its remote now.
/// Any remote that cannot be asked fails the whole pack: a silently unpinned
/// repo is worse than no file.
fn remote_tips(list: &thornlist::Thornlist) -> Res<std::collections::BTreeMap<String, gix::ObjectId>> {
    let (pinned, repos): (Vec<_>, Vec<_>) = git_repos(list).into_iter().partition(|r| r.3.is_some());
    let scratch = tempfile::tempdir().context("Failed to create a scratch directory")?;
    gix::init_bare(scratch.path()).context("Failed to create a scratch repository")?;
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("ask remotes for their tips");
    item.init(Some(repos.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let asked = crate::par::parallel_map(&repos, |(repo, url, git_ref, _)| {
        let current = item.lock().expect("pack progress poisoned").add_child(repo.clone());
        let tip = fetch::git::remote_tip(scratch.path(), url, git_ref.as_deref());
        drop(current);
        item.lock().expect("pack progress poisoned").inc();
        (repo.clone(), tip)
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    let mut tips = std::collections::BTreeMap::new();
    for (repo, _, _, commit) in pinned {
        let commit = commit.expect("partitioned on it");
        tips.insert(repo, gix::ObjectId::from_hex(commit.as_bytes()).context("The list pins an invalid commit")?);
    }
    let mut failed = Vec::new();
    for (repo, tip) in asked? {
        match tip {
            Ok(tip) => {
                tips.insert(repo, tip);
            }
            Err(e) => failed.push(format!("{repo}: {e:#}")),
        }
    }
    if !failed.is_empty() {
        bail!("Could not ask every remote for its tip:\n  {}", failed.join("\n  "));
    }
    Ok(tips)
}

/// Each git repo the list names once: (name, URL, branch or tag, pin).
type GitRepo = (String, String, Option<String>, Option<String>);
fn git_repos(list: &thornlist::Thornlist) -> Vec<GitRepo> {
    let mut repos: Vec<GitRepo> = Vec::new();
    for c in list.components().iter().filter(|c| c.ty == ComponentType::Git) {
        if !repos.iter().any(|r| r.0 == c.repo) {
            repos.push((c.repo.clone(), c.url.clone().unwrap_or_default(), c.branch.clone(), c.commit.clone()));
        }
    }
    repos
}

/// `cactup sim unpack` (§8.9): a `.sim` file back into the two files a
/// veteran works with — its thornlist, with every pinned repo pinned by
/// `!REPO_COMMIT`, and its parfile — both under the names they carried.
pub fn unpack(file: &Path, dir: &Path, force: bool) -> Res<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("Failed to read '{}'", file.display()))?;
    let sim = SimFile::from_toml(&text).map_err(|e| anyhow!("'{}': {e}", file.display()))?;
    let pins = sim.repos.iter().filter_map(|r| Some((r.name.clone(), r.commit.clone()?))).collect();
    let thornlist = thornlist::pin_text(&sim.thornlist.text, &pins)
        .with_context(|| format!("Failed to pin the thornlist in '{}'", file.display()))?;
    let outputs = [(&sim.thornlist.filename, thornlist), (&sim.parfile.filename, sim.parfile.text.clone())];
    for (name, _) in &outputs {
        let path = dir.join(name);
        if path.exists() && !force {
            bail!("'{}' already exists (use -f to overwrite it)", path.display());
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    for (name, content) in outputs {
        let path = dir.join(name);
        std::fs::write(&path, content).with_context(|| format!("Failed to write '{}'", path.display()))?;
        println!("Wrote '{}'", path.display());
    }
    let unpinned: Vec<&str> =
        sim.repos.iter().filter(|r| r.commit.is_none()).map(|r| r.name.as_str()).collect();
    if !unpinned.is_empty() {
        println!("{}", format!("Not pinned, so they follow their branch: {}", unpinned.join(", ")).yellow());
    }
    for input in &sim.inputs {
        let what = input.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
        println!("The parfile needs knob {}{what}", input.knob.bold());
    }
    Ok(())
}

/// The config record the simulation's binary was built from: the copy frozen
/// at create, or — for a simulation created before cactup froze it — the
/// config's own record, but only while it still describes that very build.
fn frozen_config(inst: &Installation, sim: &Simulation) -> Res<ConfigMeta> {
    if let Some(config) = sim.frozen_config()? {
        return Ok(config);
    }
    match ConfigMeta::load(&inst.cactus_root(), &sim.meta.configuration)? {
        Some(config) if config.build_id == sim.meta.build_id => Ok(config),
        _ => bail!(
            "Simulation \"{}\" was created before cactup recorded the sources of the build it runs, and config \
             \"{}\" has been rebuilt since, so they are lost. Create a new simulation from it to export",
            sim.name,
            sim.meta.configuration
        ),
    }
}

/// One pin per repo the list names, in order of first appearance: a git repo
/// at the commit of its source state (§7.4's `<commit>[+<n>mod@…]`), dirty
/// when that state saw local edits; a repo with no state, or anything not
/// git, unpinned.
fn repo_pins(list: &thornlist::Thornlist, state_of: impl Fn(&str) -> Option<String>) -> Vec<RepoPin> {
    let mut pins: Vec<RepoPin> = Vec::new();
    for c in list.components() {
        if pins.iter().any(|p| p.name == c.repo) {
            continue;
        }
        let state = if c.ty == ComponentType::Git { state_of(&c.repo) } else { None };
        pins.push(RepoPin {
            name: c.repo.clone(),
            url: c.url.clone().unwrap_or_default(),
            git_ref: c.branch.clone(),
            commit: state.as_deref().map(|s| fetch::committed(s).to_owned()),
            sha256: None,
            cactarium: None,
            dirty: state.is_some_and(|s| s.contains('+')),
        });
    }
    pins
}

/// Per pin, whether its commit is on the remote (true for anything unpinned:
/// there is nothing to check). The repos are walked in parallel, with
/// progress: on a network filesystem, eighty history walks take a while.
fn check_repos(repos_dir: &Path, pins: &[RepoPin], fetched: Option<&FetchState>) -> Res<Vec<bool>> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("check commits are published");
    item.init(Some(pins.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let checked = crate::par::parallel_map(pins, |pin| {
        let current = item.lock().expect("export progress poisoned").add_child(pin.name.clone());
        let published = match pin.commit.as_deref().map(|c| gix::ObjectId::from_hex(c.as_bytes())) {
            None => true,
            Some(Err(_)) => false,
            // cactup fetched exactly this commit from the remote.
            Some(Ok(id)) if fetched.and_then(|f| f.repos.get(&pin.name)).is_some_and(|r| r.head == id.to_string()) => {
                true
            }
            Some(Ok(id)) => fetch::git::is_published(&repos_dir.join(&pin.name), id).unwrap_or(false),
        };
        drop(current);
        item.lock().expect("export progress poisoned").inc();
        published
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    if gix::interrupt::is_triggered() {
        bail!("Interrupted");
    }
    checked
}

/// A parfile with the variables it uses, under its own file name.
fn parfile_at(path: &Path) -> Res<Parfile> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("'{}' names no file", path.display()))?;
    let text = std::fs::read_to_string(path).with_context(|| format!("Failed to read '{}'", path.display()))?;
    let kind = if filename.ends_with(".py") { ParfileKind::Py } else { ParfileKind::Par };
    // A `.py` parfile reads its variables through the preamble (§6.1), not
    // through `@…@` tokens, so there is nothing to scan.
    let vars = match kind {
        ParfileKind::Par => VarSet::references(&text, Syntax::Parfile)?.vars,
        ParfileKind::Py => Vec::new(),
    };
    Ok(Parfile { filename, kind, vars, text })
}

/// One input per knob the parfile requires (`@KNOB(name)@` with no default)
/// that is not one of cactup's standard knobs: those are the user's own
/// settings (allocation, mail, …), not something the simulation needs.
fn inputs(parfile: &Parfile) -> Vec<Input> {
    if parfile.kind != ParfileKind::Par {
        return Vec::new();
    }
    let Ok(refs) = VarSet::references(&parfile.text, Syntax::Parfile) else { return Vec::new() };
    refs.knobs
        .into_iter()
        .filter(|k| !k.optional && !KNOWN_KNOBS.iter().any(|s| s.name == k.name))
        .map(|k| Input { knob: k.name, description: None, url: None, sha256: None })
        .collect()
}

/// Warn about absolute paths in the parfile's values: whoever imports the
/// simulation will not have `/scratch/me/initial-data`, and a knob input is
/// how a parfile asks for such a thing portably.
fn warn_about_paths(parfile: &Parfile) {
    if parfile.kind != ParfileKind::Par {
        return;
    }
    for (n, line) in parfile.text.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        for (i, quoted) in code.split('"').enumerate() {
            if i % 2 == 1 && quoted.starts_with('/') && quoted != "/dev/null" && !quoted.starts_with("/dev/") {
                println!(
                    "{}",
                    format!(
                        "Line {}: \"{quoted}\" is an absolute path, which whoever imports this simulation will not \
                         have; consider a knob input such as @KNOB(my-data)@ instead",
                        n + 1
                    )
                    .yellow()
                );
            }
        }
    }
}

/// The parfile's leading comment block (after any blank lines), `#`s
/// stripped: its first line as a summary, and the whole block.
fn leading_comment(text: &str) -> (Option<String>, Option<String>) {
    let block: Vec<&str> = text
        .lines()
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(|l| {
            let l = l.trim_start().trim_start_matches('#');
            l.strip_prefix(' ').unwrap_or(l).trim_end()
        })
        .collect();
    let description = block.join("\n").trim().to_owned();
    let summary = block.iter().map(|l| l.trim()).find(|l| !l.is_empty()).map(str::to_owned);
    (summary, Some(description).filter(|d| !d.is_empty()))
}

fn provenance(sim: &Simulation, config: &ConfigMeta, list: &thornlist::Thornlist, processed: &str) -> Provenance {
    let (flavor, optionlist) = match &config.optionlist_source {
        OptionlistSource::Flavor(f) => (Some(f.clone()), None),
        OptionlistSource::Optionlist(p) => {
            (None, Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
        }
    };
    let flags = &config.flags;
    let flags = [("debug", flags.debug), ("optimize", flags.optimize), ("unsafe", flags.unsafe_build), ("profile", flags.profile)]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| name.to_owned())
        .collect();
    // What the machine or flavor switched off: disabled in the list Cactus
    // was handed, but not in the source list.
    let machine_disabled = thornlist::parse(processed)
        .map(|p| {
            p.disabled_thorns().iter().filter(|t| !list.disabled_thorns().contains(t)).cloned().collect()
        })
        .unwrap_or_default();
    Provenance {
        machine: Some(sim.meta.machine.clone()),
        flavor,
        optionlist,
        universe: config.universe.clone(),
        flags,
        machine_disabled,
        ..exporter()
    }
}

/// Provenance that says only who wrote the file, and when.
fn exporter() -> Provenance {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let version = match crate::build_info::DIST {
        Some(stamp) => format!("cactup {} ({})", env!("CARGO_PKG_VERSION"), stamp.id),
        None => format!("cactup {} (dev)", env!("CARGO_PKG_VERSION")),
    };
    Provenance {
        exported_by: Some(version),
        exported_at: now.parse().ok(),
        machine: None,
        flavor: None,
        optionlist: None,
        universe: None,
        flags: Vec::new(),
        machine_disabled: Vec::new(),
        base: None,
        extra: Default::default(),
    }
}

/// The shape of the latest restart, as defaults for whoever imports it.
fn run_hints(sim: &Simulation) -> Option<RunHints> {
    let id = *super::restart::list_ids(&sim.dir).ok()?.last()?;
    let meta = super::restart::Restart::load(&sim.dir, id).ok()?.meta;
    Some(RunHints {
        tasks: Some(meta.tasks).filter(|&t| t > 0),
        cpus_per_task: Some(meta.cpus).filter(|&c| c > 0),
        gpus_per_task: Some(meta.gpus_per_task).filter(|&g| g > 0),
        walltime: Some(meta.walltime.to_string()),
    })
}

/// A thornlist path's file name.
fn thornlist_filename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "thornlist.th".to_owned())
}

/// The user's git `user.name`, from their global git configuration.
fn git_user_name() -> Option<String> {
    let config = gix::config::File::from_globals().ok()?;
    config.string("user.name").map(|n| n.to_string()).filter(|n| !n.trim().is_empty())
}

fn short(commit: &str) -> &str {
    &commit[..12.min(commit.len())]
}
