//! `cactup import <file.sim>` (spec §8.9): reproduce a `.sim` in one step —
//! install its sources at their pinned commits, build them, create the
//! simulation with its knob inputs, and (with `--submit`/`--run`) start it.
//!
//! Everything that might need the user — the inputs' values, the downloads
//! behind them — is settled first, before the long install and build, so
//! nobody is asked a question an hour into a build. Every network step runs
//! here, on the login node (D11); the simulation's restarts then read only
//! what was frozen.

use super::Ctx;
use crate::args::{Args, Commands, ImportArgs, InstallArgs, SimCommand};
#[cfg(test)]
use crate::args::{StartFlags, TopologyFlags, UniverseFlags};
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::sim::{Input, SimFile};
use cactup_ecosystem_common::version::TrackedVersion;
use clap::Parser;
use colored::Colorize;
use indexmap::IndexMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn dispatch(ctx: &Ctx, args: ImportArgs) -> Res<()> {
    let path = match source_of(&args.simfile)? {
        Source::File(path) => path,
        Source::Published(req) => download_published(ctx, &req)?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read '{}'", path.display()))?;
    let sim_file = SimFile::from_toml(&text).map_err(|e| anyhow!("'{}': {e}", path.display()))?;
    let name = sim_file.sim.name.clone();
    let config = args.config.clone().unwrap_or_else(|| name.clone());
    let sim_name = args.sim.clone().unwrap_or_else(|| name.clone());
    // An installation that already has exactly this code is used as it is,
    // rather than a second copy of the same tree.
    let existing = if args.new_installation { None } else { matching_installation(ctx, &sim_file)? };
    let alias = match &existing {
        Some(alias) => alias.clone(),
        None => args.alias.clone().unwrap_or_else(|| name.clone()),
    };
    if existing.is_none() && ctx.db.read()?.installations.contains_key(&alias) {
        bail!("An installation named {alias} already exists; pass --alias to name the new one");
    }
    let interactive = !args.yes && std::io::stdin().is_terminal();

    // 1. The inputs, before anything long runs.
    let repo = repo_source(ctx, &sim_file, &args.inputs)?;
    let knobs = settle_inputs(ctx, &sim_file.inputs, &args.inputs, interactive, args.yes, repo.as_ref())?;

    // 2. The sources, at their pins.
    if let Some(alias) = &existing {
        println!("Installation {} already has the file's sources at its pins; using it.", alias.bold());
    } else {
        super::install::dispatch(
            ctx,
            InstallArgs {
                release: None,
                thornlist: None,
                simfile: Some(path.clone()),
                alias: Some(alias.clone()),
                silent: !interactive,
                install_prefix: args.install_prefix.clone(),
                no_symlink: args.no_symlink,
                symlink_prefix: None,
                symlink_name: None,
                inputs_settled: true,
            },
        )?;
        // `install` reports some refusals (an alias taken meanwhile, say) and
        // returns without error; nothing below may run on a tree it did not register.
        if !ctx.db.read()?.installations.contains_key(&alias) {
            bail!("The installation was not created, so the import stops here");
        }
    }
    let ctx = Ctx {
        globals: crate::args::GlobalOpts { installation: Some(alias.clone()), ..ctx.globals.clone() },
        db: ctx.db.clone(),
    };

    // 3. The build: in the foreground, or queued and waited for.
    let Commands::Build { start, .. } = parse(&build_command(&args, &config))? else {
        unreachable!("parsed as build")
    };
    super::build::build_to_completion(&ctx, *start)?;

    // 4. The simulation, from the file's parfile, with its knob inputs.
    let inst = Installation::resolve(&ctx)?;
    let machine = super::machine::resolve(&ctx)?;
    let imports = inst.root.join("imports");
    std::fs::create_dir_all(&imports)
        .with_context(|| format!("Failed to create '{}'", imports.display()))?;
    let parfile = imports.join(&sim_file.parfile.filename);
    std::fs::write(&parfile, &sim_file.parfile.text)
        .with_context(|| format!("Failed to write '{}'", parfile.display()))?;
    let mut sim = crate::sim::create(
        &ctx,
        &machine,
        &inst,
        &crate::sim::CreateRequest {
            force: false,
            ignore_machine: false,
            name: &sim_name,
            parfile: &parfile,
            config: Some(&config),
            sim_dir: None,
        },
    )?;
    if !knobs.is_empty() {
        sim.set_knobs(knobs)?;
    }

    // 5. Started, or the command that starts it.
    if args.submit || args.run {
        match started_command(&sim_file, &args, &sim_name)? {
            Commands::Sim(SimCommand::Submit(start)) => crate::sim::start::submit(&ctx, start),
            Commands::Sim(SimCommand::Run(run)) => crate::sim::start::run(&ctx, run),
            _ => unreachable!("parsed as sim submit/run"),
        }
    } else {
        let topology = start_args(&sim_file, &args);
        let words = ["cactup", "-I", alias.as_str(), "sim", "submit", sim_name.as_str()]
            .into_iter()
            .map(str::to_owned)
            .chain(topology);
        let command = words.map(|w| paste_word(&w)).collect::<Vec<_>>().join(" ");
        println!("Imported. To submit it with the file's suggested shape: {}", command.bold());
        Ok(())
    }
}

/// The installation to import into without installing: with `-I`, that one,
/// which must have the file's code exactly (else the import stops, saying
/// how it differs); without, the first installation that does, the active
/// one tried first. "Exactly": every thorn the file's thornlist builds is in
/// the installation's thornlist, built, from the same repository, and every
/// repo the file pins is checked out at that commit with no uncommitted
/// changes. Extra thorns are fine: they change nothing the file activates.
fn matching_installation(ctx: &Ctx, sim: &SimFile) -> Res<Option<String>> {
    let db = ctx.db.read()?;
    if let Some(alias) = &ctx.globals.installation {
        let inst = Installation::resolve(ctx)?;
        let differences = differences(&inst, sim)?;
        if !differences.is_empty() {
            bail!(
                "Installation {alias} does not have the file's code:\n  {}\nOmit -I (or pass --new-installation) to \
                 install a copy at the file's pins",
                differences.join("\n  ")
            );
        }
        return Ok(Some(alias.clone()));
    }
    let pins: Vec<(&str, &str)> =
        sim.repos.iter().filter_map(|r| Some((r.name.as_str(), r.commit.as_deref()?))).collect();
    if pins.is_empty() {
        return Ok(None);
    }
    // The cheap test first: what each installation's last fetch recorded.
    // Only one whose record agrees on every pin is probed on disk.
    let mut candidates: Vec<&crate::database::CactusInstallation> = db
        .installations
        .values()
        .filter(|entry| {
            let Ok(Some(state)) = crate::fetch::FetchState::read(std::path::Path::new(&entry.path)) else {
                return false;
            };
            pins.iter().all(|(_, commit)| state.repos.values().any(|r| r.head == *commit))
        })
        .collect();
    candidates.sort_by_key(|entry| db.active_installation.as_deref() != Some(entry.alias.as_str()));
    // A candidate that cannot be compared (an installation.toml a newer
    // cactup wrote, a live thornlist that does not parse) is no match, like
    // one whose fetch record cannot be read above: the user never named it,
    // so it must not stop the import, which installs a fresh copy instead.
    // An interrupt still stops it.
    for entry in candidates {
        let inst = Installation::new(entry.alias.clone(), &entry.path);
        match differences(&inst, sim) {
            Ok(differences) if differences.is_empty() => return Ok(Some(entry.alias.clone())),
            Ok(_) => {}
            Err(e) if gix::interrupt::is_triggered() => return Err(e),
            Err(_) => {}
        }
    }
    Ok(None)
}

/// How installation `inst`'s code differs from what `sim` records, one line
/// each (see [`matching_installation`]); empty when it has exactly that code.
fn differences(inst: &Installation, sim: &SimFile) -> Res<Vec<String>> {
    use crate::fetch::git::normalize_url;
    let wanted = crate::thornlist::parse(&sim.thornlist.text).context("The file's thornlist does not parse")?;
    let live_path = inst.live_thornlist_to_read()?;
    let live_text = std::fs::read_to_string(&live_path)
        .with_context(|| format!("Failed to read '{}'", live_path.display()))?;
    let live = crate::thornlist::parse(&live_text)
        .with_context(|| format!("'{}' does not parse", live_path.display()))?;
    let mut out = Vec::new();
    // The installation's name for each of the file's repos.
    let mut repo_dirs: IndexMap<String, String> = IndexMap::new();
    for c in wanted.components().iter().filter(|c| !c.fetch_only) {
        let Some(have) = live.components().iter().find(|l| l.checkout == c.checkout) else {
            out.push(format!("{} is not in its thornlist", c.checkout));
            continue;
        };
        let same_source = match (&c.url, &have.url) {
            (Some(want), Some(got)) => normalize_url(want) == normalize_url(got),
            (want, got) => want == got,
        };
        if !same_source {
            out.push(format!("{} comes from {} there", c.checkout, have.url.as_deref().unwrap_or("elsewhere")));
        } else if have.fetch_only {
            out.push(format!("{} is fetched but not built there", c.checkout));
        } else {
            repo_dirs.entry(c.repo.clone()).or_insert_with(|| have.repo.clone());
        }
    }
    let pins: Vec<(&str, &str, String)> = sim
        .repos
        .iter()
        .filter_map(|r| Some((r.name.as_str(), r.commit.as_deref()?, repo_dirs.get(&r.name)?.clone())))
        .collect();
    let repos_dir = inst.cactus_root()?.join("repos");
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child(format!("compare {} with the file's pins", inst.alias));
    item.init(Some(pins.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let states = crate::par::parallel_map(&pins, |(_, _, dir)| {
        let current = item.lock().expect("import progress poisoned").add_child(dir.clone());
        let state = crate::fetch::git::source_state(&repos_dir.join(dir)).ok();
        drop(current);
        item.lock().expect("import progress poisoned").inc();
        state
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    for ((name, commit, _), state) in pins.iter().zip(states?) {
        let short = |c: &str| c[..12.min(c.len())].to_owned();
        match state {
            None => out.push(format!("{name} cannot be read there")),
            Some(state) if state.contains('+') => out.push(format!("{name} has uncommitted changes there")),
            Some(state) if crate::fetch::committed(&state) != *commit => {
                out.push(format!("{name} is at {} there; the file pins {}", short(&state), short(commit)))
            }
            Some(_) => {}
        }
    }
    Ok(out)
}

/// `word` as it can be pasted into a shell and read back as itself: bare when
/// it is plain, else single-quoted.
fn paste_word(word: &str) -> String {
    let plain = !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain { word.to_owned() } else { crate::sim::start::shell_quote(word) }
}

/// What `import`'s argument names: a `.sim` file, or a sim published on
/// the Cactarium.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    File(PathBuf),
    Published(String),
}

/// A file when one is there. Otherwise a published sim when the argument
/// has a `/`, does not end in `.sim` and does not start like a path (`/`,
/// `.`, `~`); a mistyped reference (`Max/bbh`) then fails with what is wrong
/// with it, not with "no such file". Anything else is a missing file. A sim
/// name may hold a `.`, so a `.sim` that is not at the end does not make it
/// a path.
fn source_of(arg: &Path) -> Res<Source> {
    let text = arg.to_string_lossy();
    match std::fs::canonicalize(arg) {
        Ok(path) => Ok(Source::File(path)),
        Err(_) if text.contains('/') && !text.ends_with(".sim") && !text.starts_with(['/', '.', '~']) => {
            Ok(Source::Published(text.into_owned()))
        }
        Err(e) => Err(e).with_context(|| format!("Failed to find '{}'", arg.display())),
    }
}

/// Where the copy of a published sim downloaded from the Cactarium is kept,
/// under `root` (`~/.cactup/simfiles`): `<owner>/<name>/<version>.sim` for an
/// entry that is one sim (the layout since sims were first published, so
/// copies downloaded before stay in use), `<owner>/<name>/<sim>/<version>.sim`
/// for one sim of a multi-sim entry, which serves a file per sim at each
/// version. The two never meet: a sim name starts with a letter, a version
/// with a digit. `version` is a parsed version, written in its canonical
/// form, so nothing typed or answered can lead out of the cache. Every read
/// and write of the cache goes through this.
pub(crate) fn simfile_cache_path(
    root: &Path,
    owner: &str,
    name: &str,
    sim: Option<&str>,
    version: &TrackedVersion,
) -> PathBuf {
    let entry = root.join(owner).join(name);
    let dir = match sim {
        Some(sim) => entry.join(sim),
        None => entry,
    };
    dir.join(format!("{version}.sim"))
}

/// Download published sim `req` from the Cactarium (§8.10) into
/// `~/.cactup/simfiles` (see [`simfile_cache_path`]); see [`fetch_published`].
fn download_published(ctx: &Ctx, req: &str) -> Res<PathBuf> {
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    fetch_published(&client, &crate::CACTUP_ROOT.join("simfiles"), req)
}

/// The one version `slug` names exactly (`@=1.0.1`, or `@1.0.1` written out
/// in full), or `None` for a range (`@^1.0.1`, `@~2026.5.1`, `@>=1.0.0`
/// included), or no version at all.
fn exact_version(slug: &cactup_ecosystem_common::slug::Slug) -> Option<TrackedVersion> {
    if !slug.req().is_ok_and(|r| r.is_exact()) {
        return None;
    }
    slug.at.as_deref()?.trim_start_matches('=').trim().parse().ok()
}

/// Fetch published sim `req` into the cache under `root`, checked against the
/// hash the registry sends, and return the cached file. `req` is
/// `owner/name[@range|=X.Y.Z]` for an entry that is one sim, or
/// `owner/name/sim[@range|=X.Y.Z]` for one sim of a multi-sim entry (the
/// newest version in range that has that sim; a newer one in range that
/// dropped it is warned about). A bare `owner/name` of a multi-sim entry is
/// refused by the Cactarium, listing its sims, and that message is shown as
/// is.
fn fetch_published(client: &crate::cactarium::Client, root: &Path, req: &str) -> Res<PathBuf> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = req
        .parse()
        .map_err(|e| anyhow!("\"{req}\" is neither a file nor a sim (owner/name[/sim][@version]): {e}"))?;
    let (owner, name) = (slug.owner.as_str(), slug.name.as_str());
    // The third segment of a sim reference is always one sim of a multi-sim entry.
    let sim = slug.as_sim().map_err(|e| anyhow!("\"{req}\" is not a sim (owner/name[/sim][@version]): {e}"))?;
    let sim = sim.as_ref().map(|s| s.as_str());
    let shown = match sim {
        Some(sim) => format!("{owner}/{name}/{sim}"),
        None => format!("{owner}/{name}"),
    };
    let cached = |version: &TrackedVersion| simfile_cache_path(root, owner, name, sim, version);
    let exact = exact_version(&slug);
    // Resolved even for an exact version: that is how a yanked version is
    // told. An exact version already downloaded still imports offline, its
    // yank status unknown.
    let resolved = match (client.resolve(req), &exact) {
        (Ok(resolved), _) => resolved,
        (Err(e), Some(v)) if cached(v).is_file() => {
            println!(
                "{}",
                format!("Could not ask the Cactarium about {req} ({e:#}); using the copy downloaded before").dimmed()
            );
            return Ok(cached(v));
        }
        // A Cactarium from before multi-sim entries reads the third segment
        // as a thorn, and refuses a sim name that is not a thorn name, in its
        // own words. A current one says "is not a thorn name" too, but only
        // of a package, and that message is shown as it is.
        (Err(e), _)
            if sim.is_some()
                && format!("{e:#}").contains("is not a Cactarium reference")
                && format!("{e:#}").contains("is not a thorn name") =>
        {
            return Err(predates_multi_sim(client, &shown, &format!("{e:#}")));
        }
        (Err(e), _) => return Err(e),
    };
    if resolved.kind != "sim" {
        bail!("{} is a {}, not a sim", resolved.slug, resolved.kind);
    }
    // The answer must be about the sim asked for. One from a Cactarium that
    // predates multi-sim entries names none: it read the sim as a thorn and
    // answered for the whole entry, which is not what was asked.
    match (sim, resolved.sim.as_deref()) {
        (Some(want), Some(got)) if want == got => {}
        (Some(_), None) => return Err(predates_multi_sim(client, &shown, "its answer names no sim")),
        (Some(want), Some(got)) => bail!("Asked the Cactarium for the sim {want} of {owner}/{name}, and it answered with {got}"),
        (None, Some(got)) => bail!(
            "The Cactarium answered {req} with the sim {got}; name the sim you want, as {owner}/{name}/{got}"
        ),
        (None, None) => {}
    }
    crate::cactarium::warn_if_yanked(&resolved.slug, &resolved.version);
    // A version is all a cache path and a download URL are made of: only
    // one that reads as a version is taken, in its canonical form.
    let version: TrackedVersion = resolved
        .version
        .version
        .parse()
        .map_err(|_| anyhow!("The Cactarium named sim version \"{}\"", resolved.version.version.escape_debug()))?;
    // An exact request answered with another version would import code the
    // user did not ask for; refuse rather than trust the server's matching.
    if let Some(wanted) = &exact
        && version != *wanted
    {
        bail!("Asked the Cactarium for {shown}@{wanted}, and it answered with version {version}");
    }
    // Said, never refused: the version chosen is what was asked for, the
    // newest in range that has the sim.
    if let (Some(sim), Some(newer)) = (sim, resolved.newer_without_sim.as_deref()) {
        println!(
            "{}",
            format!(
                "{owner}/{name}@{newer} no longer has the sim {sim}; importing {version}, the newest version \
                 asked for that has it"
            )
            .yellow()
        );
    }
    let dest = cached(&version);
    if !dest.is_file() {
        // Checked (hash, and for one sim of several its stamp) before it is
        // put in the cache, whose copies are used again unchecked.
        client.download_sim(owner, name, &version.to_string(), sim, &dest)?;
        println!("Downloaded {shown}@{version} from {}", client.base());
    }
    Ok(dest)
}

/// A Cactarium that cannot serve one sim of a multi-sim entry: it predates
/// them, and would read `owner/name/sim` as a package's thorn.
fn predates_multi_sim(client: &crate::cactarium::Client, shown: &str, detail: &str) -> anyhow::Error {
    anyhow!(
        "The Cactarium at {} predates sims named as owner/name/sim ({detail}), so it cannot serve {shown}; \
         import it from a Cactarium that has them",
        client.base()
    )
}

/// `cactup login` (§8.10): store a token for the Cactarium this cactup
/// talks to, in `~/.cactup/credentials` (owner-only), never as a knob.
pub fn login(ctx: &Ctx, token: Option<String>) -> Res<()> {
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let token = match token {
        Some(token) => token,
        None => {
            println!("Create a token at {}/settings, then paste it here.", client.base());
            super::prompt_with_default("Token", "")?
        }
    };
    let token = token.trim();
    if token.is_empty() {
        bail!("No token given");
    }
    let path = crate::cactarium::credentials_path();
    crate::cactarium::store_token(&path, client.base(), token)?;
    println!("Stored the token for {} in '{}'", client.base(), path.display());
    Ok(())
}

/// `cactup sim publish <file.sim> <owner/name>` (§8.10): only a fully
/// specified file — every repo pinned, nothing dirty or untracked — since
/// the registry's promise is that anyone can reproduce it.
pub fn publish(ctx: &Ctx, file: &std::path::Path, target: &str, version: Option<&str>) -> Res<()> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = target.parse().map_err(|e| anyhow!("\"{target}\" is not owner/name: {e}"))?;
    if slug.at.is_some() || slug.sha.is_some() || slug.thorn.is_some() {
        bail!("Publish to owner/name, with --version X.Y.Z to choose the version");
    }
    let text = std::fs::read_to_string(file).with_context(|| format!("Failed to read '{}'", file.display()))?;
    let sim = SimFile::from_toml(&text).map_err(|e| anyhow!("'{}': {e}", file.display()))?;
    let missing = sim.unspecified();
    if !missing.is_empty() {
        let what: Vec<String> = missing.iter().map(ToString::to_string).collect();
        bail!("'{}' is not fully specified, so it cannot be published: {}", file.display(), what.join("; "));
    }
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let published = client.publish_sim(slug.owner.as_str(), slug.name.as_str(), text.into_bytes(), version)?;
    print_published(&published);
    for warning in &published.warnings {
        println!("{}", warning.yellow());
    }
    Ok(())
}

/// `cactup stack publish` (§8.10): cut release `version` of stack
/// `owner/name` from its definition (and its own thornlist, for a flat
/// stack). The registry resolves and freezes it; a release never changes,
/// so publishing an existing version again is refused there.
pub fn publish_stack(
    ctx: &Ctx,
    definition: &std::path::Path,
    target: &str,
    version: &str,
    thornlist: Option<&std::path::Path>,
) -> Res<()> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = target.parse().map_err(|e| anyhow!("\"{target}\" is not owner/name: {e}"))?;
    if slug.at.is_some() || slug.sha.is_some() || slug.thorn.is_some() {
        bail!("Name the stack as owner/name, and its version with --version");
    }
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path).with_context(|| format!("Failed to read '{}'", path.display()))
    };
    let request = cactup_ecosystem_common::api::ReleaseRequest {
        definition: read(definition)?,
        thornlist: thornlist.map(read).transpose()?,
        version: version.to_owned(),
    };
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let published = client.publish_stack(slug.owner.as_str(), slug.name.as_str(), &request)?;
    print_published(&published);
    for warning in &published.warnings {
        println!("{}", warning.yellow());
    }
    Ok(())
}

/// `cactup refresh owner/name` (§8.10): have the Cactarium look for new
/// release tags in a package's (or a repo-kept stack's) repository now, so a
/// release just tagged can be installed without waiting for the next poll.
pub fn refresh(ctx: &Ctx, target: &str) -> Res<()> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = target.parse().map_err(|e| anyhow!("\"{target}\" is not owner/name: {e}"))?;
    if slug.at.is_some() || slug.sha.is_some() || slug.thorn.is_some() {
        bail!("Name the package or stack as owner/name");
    }
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    println!("Asking {} to check {} for new tags...", client.base(), slug.entry());
    let refreshed = client.refresh(slug.owner.as_str(), slug.name.as_str())?;
    match refreshed.new_versions {
        0 => println!("{}: no new versions.", refreshed.slug),
        1 => println!("{}: {}", refreshed.slug, "1 new version.".green()),
        n => println!("{}: {}", refreshed.slug, format!("{n} new versions.").green()),
    }
    // New versions of a public entry may wait for an admin; until then the
    // Cactarium neither serves nor lists them.
    match refreshed.awaiting_review {
        0 => {}
        1 => println!("{}", "1 version awaits review, and is not served until an admin approves it.".yellow()),
        n => println!("{}", format!("{n} versions await review, and are not served until an admin approves them.").yellow()),
    }
    Ok(())
}

/// What a publish made, and whether anyone can have it yet: a release that
/// waits for review is not served, listed or resolved until it is approved.
fn print_published(published: &cactup_ecosystem_common::api::Published) {
    if published.visibility == "pending" {
        println!("Published {}: {}", published.slug.bold(), published.url);
        println!("{}", "It awaits review: nobody can install or import it until an admin approves it.".yellow());
    } else {
        println!("Published {} ({}): {}", published.slug.bold(), published.visibility, published.url);
    }
}

/// The knob values for the parfile's inputs (§5.1): from `--input`, else a
/// download of the input's URL, else asked for. An input left empty while
/// the user's global knob has a value takes the global value at submit (the
/// simulation records nothing for it); one with neither is an error now,
/// not at submit.
/// Where the `.sim`'s `repo-file` inputs come from, when it has any not
/// given with `--input`: the repository the Cactarium publishes it from, at
/// the commit of the version its `cactarium` stamp names.
fn repo_source(ctx: &Ctx, sim: &SimFile, given: &[String]) -> Res<Option<crate::inputs::RepoSource>> {
    let given_knob = |knob: &str| given.iter().any(|g| g.split_once('=').is_some_and(|(k, _)| k == knob));
    let Some(first) = sim.inputs.iter().find(|i| i.repo_file.is_some() && !given_knob(&i.knob)) else { return Ok(None) };
    let Some(slug) = &sim.sim.cactarium else {
        bail!(
            "Input {} is a file of the simulation's own repository, and this .sim names no Cactarium version to find \
             that repository by; import it as owner/name, or give it with --input {}=VALUE",
            first.knob,
            first.knob
        );
    };
    let resolved = crate::cactarium::Client::new(&ctx.db.read()?).resolve(&slug.to_string())?;
    let url = resolved.url.ok_or_else(|| anyhow!("The Cactarium names no repository for {slug}"))?;
    let commit = resolved.version.commit.ok_or_else(|| anyhow!("The Cactarium names no commit for {slug}"))?;
    Ok(Some(crate::inputs::RepoSource { url, commit }))
}

fn settle_inputs(
    ctx: &Ctx,
    inputs: &[Input],
    given: &[String],
    interactive: bool,
    yes: bool,
    repo: Option<&crate::inputs::RepoSource>,
) -> Res<IndexMap<String, String>> {
    let mut given_values = IndexMap::new();
    for pair in given {
        let (knob, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("--input wants KNOB=VALUE, not \"{pair}\""))?;
        if !inputs.iter().any(|i| i.knob == knob) {
            let known: Vec<&str> = inputs.iter().map(|i| i.knob.as_str()).collect();
            bail!("The parfile has no input {knob} (its inputs: {})", known.join(", "));
        }
        given_values.insert(knob.to_owned(), value.to_owned());
    }
    let db = ctx.db.read()?;
    // Resolved only when an input is fetched: finding it may read the machine.
    let mut dir = None;
    let policy = crate::inputs::SizePolicy::new(interactive, yes);
    let mut knobs = IndexMap::new();
    let mut missing = Vec::new();
    for input in inputs {
        let what = input.description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
        let value = if let Some(value) = given_values.shift_remove(&input.knob) {
            Some(value)
        } else if input.url.is_some() || input.repo_file.is_some() {
            println!("Input {}{what}", input.knob.bold());
            if dir.is_none() {
                dir = Some(crate::inputs::inputs_dir(ctx)?);
            }
            let value = crate::inputs::fetch(dir.as_deref().expect("set above"), input, &policy, repo)
                .with_context(|| format!("Failed to fetch input {}", input.knob))?;
            println!("  {} = {}", input.knob, value.display());
            Some(value.display().to_string())
        } else if interactive {
            let global = db.knob(&input.knob).unwrap_or("");
            let answer = super::prompt_with_default(
                &format!("Value for the parfile's input {}{what}", input.knob),
                global,
            )?;
            // The global value, taken as offered, stays the global knob's.
            Some(answer).filter(|a| !a.is_empty() && a != global)
        } else {
            None
        };
        match value {
            Some(value) => {
                knobs.insert(input.knob.clone(), value);
            }
            None if db.knob(&input.knob).is_some() => {}
            None => missing.push(format!("{}{what}", input.knob)),
        }
    }
    if !missing.is_empty() {
        bail!("The parfile needs values for: {}. Pass each as --input KNOB=VALUE", missing.join(", "));
    }
    Ok(knobs)
}

/// The `cactup build` command line the import runs: its config, with the
/// flavor and make jobs given to import.
fn build_command(args: &ImportArgs, config: &str) -> Vec<String> {
    let mut build = vec!["build".to_owned(), config.to_owned()];
    if let Some(flavor) = &args.flavor {
        build.extend(["--flavor".to_owned(), flavor.clone()]);
    }
    if let Some(jobs) = args.make_jobs {
        build.extend(["--make-jobs".to_owned(), jobs.to_string()]);
    }
    build
}

/// The flags for `sim submit`/`run`: every start, universe and TOPOLOGY flag
/// given to import, passed through as given; the `.sim`'s run hints fill in
/// tasks, CPUs and GPUs per task and wall time where those give none. A
/// `.sim` suggests no node count (it is machine shaped), so without `-n` the
/// started command derives one from the tasks (§8.5).
fn start_args(sim: &SimFile, args: &ImportArgs) -> Vec<String> {
    let hints = sim.run.as_ref();
    let mut topology = args.topology.clone();
    topology.tasks = topology.tasks.or(hints.and_then(|h| h.tasks));
    topology.cpus = topology.cpus.or(hints.and_then(|h| h.cpus_per_task));
    topology.gpus_per_task = topology.gpus_per_task.or(hints.and_then(|h| h.gpus_per_task));
    let mut words = args.start_flags.to_args();
    words.extend(args.universe.to_args());
    words.extend(topology.to_args());
    // The hint stays text: the started command parses it like any typed one.
    if topology.wall_time.is_none()
        && let Some(wall) = hints.and_then(|h| h.walltime.clone())
    {
        words.extend(["--wall-time".to_owned(), wall]);
    }
    words
}

/// The `sim submit` (with `--submit`) or `sim run` (with `--run`) that
/// import starts for simulation `sim_name`, parsed like a typed command.
/// Its `--allocation` was laid into this command's knob overlay with the
/// rest of import's flags (`Commands::topology`, §5.1), so the started
/// command sees it as the allocation knob.
pub(crate) fn started_command(sim: &SimFile, args: &ImportArgs, sim_name: &str) -> Res<Commands> {
    let verb = if args.submit { "submit" } else { "run" };
    let mut command = vec!["sim".to_owned(), verb.to_owned(), sim_name.to_owned()];
    command.extend(start_args(sim, args));
    parse(&command)
}

/// Parse a cactup command line (without the program name), so the steps
/// import chains get every default exactly as typed commands would.
fn parse(words: &[String]) -> Res<Commands> {
    let argv = std::iter::once("cactup".to_owned()).chain(words.iter().cloned());
    Args::try_parse_from(argv).map(|a| a.command).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
pub(crate) mod tests {
    /// The printed command pastes back as the words it was built from.
    #[test]
    fn printed_words_paste_back_as_themselves() {
        assert_eq!(super::paste_word("--job-name"), "--job-name");
        assert_eq!(super::paste_word("my job"), "'my job'");
        assert_eq!(super::paste_word("it's"), "'it'\\''s'");
        assert_eq!(super::paste_word(""), "''");
        assert_eq!(super::paste_word("01:00:00"), "01:00:00");
    }

    use super::*;
    use cactup_ecosystem_common::sim::{Parfile, ParfileKind, RunHints, SimMeta, ThornlistText};

    /// An installation has a .sim's code when every thorn the file builds
    /// is there from the same repo and every pinned repo is at its pin,
    /// clean; extra thorns do not matter. Each way it can differ is named.
    #[test]
    fn an_installation_matches_a_sim_only_at_its_exact_pins() {
        use crate::fetch::git::testrepo;
        use cactup_ecosystem_common::sim::RepoPin;
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream/carpetx");
        testrepo::init(&upstream);
        let v1 = testrepo::commit_tree(&upstream, &[("Makefile", "v1\n", false)]);
        let v2 = testrepo::commit_tree(&upstream, &[("Makefile", "v2\n", false)]);
        let (branch, _) = crate::fetch::git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let section = |checkout: &str| {
            format!("!TARGET = $ROOT\n!TYPE = git\n!URL = {url}\n!REPO_BRANCH = {branch}\n!NAME = carpetx\n!CHECKOUT =\n{checkout}\n")
        };
        let header = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n";
        let sim = |checkout: &str, commit: gix::ObjectId| SimFile {
            format: cactup_ecosystem_common::SIM_FORMAT,
            sim: SimMeta {
                name: "bhns".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText { filename: "bhns.th".into(), text: format!("{header}{}", section(checkout)), untracked: vec![] },
            parfile: Parfile { filename: "bhns.par".into(), kind: ParfileKind::Par, vars: vec![], text: "x\n".into() },
            inputs: vec![],
            repos: vec![RepoPin {
                name: "carpetx".into(),
                url: url.clone(),
                git_ref: Some(branch.clone()),
                commit: Some(commit.to_string()),
                sha256: None,
                cactarium: None,
                dirty: false,
            }],
            run: None,
            provenance: None,
        };

        // An installation of the repo at v1, with one more file than the
        // .sim builds.
        let root = tmp.path().join("install");
        let inst = Installation::new("t", &root);
        let live = format!("{header}{}", section("Makefile\nREADME.extra"))
            .replace("!NAME = carpetx\n", &format!("!NAME = carpetx\n!REPO_COMMIT = {v1}\n"));
        let list = crate::thornlist::parse(&live).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let plan = crate::fetch::plan(&list, &root, &mut progress).unwrap();
        let report = crate::fetch::execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        std::fs::create_dir_all(inst.live_thornlist().unwrap().parent().unwrap()).unwrap();
        std::fs::write(inst.live_thornlist().unwrap(), &live).unwrap();

        assert_eq!(differences(&inst, &sim("Makefile", v1)).unwrap(), Vec::<String>::new());
        let at_v2 = differences(&inst, &sim("Makefile", v2)).unwrap();
        assert!(at_v2.len() == 1 && at_v2[0].starts_with("carpetx is at "), "{at_v2:?}");
        let missing = differences(&inst, &sim("README", v1)).unwrap();
        assert_eq!(missing, ["README is not in its thornlist"]);

        // Auto-match skips an installation it cannot compare, here one whose
        // installation.toml a newer cactup wrote, even the active one with a
        // fetch record that agrees on every pin: the user never named it, so
        // it must not stop the import.
        crate::fetch::FetchState::record(&root, &report.records()).unwrap();
        let newer = tmp.path().join("newer");
        std::fs::create_dir_all(newer.join(".cactup")).unwrap();
        std::fs::copy(root.join(".cactup/fetch-state.toml"), newer.join(".cactup/fetch-state.toml")).unwrap();
        let schema = crate::installation::INSTALLATION.current + 1;
        std::fs::write(newer.join(".cactup/installation.toml"), format!("schema = {schema}\n")).unwrap();
        let entry = |alias: &str, path: &std::path::Path| crate::database::CactusInstallation {
            alias: alias.into(),
            release: None,
            path: path.display().to_string(),
            thornlist: None,
            simfile: None,
            stack: None,
            current_release: None,
            current_thornlist: None,
            stack_contact: None,
            unfetched_repos: IndexMap::new(),
        };
        let ctx = ctx(&tmp.path().join("db"));
        ctx.db
            .update(|d| {
                d.installations.insert("newer".to_owned(), entry("newer", &newer));
                d.active_installation = Some("newer".to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(matching_installation(&ctx, &sim("Makefile", v1)).unwrap(), None);
        ctx.db
            .update(|d| {
                d.installations.insert("t".to_owned(), entry("t", &root));
                Ok(())
            })
            .unwrap();
        assert_eq!(matching_installation(&ctx, &sim("Makefile", v1)).unwrap().as_deref(), Some("t"));
        std::fs::write(root.join("Cactus/repos/carpetx/Makefile"), "edited\n").unwrap();
        assert_eq!(differences(&inst, &sim("Makefile", v1)).unwrap(), ["carpetx has uncommitted changes there"]);
    }

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
                risky_allow_untrusted_artifacts: false,
            },
            db: crate::database::Db::in_dir(dir),
        }
    }

    fn input(knob: &str) -> Input {
        Input {
            knob: knob.into(),
            description: Some("initial data".into()),
            url: None,
            sha256: None,
            unpack: false,
            path: None,
            fixups: vec![],
            repo_file: None,
        }
    }

    /// Inputs are settled before anything long runs: `--input` sets the
    /// simulation's own value; one left unset falls back to a global knob the
    /// user already has (recording nothing); one with neither fails now.
    #[test]
    fn inputs_come_from_flags_else_the_global_knob_else_fail_early() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path());
        let inputs = [input("kadath-initial-data"), input("eos-table")];

        let err = format!("{:#}", settle_inputs(&ctx, &inputs, &[], false, false, None).unwrap_err());
        assert!(
            err.contains("kadath-initial-data") && err.contains("eos-table") && err.contains("--input"),
            "{err}"
        );

        ctx.db
            .update(|db| {
                db.set_knob("eos-table", "/global/eos.h5".to_owned());
                Ok(())
            })
            .unwrap();
        let knobs =
            settle_inputs(&ctx, &inputs, &["kadath-initial-data=/id/bhns.info".to_owned()], false, false, None)
                .unwrap();
        assert_eq!(knobs.len(), 1, "the global eos-table is used as is, not copied: {knobs:?}");
        assert_eq!(knobs["kadath-initial-data"], "/id/bhns.info");

        let err =
            format!("{:#}", settle_inputs(&ctx, &inputs, &["nope=1".to_owned()], false, false, None).unwrap_err());
        assert!(err.contains("no input nope"), "{err}");
    }

    #[test]
    fn topology_takes_flags_over_the_files_suggestion() {
        let sim = SimFile {
            format: cactup_ecosystem_common::SIM_FORMAT,
            sim: SimMeta {
                name: "s".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText { filename: "s.th".into(), text: String::new(), untracked: vec![] },
            parfile: Parfile {
                filename: "s.par".into(),
                kind: ParfileKind::Par,
                vars: vec![],
                text: String::new(),
            },
            inputs: vec![],
            repos: vec![],
            run: Some(RunHints {
                tasks: Some(256),
                cpus_per_task: Some(8),
                gpus_per_task: None,
                walltime: Some("48:00:00".into()),
            }),
            provenance: None,
        };
        let args = ImportArgs::try_parse_from_args(&["s.sim", "-T", "64", "-q", "checkpt"]);
        assert_eq!(
            start_args(&sim, &args),
            ["--queue", "checkpt", "--tasks", "64", "--cpus", "8", "--wall-time", "48:00:00"]
        );
        // No node count of its own: `sim submit` derives it from the tasks.
        // An explicit one is passed through, as is a wall time given here.
        let args = ImportArgs::try_parse_from_args(&["s.sim", "-n", "2", "-w", "1:00:00"]);
        assert_eq!(
            start_args(&sim, &args),
            ["--nodes", "2", "--tasks", "256", "--cpus", "8", "--wall-time", "01:00:00"]
        );
    }

    fn sim_without_hints() -> SimFile {
        SimFile {
            format: cactup_ecosystem_common::SIM_FORMAT,
            sim: SimMeta {
                name: "s".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText { filename: "s.th".into(), text: String::new(), untracked: vec![] },
            parfile: Parfile {
                filename: "s.par".into(),
                kind: ParfileKind::Par,
                vars: vec![],
                text: String::new(),
            },
            inputs: vec![],
            repos: vec![],
            run: None,
            provenance: None,
        }
    }

    /// The start, universe and TOPOLOGY flags as one comparable string.
    fn shared(start: &StartFlags, universe: &UniverseFlags, topology: &TopologyFlags) -> String {
        format!("{start:?} {universe:?} {topology:?}")
    }

    /// Every flag of the groups `import` shares with `sim submit`/`sim run`
    /// parses under `import` (long form, and short form but for `-a`) and
    /// reaches the started command exactly as given. The flags are read off
    /// the groups' own clap definitions, so one added to a group later is
    /// covered without touching this test, and fails here if `to_args` does
    /// not pass it on.
    #[test]
    fn import_passes_every_shared_flag_through() {
        use clap::Args as _;
        let sim = sim_without_hints();
        let parse_import = |words: &[String]| {
            let mut argv: Vec<String> =
                ["cactup", "import", "s.sim", "--submit"].iter().map(|w| w.to_string()).collect();
            argv.extend(words.iter().cloned());
            match Args::try_parse_from(argv).map(|a| a.command) {
                Ok(Commands::Import(args)) => Some(args),
                _ => None,
            }
        };
        let defaults = parse_import(&[]).unwrap();
        let defaults = shared(&defaults.start_flags, &defaults.universe, &defaults.topology);
        let groups = [
            TopologyFlags::augment_args(clap::Command::new("g")),
            UniverseFlags::augment_args(clap::Command::new("g")),
            StartFlags::augment_args(clap::Command::new("g")),
        ];
        let mut checked = 0;
        for arg in groups.iter().flat_map(|g| g.get_arguments()) {
            let long = arg.get_long().unwrap_or_else(|| panic!("{} has no long form", arg.get_id()));
            // A value each kind of flag accepts: a count, a wall time, a name.
            let with = |flag: String| -> Option<(Vec<String>, ImportArgs)> {
                if !arg.get_action().takes_values() {
                    return parse_import(std::slice::from_ref(&flag)).map(|a| (vec![flag.clone()], a));
                }
                ["3", "1:00:00", "x"].iter().find_map(|v| {
                    let words = vec![flag.clone(), v.to_string()];
                    parse_import(&words).map(|a| (words, a))
                })
            };
            let (words, args) =
                with(format!("--{long}")).unwrap_or_else(|| panic!("--{long} does not parse under import"));
            let given = shared(&args.start_flags, &args.universe, &args.topology);
            assert_ne!(given, defaults, "--{long} changed nothing");
            assert_eq!(args.alias, None, "--{long}");
            if let Some(short) = arg.get_short().filter(|_| long != "allocation") {
                let (_, by_short) =
                    with(format!("-{short}")).unwrap_or_else(|| panic!("-{short} does not parse under import"));
                let by_short = shared(&by_short.start_flags, &by_short.universe, &by_short.topology);
                assert_eq!(by_short, given, "-{short}");
            }
            for verb in ["submit", "run"] {
                let mut command = vec!["sim".to_owned(), verb.to_owned(), "s".to_owned()];
                command.extend(start_args(&sim, &args));
                let started = match parse(&command).unwrap() {
                    Commands::Sim(SimCommand::Submit(start)) => start,
                    Commands::Sim(SimCommand::Run(run)) => run.start,
                    _ => unreachable!(),
                };
                let reached = shared(&started.start_flags, &started.universe, &started.topology);
                assert_eq!(reached, given, "{words:?} under `sim {verb}`");
            }
            checked += 1;
        }
        assert!(checked >= 19, "only {checked} shared flags found");

        // `-a` is the alias here; the account is `--allocation`.
        let args = parse_import(&["-a".to_owned(), "mine".to_owned()]).unwrap();
        assert_eq!((args.alias.as_deref(), args.topology.allocation.as_deref()), (Some("mine"), None));
        let args = parse_import(&["--allocation".to_owned(), "acct".to_owned()]).unwrap();
        assert_eq!((args.alias.as_deref(), args.topology.allocation.as_deref()), (None, Some("acct")));
        // Passed through like the rest, --allocation is also, as `-a` is
        // elsewhere, this command's allocation knob (`Commands::topology`):
        // the started command's resolve_topology reads it from there.
        let argv = ["cactup", "import", "s.sim", "--submit", "--allocation", "acct"];
        let overlay = Args::try_parse_from(argv).unwrap().knob_overlay().unwrap();
        assert_eq!(overlay.get("allocation").map(String::as_str), Some("acct"));
    }

    /// `import` takes everything `sim submit` and `sim run` take, but for
    /// what only names or creates a simulation there (`sim` and `config` are
    /// import's own `--sim`/`--config`, naming what it creates). A flag added to
    /// `sim submit`/`sim run` outside the shared groups fails here.
    #[test]
    fn import_accepts_what_sim_submit_and_run_accept() {
        use clap::CommandFactory;
        let mut cmd = Args::command();
        cmd.build();
        let import = cmd.find_subcommand("import").unwrap();
        let has: std::collections::HashSet<&str> =
            import.get_arguments().map(|a| a.get_id().as_str()).collect();
        let sim = cmd.find_subcommand("sim").unwrap();
        let missing = |verb: &str| -> Vec<String> {
            let sub = sim.find_subcommand(verb).unwrap();
            let ids = sub.get_arguments().map(|a| a.get_id().to_string());
            ids.filter(|id| !has.contains(id.as_str())).collect()
        };
        let own = ["parfile", "force", "overwrite", "silent"];
        assert_eq!(missing("submit"), own);
        let mut run_own = own.to_vec();
        run_own.extend(["debug", "restart_id", "sim_dir"]);
        assert_eq!(missing("run"), run_own);
        // Only the allocation's short form is dropped, and only here.
        let short = |c: &clap::Command| {
            c.get_arguments().find(|a| a.get_id() == "allocation").unwrap().get_short()
        };
        assert_eq!(short(import), None);
        assert_eq!(short(sim.find_subcommand("submit").unwrap()), Some('a'));
    }

    /// `-j` reaches import's build, with or without --submit/--run, and is
    /// not the TOPOLOGY's `-J` (job name).
    #[test]
    fn import_make_jobs_reaches_the_build() {
        use crate::args::MakeJobs;
        let jobs_of = |words: &[&str]| {
            let args = ImportArgs::try_parse_from_args(words);
            match parse(&build_command(&args, "c")).unwrap() {
                Commands::Build { start, .. } => (start.opts.make_jobs, args.topology.job_name),
                _ => unreachable!(),
            }
        };
        assert_eq!(jobs_of(&["s.sim", "--submit", "-j", "8"]), (Some(MakeJobs::Count(8)), None));
        assert_eq!(jobs_of(&["s.sim", "--run", "--make-jobs", "max"]), (Some(MakeJobs::Max), None));
        assert_eq!(jobs_of(&["s.sim", "-j", "2"]), (Some(MakeJobs::Count(2)), None));
        assert_eq!(jobs_of(&["s.sim", "--submit", "-J", "x"]), (None, Some("x".to_owned())));
        assert_eq!(jobs_of(&["s.sim"]), (None, None));
    }

    impl ImportArgs {
        fn try_parse_from_args(words: &[&str]) -> ImportArgs {
            let mut argv = vec!["cactup".to_owned(), "import".to_owned()];
            argv.extend(words.iter().map(|w| w.to_string()));
            match Args::try_parse_from(argv).unwrap().command {
                Commands::Import(args) => args,
                _ => unreachable!(),
            }
        }
    }

    /// A `.sim` as the Cactarium stores it, stamped with `stamp`.
    pub(crate) fn stamped(stamp: &str) -> String {
        let mut sim = sim_without_hints();
        sim.sim.cactarium = Some(stamp.parse().unwrap());
        sim.to_toml().unwrap()
    }

    /// A `.sim` that names no Cactarium version.
    pub(crate) fn unstamped() -> String {
        sim_without_hints().to_toml().unwrap()
    }

    /// A `/resolve` answer for a sim entry.
    fn resolved(slug: &str, version: &str, sim: Option<&str>, newer_without_sim: Option<&str>) -> String {
        let mut answer = serde_json::json!({
            "slug": slug, "kind": "sim", "url": "https://github.com/et/sims", "contact": null,
            "version": version, "track": null, "tag": format!("sim-v{version}"), "commit": "c", "tree": null,
            "yanked": false, "yank-reason": null, "state": "ok", "curated": false, "published": 0
        });
        if let Some(sim) = sim {
            answer["sim"] = sim.into();
        }
        if let Some(newer) = newer_without_sim {
            answer["newer-without-sim"] = newer.into();
        }
        answer.to_string()
    }

    /// A fake Cactarium: `/resolve` answers with `answer(req)`, a download
    /// serves `file(path)` with its own sha256.
    fn cactarium(
        requests: usize,
        answer: impl Fn(&str) -> (&'static str, String) + Send + 'static,
        file: impl Fn(&str) -> String + Send + 'static,
    ) -> (crate::cactarium::Client, std::thread::JoinHandle<Vec<String>>) {
        let (base, heads) = crate::fetch::download::test_server::serve_with(requests, move |path| {
            if path.contains("/resolve?") {
                let (status, body) = answer(path);
                (status, "api-generation: 1\r\n".into(), body.into_bytes())
            } else {
                let body = file(path).into_bytes();
                let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &body);
                let sha: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
                ("200 OK", format!("x-sha256: {sha}\r\n"), body)
            }
        });
        (crate::cactarium::Client::at(&base), heads)
    }

    /// The cache keeps an entry that is one sim where it always did, and each
    /// sim of a multi-sim entry apart from the others of its version.
    #[test]
    fn the_sim_cache_keys_on_the_sim_name() {
        let root = Path::new("/c/simfiles");
        let v = |v: &str| v.parse::<TrackedVersion>().unwrap();
        assert_eq!(simfile_cache_path(root, "max", "bbh", None, &v("1.0.0")), root.join("max/bbh/1.0.0.sim"));
        assert_eq!(
            simfile_cache_path(root, "et", "cottonmouth-sims", Some("qc0-z4c"), &v("2026.5.1")),
            root.join("et/cottonmouth-sims/qc0-z4c/2026.5.1.sim")
        );
        assert_ne!(
            simfile_cache_path(root, "et", "s", Some("a"), &v("1.0.0")),
            simfile_cache_path(root, "et", "s", Some("b"), &v("1.0.0"))
        );
    }

    /// Only one version, written out in full, is exact: a range is not,
    /// whatever its digits, and neither is anything that is no version.
    #[test]
    fn only_one_version_is_exact() {
        use cactup_ecosystem_common::slug::Slug;
        let exact = |s: &str| exact_version(&s.parse::<Slug>().unwrap()).map(|v| v.to_string());
        assert_eq!(exact("et/s/a@=1.0.1").as_deref(), Some("1.0.1"));
        assert_eq!(exact("et/s/a@1.0.1").as_deref(), Some("1.0.1"));
        assert_eq!(exact("max/bbh@=2026.5.1").as_deref(), Some("2026.5.1"));
        for range in ["et/s/a@^1.0.0", "et/s/a@~2026.5.1", "et/s/a@>=1.0.0", "et/s/a@1", "et/s/a@1.0", "et/s/a"] {
            assert_eq!(exact(range), None, "{range}");
        }
        // Nothing typed can name a cache path that is not a version.
        assert!("o/n@=/abs/a.b.c".parse::<Slug>().map(|s| exact_version(&s)).ok().flatten().is_none());
    }

    /// `import` takes a file when one is there; otherwise a reference-like
    /// argument is a published sim (a sim name may carry a dot, and a typo
    /// like `Max/bbh` gets the reference's error), and a path-like one is a
    /// missing file.
    #[test]
    fn import_tells_a_file_from_a_published_sim() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("et/s");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();
        assert_eq!(source_of(&file).unwrap(), Source::File(file.canonicalize().unwrap()));
        let missing = tmp.path().join("bbh.sim");
        let err = format!("{:#}", source_of(&missing).unwrap_err());
        assert!(err.starts_with("Failed to find"), "{err}");
        for published in ["max/bbh", "max/bbh@1", "et/cottonmouth-sims/qc0-z4c@~2026.5", "et/x/bhns.gw230529", "max/a.sims", "Max/bbh"] {
            assert_eq!(source_of(Path::new(published)).unwrap(), Source::Published(published.to_owned()), "{published}");
        }
        for path in ["runs/qc0.sim", "bbh.sim", "./x/y", "/no/such/path", "~/x/y", "bbh"] {
            assert!(source_of(Path::new(path)).is_err(), "{path}");
        }
        // A mistyped reference says what is wrong with it.
        let nobody = crate::cactarium::Client::at("http://127.0.0.1:9");
        let err = format!("{:#}", fetch_published(&nobody, tmp.path(), "Max/bbh").unwrap_err());
        assert!(err.contains("neither a file nor a sim"), "{err}");
    }

    /// One sim of a multi-sim entry: the 3-part request goes to /resolve,
    /// the sim to the download as `?sim=`, the file is cached under the sim's
    /// name, and a newer version that dropped the sim is warned about, not
    /// refused. Two sims of one version are two files.
    #[test]
    fn one_sim_of_a_multi_sim_entry_is_fetched_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, heads) = cactarium(
            4,
            |path| {
                let sim = if path.contains("qc0-z4c") { "qc0-z4c" } else { "mag-tov" };
                let newer = (sim == "qc0-z4c").then_some("2026.6.0");
                ("200 OK", resolved(&format!("et/cottonmouth-sims/{sim}@2026.5.1"), "2026.5.1", Some(sim), newer))
            },
            |path| stamped(&format!("et/cottonmouth-sims/{}@2026.5.1", path.rsplit('=').next().unwrap())),
        );
        let qc0 = fetch_published(&client, tmp.path(), "et/cottonmouth-sims/qc0-z4c@~2026.5").unwrap();
        assert_eq!(qc0, tmp.path().join("et/cottonmouth-sims/qc0-z4c/2026.5.1.sim"));
        let text = std::fs::read_to_string(&qc0).unwrap();
        assert!(text.starts_with("format = 4\n"), "a 3-part stamp is format 4: {text}");
        let read = SimFile::from_toml(&text).unwrap();
        assert_eq!(read.sim.cactarium.unwrap().as_sim().unwrap().unwrap().as_str(), "qc0-z4c");

        let mag = fetch_published(&client, tmp.path(), "et/cottonmouth-sims/mag-tov").unwrap();
        assert_eq!(mag, tmp.path().join("et/cottonmouth-sims/mag-tov/2026.5.1.sim"));
        assert_ne!(std::fs::read_to_string(&mag).unwrap(), text);

        let heads = heads.join().unwrap();
        assert!(heads[0].starts_with("GET /api/v1/resolve?req=et/cottonmouth-sims/qc0-z4c%40~2026.5 "), "{}", heads[0]);
        assert!(heads[1].starts_with("GET /api/v1/sims/et/cottonmouth-sims/2026.5.1/download?sim=qc0-z4c "), "{}", heads[1]);
        assert!(heads[2].starts_with("GET /api/v1/resolve?req=et/cottonmouth-sims/mag-tov "), "{}", heads[2]);
        assert!(heads[3].starts_with("GET /api/v1/sims/et/cottonmouth-sims/2026.5.1/download?sim=mag-tov "), "{}", heads[3]);
    }

    /// An entry that is one sim is addressed and cached as before: no
    /// `?sim=`, and a copy downloaded before (an exact version) is used as it
    /// is, offline too.
    #[test]
    fn a_one_sim_entry_is_fetched_and_cached_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, heads) = cactarium(
            2,
            |_| ("200 OK", resolved("max/bbh@1.2.0", "1.2.0", None, None)),
            |_| stamped("max/bbh@1.2.0"),
        );
        let path = fetch_published(&client, tmp.path(), "max/bbh@1").unwrap();
        assert_eq!(path, tmp.path().join("max/bbh/1.2.0.sim"));
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("format = 1\n"));
        let heads = heads.join().unwrap();
        assert!(heads[1].starts_with("GET /api/v1/sims/max/bbh/1.2.0/download "), "{}", heads[1]);

        // Offline: nothing listens at the client's address any more.
        let gone = crate::cactarium::Client::at(client.base());
        assert_eq!(fetch_published(&gone, tmp.path(), "max/bbh@=1.2.0").unwrap(), path);
    }

    /// A Cactarium from before multi-sim entries reads the third segment as
    /// a thorn: it refuses a sim name that is no thorn name, or answers for
    /// the whole entry without naming the sim. Both fail, saying why, and
    /// nothing is downloaded.
    #[test]
    fn a_cactarium_without_multi_sim_entries_fails_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = r#"{"error":{"code":"bad-request","message":"invalid: 'et/s/qc0-z4c' is not a Cactarium reference: 'qc0-z4c' is not a thorn name"}}"#;
        let (client, _) = cactarium(1, move |_| ("400 Bad Request", refused.to_owned()), |_| unreachable!());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s/qc0-z4c").unwrap_err());
        assert!(err.contains("predates sims named as owner/name/sim") && err.contains("et/s/qc0-z4c"), "{err}");

        let (client, _) = cactarium(1, |_| ("200 OK", resolved("et/s@1.0.0", "1.0.0", None, None)), |_| unreachable!());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s/qc0@1").unwrap_err());
        assert!(err.contains("predates sims named as owner/name/sim") && err.contains("names no sim"), "{err}");
        assert!(!tmp.path().join("et").exists(), "nothing downloaded");
    }

    /// A bare `owner/name` of a multi-sim entry: the Cactarium's refusal,
    /// which lists its sims, is what the user reads.
    #[test]
    fn a_bare_multi_sim_entry_relays_its_sims() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = r#"{"error":{"code":"bad-request","message":"et/s holds several sims, each named: et/s/a, et/s/b; name one, as owner/name/sim[@range]"}}"#;
        let (client, _) = cactarium(1, move |_| ("400 Bad Request", refused.to_owned()), |_| unreachable!());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s@1").unwrap_err());
        assert_eq!(err, "et/s holds several sims, each named: et/s/a, et/s/b; name one, as owner/name/sim[@range]");
    }

    /// The file downloaded must be stamped as the sim asked for; another is
    /// refused and not kept. A third segment that is no sim name is refused
    /// before anything is asked.
    #[test]
    fn the_file_must_be_the_sim_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _) = cactarium(
            2,
            |_| ("200 OK", resolved("et/s/a@1.0.0", "1.0.0", Some("a"), None)),
            |_| stamped("et/s/b@1.0.0"),
        );
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s/a").unwrap_err());
        assert!(err.contains("et/s/a@1.0.0") && err.contains("et/s/b@1.0.0"), "{err}");
        assert!(!tmp.path().join("et/s/a/1.0.0.sim").exists());

        let nobody = crate::cactarium::Client::at("http://127.0.0.1:9");
        let err = format!("{:#}", fetch_published(&nobody, tmp.path(), "et/s/Qc0").unwrap_err());
        assert!(err.contains("is not a sim"), "{err}");
    }

    /// A range written with a full version (`^2026.5.1`, `~2026.5.1`,
    /// `>=1.0.0`) is a range: the newest match is taken, not refused for
    /// differing from the digits typed.
    #[test]
    fn a_range_with_a_full_version_is_a_range() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _) = cactarium(
            6,
            |_| ("200 OK", resolved("et/cottonmouth-sims/qc0-z4c@2026.5.3", "2026.5.3", Some("qc0-z4c"), None)),
            |_| stamped("et/cottonmouth-sims/qc0-z4c@2026.5.3"),
        );
        let want = tmp.path().join("et/cottonmouth-sims/qc0-z4c/2026.5.3.sim");
        for req in ["^2026.5.1", "~2026.5.1", ">=2026.0.0"] {
            let got = fetch_published(&client, tmp.path(), &format!("et/cottonmouth-sims/qc0-z4c@{req}"));
            assert_eq!(got.unwrap_or_else(|e| panic!("{req}: {e:#}")), want);
        }
        // Exact is still exact.
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/cottonmouth-sims/qc0-z4c@=2026.5.1").unwrap_err());
        assert!(err.contains("answered with version 2026.5.3"), "{err}");
    }

    /// A current Cactarium refuses a third segment that is no thorn name for
    /// a package, in words close to an old one's: that is relayed as is,
    /// not taken for a registry that predates multi-sim entries.
    #[test]
    fn a_current_cactariums_refusal_is_relayed() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = r#"{"error":{"code":"bad-request","message":"invalid: 'qc0-z4c' in 'et/carpetx/qc0-z4c' is not a thorn name"}}"#;
        let (client, _) = cactarium(1, move |_| ("400 Bad Request", refused.to_owned()), |_| unreachable!());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/carpetx/qc0-z4c").unwrap_err());
        assert_eq!(err, "invalid: 'qc0-z4c' in 'et/carpetx/qc0-z4c' is not a thorn name");
    }

    /// One sim of several, downloaded before: used again without a second
    /// download, and, for an exact version, when the Cactarium cannot be
    /// reached.
    #[test]
    fn one_sim_of_several_is_cached_and_used_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, heads) = cactarium(
            3,
            |_| ("200 OK", resolved("et/s/a@1.0.0", "1.0.0", Some("a"), None)),
            |_| stamped("et/s/a@1.0.0"),
        );
        let path = fetch_published(&client, tmp.path(), "et/s/a@1").unwrap();
        assert_eq!(path, tmp.path().join("et/s/a/1.0.0.sim"));
        assert_eq!(fetch_published(&client, tmp.path(), "et/s/a@=1.0.0").unwrap(), path);
        let heads = heads.join().unwrap();
        assert!(heads[2].contains("/resolve?"), "the second import asked only /resolve: {}", heads[2]);
        let gone = crate::cactarium::Client::at(client.base());
        assert_eq!(fetch_published(&gone, tmp.path(), "et/s/a@=1.0.0").unwrap(), path);
        // Offline, another sim of that version is not that copy.
        assert!(fetch_published(&gone, tmp.path(), "et/s/b@=1.0.0").is_err());
    }

    /// An answer about another sim than the one asked for, or naming a sim
    /// when none was asked for, is refused before anything is downloaded.
    #[test]
    fn an_answer_about_another_sim_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _) =
            cactarium(2, |_| ("200 OK", resolved("et/s/b@1.0.0", "1.0.0", Some("b"), None)), |_| unreachable!());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s/a").unwrap_err());
        assert!(err.contains("the sim a of et/s, and it answered with b"), "{err}");
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s@1").unwrap_err());
        assert!(err.contains("with the sim b; name the sim you want, as et/s/b"), "{err}");
        assert!(!tmp.path().join("et").exists(), "nothing downloaded");
    }

    /// A file with no stamp is not the sim asked for: refused, and not
    /// left in the cache.
    #[test]
    fn a_sim_without_a_stamp_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (client, _) =
            cactarium(2, |_| ("200 OK", resolved("et/s/a@1.0.0", "1.0.0", Some("a"), None)), |_| unstamped());
        let err = format!("{:#}", fetch_published(&client, tmp.path(), "et/s/a").unwrap_err());
        assert!(err.contains("does not say which sim it is"), "{err}");
        assert!(!tmp.path().join("et/s/a/1.0.0.sim").exists());
    }
}
