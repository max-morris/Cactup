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
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::sim::{Input, SimFile};
use clap::Parser;
use colored::Colorize;
use indexmap::IndexMap;
use std::io::IsTerminal;
use std::path::PathBuf;

pub fn dispatch(ctx: &Ctx, args: ImportArgs) -> Res<()> {
    let path = match std::fs::canonicalize(&args.simfile) {
        Ok(path) => path,
        // Not a file: a published sim, `owner/name[@N]`.
        Err(_) if args.simfile.to_string_lossy().contains('/') && !args.simfile.to_string_lossy().contains(".sim") => {
            download_published(ctx, &args.simfile.to_string_lossy())?
        }
        Err(e) => return Err(e).with_context(|| format!("Failed to find '{}'", args.simfile.display())),
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
    let knobs = settle_inputs(ctx, &sim_file.inputs, &args.inputs, interactive)?;

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
    let mut build = vec!["build".to_owned(), config.clone()];
    if let Some(flavor) = &args.flavor {
        build.extend(["--flavor".to_owned(), flavor.clone()]);
    }
    let Commands::Build { start, .. } = parse(&build)? else { unreachable!("parsed as build") };
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
    let topology = topology(&sim_file, &args);
    if args.submit || args.run {
        let verb = if args.submit { "submit" } else { "run" };
        let mut command = vec!["sim".to_owned(), verb.to_owned(), sim_name.clone()];
        command.extend(topology);
        match parse(&command)? {
            Commands::Sim(SimCommand::Submit(start)) => crate::sim::start::submit(&ctx, start),
            Commands::Sim(SimCommand::Run(run)) => crate::sim::start::run(&ctx, run),
            _ => unreachable!("parsed as sim submit/run"),
        }
    } else {
        let mut command = format!("cactup -I {alias} sim submit {sim_name}");
        for word in topology {
            command.push(' ');
            command.push_str(&word);
        }
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
    for entry in candidates {
        let inst = Installation::new(entry.alias.clone(), &entry.path);
        if differences(&inst, sim)?.is_empty() {
            return Ok(Some(entry.alias.clone()));
        }
    }
    Ok(None)
}

/// How installation `inst`'s code differs from what `sim` records, one line
/// each (see [`matching_installation`]); empty when it has exactly that code.
fn differences(inst: &Installation, sim: &SimFile) -> Res<Vec<String>> {
    use crate::fetch::git::normalize_url;
    let wanted = crate::thornlist::parse(&sim.thornlist.text).context("The file's thornlist does not parse")?;
    let live_path = inst.live_thornlist_to_read();
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
    let repos_dir = inst.cactus_root().join("repos");
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

/// Download published sim `req` (`owner/name`, the newest version, or
/// `owner/name@N`) from the Cactarium (§8.10) to
/// `~/.cactup/simfiles/<owner>/<name>/<N>.sim`, checked against the hash
/// the registry sends.
fn download_published(ctx: &Ctx, req: &str) -> Res<PathBuf> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = req.parse().map_err(|e| anyhow!("\"{req}\" is neither a file nor a sim (owner/name[@N]): {e}"))?;
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let number = match slug.number().map_err(|e| anyhow!("{e}"))? {
        Some(n) => n,
        None => {
            let resolved = client.resolve(req)?;
            if resolved.kind != "sim" {
                bail!("{} is a {}, not a sim", resolved.slug, resolved.kind);
            }
            resolved.version.version.parse().map_err(|_| anyhow!("The Cactarium named sim version \"{}\"", resolved.version.version))?
        }
    };
    let (owner, name) = (slug.owner.as_str(), slug.name.as_str());
    let dest = crate::CACTUP_ROOT.join("simfiles").join(owner).join(name).join(format!("{number}.sim"));
    if !dest.is_file() {
        client.download_sim(owner, name, number, &dest)?;
        println!("Downloaded {owner}/{name}@{number} from {}", client.base());
    }
    Ok(dest)
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
pub fn publish(ctx: &Ctx, file: &std::path::Path, target: &str) -> Res<()> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = target.parse().map_err(|e| anyhow!("\"{target}\" is not owner/name: {e}"))?;
    if slug.at.is_some() || slug.thorn.is_some() {
        bail!("Publish to owner/name; the Cactarium numbers the version");
    }
    let text = std::fs::read_to_string(file).with_context(|| format!("Failed to read '{}'", file.display()))?;
    let sim = SimFile::from_toml(&text).map_err(|e| anyhow!("'{}': {e}", file.display()))?;
    let missing = sim.unspecified();
    if !missing.is_empty() {
        let what: Vec<String> = missing.iter().map(ToString::to_string).collect();
        bail!("'{}' is not fully specified, so it cannot be published: {}", file.display(), what.join("; "));
    }
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let published = client.publish_sim(slug.owner.as_str(), slug.name.as_str(), text.into_bytes())?;
    println!("Published {} ({}): {}", published.slug.bold(), published.visibility, published.url);
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
    if slug.at.is_some() || slug.thorn.is_some() {
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
    println!("Published {} ({}): {}", published.slug.bold(), published.visibility, published.url);
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
    if slug.at.is_some() || slug.thorn.is_some() {
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
    Ok(())
}

/// The knob values for the parfile's inputs (§5.1): from `--input`, else a
/// download of the input's URL, else asked for. An input left empty while
/// the user's global knob has a value takes the global value at submit (the
/// simulation records nothing for it); one with neither is an error now,
/// not at submit.
fn settle_inputs(
    ctx: &Ctx,
    inputs: &[Input],
    given: &[String],
    interactive: bool,
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
    let mut knobs = IndexMap::new();
    let mut missing = Vec::new();
    for input in inputs {
        let what = input.description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
        let value = if let Some(value) = given_values.shift_remove(&input.knob) {
            Some(value)
        } else if let Some(url) = &input.url {
            Some(fetch_input(url, input.sha256.as_deref())?.display().to_string())
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

/// Download an input to `~/.cactup/inputs/<sha256>/<file name>`, shared by
/// every simulation that needs the same bytes, and checked against the
/// `.sim`'s hash when it records one. Already there: not downloaded again.
fn fetch_input(url: &str, sha256: Option<&str>) -> Res<PathBuf> {
    let inputs = crate::CACTUP_ROOT.join("inputs");
    let file_name = url.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("input").to_owned();
    if let Some(hash) = sha256 {
        let cached = inputs.join(hash.to_ascii_lowercase()).join(&file_name);
        if cached.is_file() {
            return Ok(cached);
        }
    }
    std::fs::create_dir_all(&inputs)
        .with_context(|| format!("Failed to create '{}'", inputs.display()))?;
    let mut temp =
        tempfile::NamedTempFile::new_in(&inputs).context("Failed to create a download file")?;
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let mut item = progress.add_child(file_name.clone());
    let downloaded = crate::fetch::download::download_to(
        crate::fetch::download::client(),
        url,
        temp.as_file_mut(),
        &mut item,
    );
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    downloaded?;
    let actual = crate::update::sha256_hex(temp.path())?;
    match sha256 {
        Some(expected) if !expected.eq_ignore_ascii_case(&actual) => {
            bail!("The download of {url} has SHA-256 {actual}, but the .sim records {expected}")
        }
        None => println!(
            "{}",
            format!("The .sim records no hash for {url}, so the download could not be checked").yellow()
        ),
        Some(_) => {}
    }
    let dir = inputs.join(&actual);
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    let dest = dir.join(&file_name);
    temp.persist(&dest)
        .with_context(|| format!("Failed to save '{}'", dest.display()))?;
    println!("Downloaded {url} to '{}'", dest.display());
    Ok(dest)
}

/// The topology flags for `sim submit`/`run`: each one given to import, else
/// the `.sim`'s suggestion.
fn topology(sim: &SimFile, args: &ImportArgs) -> Vec<String> {
    let hints = sim.run.as_ref();
    let mut flags = Vec::new();
    let mut add = |flag: &str, value: Option<String>| {
        if let Some(value) = value {
            flags.extend([flag.to_owned(), value]);
        }
    };
    add("-T", args.tasks.or(hints.and_then(|h| h.tasks)).map(|n| n.to_string()));
    add("-c", args.cpus.or(hints.and_then(|h| h.cpus_per_task)).map(|n| n.to_string()));
    add("-G", hints.and_then(|h| h.gpus_per_task).map(|n| n.to_string()));
    add("-w", args.wall_time.clone().or(hints.and_then(|h| h.walltime.clone())));
    add("-q", args.queue.clone());
    flags
}

/// Parse a cactup command line (without the program name), so the steps
/// import chains get every default exactly as typed commands would.
fn parse(words: &[String]) -> Res<Commands> {
    let argv = std::iter::once("cactup".to_owned()).chain(words.iter().cloned());
    Args::try_parse_from(argv).map(|a| a.command).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
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
        std::fs::create_dir_all(inst.live_thornlist().parent().unwrap()).unwrap();
        std::fs::write(inst.live_thornlist(), &live).unwrap();

        assert_eq!(differences(&inst, &sim("Makefile", v1)).unwrap(), Vec::<String>::new());
        let at_v2 = differences(&inst, &sim("Makefile", v2)).unwrap();
        assert!(at_v2.len() == 1 && at_v2[0].starts_with("carpetx is at "), "{at_v2:?}");
        let missing = differences(&inst, &sim("README", v1)).unwrap();
        assert_eq!(missing, ["README is not in its thornlist"]);
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
            },
            db: crate::database::Db::in_dir(dir),
        }
    }

    fn input(knob: &str) -> Input {
        Input { knob: knob.into(), description: Some("initial data".into()), url: None, sha256: None }
    }

    /// Inputs are settled before anything long runs: `--input` sets the
    /// simulation's own value; one left unset falls back to a global knob the
    /// user already has (recording nothing); one with neither fails now.
    #[test]
    fn inputs_come_from_flags_else_the_global_knob_else_fail_early() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path());
        let inputs = [input("kadath-initial-data"), input("eos-table")];

        let err = format!("{:#}", settle_inputs(&ctx, &inputs, &[], false).unwrap_err());
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
            settle_inputs(&ctx, &inputs, &["kadath-initial-data=/id/bhns.info".to_owned()], false)
                .unwrap();
        assert_eq!(knobs.len(), 1, "the global eos-table is used as is, not copied: {knobs:?}");
        assert_eq!(knobs["kadath-initial-data"], "/id/bhns.info");

        let err =
            format!("{:#}", settle_inputs(&ctx, &inputs, &["nope=1".to_owned()], false).unwrap_err());
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
        assert_eq!(topology(&sim, &args), ["-T", "64", "-c", "8", "-w", "48:00:00", "-q", "checkpt"]);
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
}
