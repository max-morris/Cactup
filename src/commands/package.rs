//! `cactup package init` (D5): a `cactarium.toml` for a repo of the installation, written for
//! review from what is installed and built; and `--publish`, which writes nothing and registers
//! the manifest at the repo's newest pushed release tag (or has Cactarium check the package for
//! new releases when it is registered).
//!
//! Nothing a heuristic finds is ever written as a claim, only as a TODO comment. A dependency is
//! written for a requirement of the repo's CCL that exactly one installed thorn meets (the one
//! built, when the build has it), in a package Cactarium serves at the version the active config
//! was built with, and the range is floored there. A fork is written only when the repo's history
//! contains a version's frozen commit and the repo has every thorn of that version. The text comes
//! from a template (TOML serialization carries no comments) and must parse as a manifest before
//! anything is written.
// Plan §4.7, misfits 1, 10, 11, 12 and 19, decisions 1 and 8, the P7 row of §7.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use cactup_ecosystem_common::api::{PinInfo, PinQuery, RegisterRequest, Registered};
use cactup_ecosystem_common::ccl::{self, Need, ThornFacts};
use cactup_ecosystem_common::manifest::{ForkOf, Manifest, PackageSection, PACKAGE_FILE};
use cactup_ecosystem_common::name::{Name, Owner};
use cactup_ecosystem_common::slug::{is_commit_id, Slug};
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use colored::Colorize;

use super::resolution::Registry;
use super::Ctx;
use crate::args::PackageInitArgs;
use crate::cactarium::Client;
use crate::deps::{Facts, PkgId};
use crate::installation::{Installation, InstallationMeta};
use crate::thornlist::{self, Component, ComponentType, Thornlist};
use crate::Res;

/// The implementation every Cactus driver provides (Carpet, PUGH, CarpetX).
const DRIVER: &str = "driver";

/// The headline over reading the repo's sources.
const SOURCES: &str = "read the repo's sources";

/// The headline over reading the history of the packages the repo may fork.
const HISTORY: &str = "read the repo's history";

/// `cactup package init [REPO]`, or with `--publish` the registration of its manifest.
pub fn init(ctx: &Ctx, args: &PackageInitArgs) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let meta = inst.meta()?;
    let cactus = inst.cactus_root()?;
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .map_err(|e| anyhow!("This installation has no pristine thornlist: {e}"))?;
    let base = thornlist::parse(&pristine)?;
    let live = thornlist::parse(&super::add::compose(&pristine, &meta.added)?)?;
    let repo = which_repo(&live, &cactus, args.repo.as_deref())?;
    let dir = cactus.join("repos").join(&repo);
    // Anonymous: what a shared manifest rests on must be reviewed.
    let client = Client::new(&ctx.db.read()?);
    if args.publish {
        return publish(&client, &live, &repo, &dir, args.owner.as_deref());
    }
    let file = dir.join(PACKAGE_FILE);
    if file.exists() && !args.force {
        bail!("'{}' exists.\nPass -f to overwrite it.", file.display());
    }
    let name = package_name(args.name.as_deref(), &repo)?;
    let config = meta.active_config()?.to_owned();
    let built_meta = crate::build::ConfigMeta::load(&cactus, &config)?
        .ok_or_else(|| anyhow!("Config {config} is not built.\nBuild it with `cactup build {config}`."))?;
    let built = built_thorns(&cactus, &config)?;
    let owner = owner(&client, args.owner.as_deref())?;
    let input = Input {
        inst: &inst,
        meta: &meta,
        cactus: &cactus,
        base: &base,
        live: &live,
        repo: &repo,
        dir: &dir,
        config: &config,
        sources: built_meta.sources.as_ref(),
        built: &built,
    };
    let generated = generate(&client, &input, &name, &owner, args.exact)?;
    // Refused unless it reads back as the manifest it means to be.
    Manifest::from_repo_file(PACKAGE_FILE, &generated.text)
        .map_err(|e| anyhow!("The cactarium.toml written for {repo} does not parse ({e}); nothing was written"))?;
    about_to_write()?;
    crate::format::write_atomic(&file, &generated.text)?;
    println!("Wrote '{}'.", file.display());
    match generated.todos {
        0 => {}
        1 => println!("{}", "Resolve the TODO in it.".yellow()),
        n => println!("{}", format!("Resolve the {n} TODOs in it.").yellow()),
    }
    if generated.shallow {
        let [said, fetch, again] = shallow_notice(&repo, &dir);
        println!("{}", said.yellow());
        println!("{fetch}");
        println!("{again}");
    }
    println!("Commit it, tag a release and push both.");
    println!("Then register it with `cactup package init {repo} --publish`.");
    Ok(())
}

/// What `init` says of a shallow clone: what it means, and the two commands past it.
fn shallow_notice(repo: &str, dir: &Path) -> [String; 3] {
    [
        format!("The clone of repo {repo} is shallow: its history cannot show a fork."),
        format!("Fetch its history with `git -C {} fetch --unshallow`.", shell_word(&dir.display().to_string())),
        format!("Then run `cactup package init {repo} -f`."),
    ]
}

/// Runs just before a generated file is written (tests: a Ctrl-C that late).
#[cfg(test)]
pub(crate) static BEFORE_WRITE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// The last check before `init` writes: a Ctrl-C that arrived after the last request fails the
/// command with nothing written, rather than writing and then failing.
pub(crate) fn about_to_write() -> Res<()> {
    #[cfg(test)]
    if let Some(hook) = BEFORE_WRITE.get() {
        hook();
    }
    if gix::interrupt::is_triggered() {
        bail!("Interrupted");
    }
    Ok(())
}

/// What [`generate`] reads.
struct Input<'a> {
    inst: &'a Installation,
    meta: &'a InstallationMeta,
    cactus: &'a Path,
    base: &'a Thornlist,
    live: &'a Thornlist,
    repo: &'a str,
    dir: &'a Path,
    config: &'a str,
    /// The source state of each repo when the config was built.
    sources: Option<&'a BTreeMap<String, String>>,
    /// The thorns the config built, as `Arrangement/Thorn`.
    built: &'a BTreeSet<String>,
}

/// The manifest's text, how many TODOs it has, and whether the clone was too shallow to show a
/// fork.
pub(crate) struct Generated {
    pub text: String,
    pub todos: usize,
    pub shallow: bool,
}

/// The repo named on the command line (a repo of the live list, or a path inside one), else the
/// one holding the current directory.
fn which_repo(live: &Thornlist, cactus: &Path, named: Option<&str>) -> Res<String> {
    let git: BTreeSet<&str> =
        live.components().iter().filter(|c| c.ty == ComponentType::Git).map(|c| c.repo.as_str()).collect();
    let repos_dir = cactus.join("repos");
    let name = match named {
        Some(n) if git.contains(n) => n.to_owned(),
        Some(n) => repo_holding(&repos_dir, Path::new(n)).ok_or_else(|| anyhow!("The installation has no repo {n}."))?,
        None => {
            let here = std::env::current_dir().map_err(|e| anyhow!("Failed to read the current directory: {e}"))?;
            repo_holding(&repos_dir, &here).ok_or_else(|| {
                anyhow!("The current directory is in no repo of the installation.\nName one: `cactup package init <repo>`.")
            })?
        }
    };
    if !git.contains(name.as_str()) {
        bail!("The installation's thornlist fetches no git repo {name}.");
    }
    if !repos_dir.join(&name).is_dir() {
        bail!("Repo {name} is not on disk.\nFetch it with `cactup inst refetch`.");
    }
    Ok(name)
}

/// The repo under `repos_dir` that `path` (followed through links) is in.
fn repo_holding(repos_dir: &Path, path: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    let repos = std::fs::canonicalize(repos_dir).ok()?;
    let rest = real.strip_prefix(&repos).ok()?;
    rest.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())
}

fn package_name(given: Option<&str>, repo: &str) -> Res<String> {
    let name = given.map_or_else(|| repo.to_ascii_lowercase(), str::to_owned);
    if let Err(e) = name.parse::<Name>() {
        match given {
            Some(_) => bail!("\"{name}\" is not a package name: {e}"),
            None => bail!("\"{name}\" is not a package name: {e}\nName the package with --name."),
        }
    }
    Ok(name)
}

/// `given`, checked as a Cactarium owner.
fn owner_given(given: &str) -> Res<String> {
    given.parse::<Owner>().map_err(|e| anyhow!("\"{given}\" is not a Cactarium owner: {e}"))?;
    Ok(given.to_owned())
}

/// The owner to write: `given`, else the user the stored token belongs to.
pub(crate) fn owner(client: &Client, given: Option<&str>) -> Res<String> {
    if let Some(o) = given {
        return owner_given(o);
    }
    let reg = Registry::new(client, ["your user"]);
    let who = reg.one("your user", "asking Cactarium", |c| c.whoami());
    reg.finish();
    Ok(who.map_err(|e| anyhow!("{e:#}\nOr name the owner with --owner."))?.user)
}

/// The thorns the config built: its `ThornList`, read as the flesh reads it.
fn built_thorns(cactus: &Path, config: &str) -> Res<BTreeSet<String>> {
    let path = cactus.join("configs").join(config).join("ThornList");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        anyhow!(
            "Config {config} has no ThornList.\nA build with --virtual-executable writes none.\n\
             Build the config with `cactup build {config}`."
        )
    })?;
    Ok(text
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with('#') && !l.starts_with('!'))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.contains('/'))
        .map(str::to_owned)
        .collect())
}

/// A thorn of the installation outside the repo.
struct Installed {
    checkout: String,
    thorn: String,
    repo: String,
    facts: ThornFacts,
    built: bool,
}

/// A thorn of the repo.
struct Own {
    name: String,
    /// Where the installation checks it out (`Arrangement/Thorn`), if it does.
    checkout: Option<String>,
    /// Its arrangement as the manifest decides it: its folder's, else the package's.
    arrangement: Option<String>,
    /// Its directory, relative to the repo's root.
    rel: PathBuf,
    facts: ThornFacts,
    /// Its sources' `#include`s: the file (relative to the thorn) and the name as written.
    includes: Vec<(String, String)>,
}

/// What Cactarium says each repo of the installation is, by repo name.
type Pins = BTreeMap<String, PinInfo>;

/// A thorn that meets a requirement, and what it is part of.
#[derive(Clone)]
struct Candidate {
    checkout: String,
    thorn: String,
    repo: String,
}

/// Why a requirement got no dependency.
enum Gap {
    /// No installed thorn meets it.
    Unmet,
    /// Its one provider is in a repo that is no registered package.
    Unregistered(Candidate),
    /// Its one provider is at a commit no version froze.
    Unfrozen(Candidate, String, String),
    /// Its one provider is of the repo's own package (another clone of it).
    Own(Candidate, String),
    /// Its one provider is at a version cactup cannot read.
    Unreadable(Candidate, String, String),
    /// Several thorns of one package meet it.
    SeveralInOne(String, Vec<Candidate>),
    /// Thorns of several packages (or repos) meet it.
    AnyOf(Vec<Candidate>),
}

/// The requirements that got no dependency, by requirement and candidates: why, and the
/// thorns of the repo needing them.
type Gaps = BTreeMap<(u8, String, String), (Need, Gap, BTreeSet<String>)>;

/// The dependencies found, by package and version: the version read, and each thorn of the
/// package with the repo's thorns needing it.
type Edges = BTreeMap<(String, String), (TrackedVersion, BTreeMap<String, BTreeSet<String>>)>;

/// A version of a package: its version, its frozen commit and its thorns.
type Frozen = (TrackedVersion, gix::ObjectId, Vec<String>);

/// The candidates the built thorns give, else every installed thorn (a requirement only an
/// unbuilt thorn meets is met by it).
fn candidates_for(installed: &[Installed], need: &Need) -> Vec<Candidate> {
    let meeting = |i: &&Installed| ccl::meets(&i.thorn, &i.facts, need);
    let mut found: Vec<&Installed> = installed.iter().filter(|i| i.built).filter(meeting).collect();
    if found.is_empty() {
        found = installed.iter().filter(meeting).collect();
    }
    found.iter().map(|i| Candidate { checkout: i.checkout.clone(), thorn: i.thorn.clone(), repo: i.repo.clone() }).collect()
}

/// The manifest for the repo described by `input`.
fn generate(client: &Client, input: &Input, name: &str, owner: &str, exact: bool) -> Res<Generated> {
    let repo = input.repo;
    let root = input.live.root().to_owned();
    let flesh = (input.live.components().iter()).any(|c| c.repo == repo && crate::fetch::is_flesh(&root, c));
    let mine: Vec<&Component> = (input.live.components().iter())
        .filter(|c| c.repo == repo && crate::sim::trim::is_arrangement_thorn(input.live, c))
        .collect();

    // The installation's thorns, read from the tree.
    let local = super::resolution::local_facts(input.live, input.cactus)?;
    let installed: Vec<Installed> = (input.live.components().iter())
        .filter(|c| c.repo != repo && crate::sim::trim::is_arrangement_thorn(input.live, c))
        .filter_map(|c| {
            let facts = local.get(&c.checkout).cloned().flatten()?;
            Some(Installed {
                checkout: c.checkout.clone(),
                thorn: thorn_of(&c.checkout).to_owned(),
                repo: c.repo.clone(),
                facts,
                built: input.built.contains(&c.checkout),
            })
        })
        .collect();

    // The repo's thorns.
    let (mut own, layout) = if flesh { (Vec::new(), Layout::default()) } else { own_thorns(input.dir, &mine) };
    if !flesh && own.is_empty() {
        bail!("Repo {repo} has no thorns (no interface.ccl).");
    }

    // Their sources, then which package each repo is.
    let repos = git_repos(input);
    let names: Vec<&str> =
        [SOURCES].into_iter().chain(own.iter().map(|t| t.name.as_str())).chain(repos.iter().map(|c| c.repo.as_str())).collect();
    let reg = Registry::new(client, names);
    let asked = (|| -> Res<(Vec<Scanned>, Pins)> {
        let scanned = reg.local_under(SOURCES, &own, "thorns", |t| t.name.clone(), "reading sources", |t| {
            scan_one(input.dir, t)
        })?;
        // Where each clone is, for the repo itself and any repo the build recorded nothing for.
        let built = |c: &&Component| built_commit(input, c);
        let unrecorded: Vec<&Component> = repos.iter().copied().filter(|c| built(c).is_none()).collect();
        let heads = reg.local(&unrecorded, "clones", |c| c.repo.clone(), "reading where HEAD is", |c| {
            super::add::clone_head(&input.inst.root, &root, &c.repo)
        })?;
        let heads: BTreeMap<&str, Option<String>> = unrecorded.iter().map(|c| c.repo.as_str()).zip(heads).collect();
        // A URL refused as written (credentials, a local path) is never sent.
        let pins: Vec<(String, PinQuery)> = (repos.iter())
            .filter(|c| c.url.as_deref().is_some_and(sendable))
            .filter_map(|c| {
                let commit = built(c)
                    .or_else(|| heads.get(c.repo.as_str()).cloned().flatten())
                    .or_else(|| c.commit.clone())
                    .filter(|c| is_commit_id(c))?;
                Some((c.repo.clone(), PinQuery { url: c.url.clone()?, commit }))
            })
            .collect();
        let queries: Vec<PinQuery> = pins.iter().map(|(_, q)| q.clone()).collect();
        let answers = reg.lookup(&queries)?;
        Ok((scanned, pins.iter().map(|(r, _)| r.clone()).zip(answers).collect()))
    })();
    reg.finish();
    let (scanned, infos) = asked?;
    for (t, (needs, includes)) in own.iter_mut().zip(scanned) {
        t.facts.needs_includes = needs;
        t.includes = includes;
    }

    // The packages the repo may fork, on a renderer whose layout knows their names.
    let (candidates, shallow) = fork_candidates(input, &own, flesh, &infos)?;
    let forks = match candidates.is_empty() || shallow {
        true => Vec::new(),
        false => {
            let names = [HISTORY].into_iter().chain(candidates.iter().map(PkgId::as_str));
            let mut reg = Registry::new(client, names);
            let found = forks(&mut reg, input, &candidates, &own);
            reg.finish();
            found?
        }
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# cactarium.toml for repo {}, written by `cactup package init`\n\
         # from installation {} and config {}. Resolve each TODO, then commit it.\n",
        comment(repo),
        comment(&input.inst.alias),
        comment(input.config)
    ));
    out.push_str("format = 1\n\n[package]\n");
    out.push_str(&format!("name = {}\nowners = [{}]\n", q(name), q(owner)));
    if flesh {
        out.push_str("flesh = true\n");
    }
    if let Some(r) = &layout.root {
        out.push_str(&format!("root = {}\n", q(r)));
    }
    if let Some(a) = &layout.arrangement {
        out.push_str(&format!("arrangement = {}\n", q(a)));
    }
    for todo in &layout.todos {
        out.push_str(todo);
    }
    let unbuilt: Vec<&str> = (own.iter())
        .filter(|t| t.checkout.as_ref().is_none_or(|c| !input.built.contains(c)))
        .map(|t| t.name.as_str())
        .collect();
    if !unbuilt.is_empty() {
        out.push_str(&format!(
            "# TODO: not built in config {}: {}.\n# List the ones that cannot build:\n# exclude-thorns = [{}]\n",
            comment(input.config),
            comment(&unbuilt.join(", ")),
            unbuilt.iter().map(|t| q(t)).collect::<Vec<_>>().join(", ")
        ));
    }

    // Dependencies: each requirement no thorn of the repo meets.
    let myself = infos.get(repo).and_then(|i| i.package.clone());
    let known: Vec<&ThornFacts> = installed.iter().map(|i| &i.facts).chain(own.iter().map(|o| &o.facts)).collect();
    let mut edges: Edges = BTreeMap::new();
    let mut gaps: Gaps = BTreeMap::new();
    for t in &own {
        for need in ccl::needs_of(&t.facts) {
            if own.iter().any(|o| ccl::meets(&o.name, &o.facts, &need)) {
                continue;
            }
            if !ccl::counts(&t.facts, &need, known.iter().copied()) {
                continue;
            }
            let candidates = candidates_for(&installed, &need);
            let gap = match candidates.as_slice() {
                [] => Gap::Unmet,
                [one] => match infos.get(&one.repo).and_then(|i| i.package.clone().map(|p| (p, i))) {
                    None => Gap::Unregistered(one.clone()),
                    Some((package, _)) if Some(&package) == myself.as_ref() => Gap::Own(one.clone(), package),
                    Some((package, info)) => match &info.version {
                        None => Gap::Unfrozen(one.clone(), package, info.commit.clone()),
                        Some(v) => match v.parse::<TrackedVersion>() {
                            Ok(parsed) => {
                                let (_, provided) = edges.entry((package, v.clone())).or_insert_with(|| (parsed, BTreeMap::new()));
                                provided.entry(one.thorn.clone()).or_default().insert(t.name.clone());
                                continue;
                            }
                            Err(_) => Gap::Unreadable(one.clone(), package, v.clone()),
                        },
                    },
                },
                several => {
                    let packages: BTreeSet<Option<&String>> =
                        several.iter().map(|c| infos.get(&c.repo).and_then(|i| i.package.as_ref())).collect();
                    match packages.into_iter().collect::<Vec<_>>().as_slice() {
                        [Some(p)] => Gap::SeveralInOne((*p).clone(), several.to_vec()),
                        _ => Gap::AnyOf(several.to_vec()),
                    }
                }
            };
            let (kind, key) = need.key();
            let signature = gap_signature(&gap);
            let entry = gaps.entry((kind, key, signature)).or_insert_with(|| (need.clone(), gap, BTreeSet::new()));
            entry.2.insert(t.name.clone());
        }
    }
    for ((package, _), (v, provided)) in &edges {
        // One entry per distinct set of thorns needing them.
        let mut by_needers: BTreeMap<&BTreeSet<String>, Vec<&str>> = BTreeMap::new();
        for (thorn, needers) in provided {
            by_needers.entry(needers).or_default().push(thorn);
        }
        for (needers, needs) in by_needers {
            out.push_str(&format!(
                "\n[[dependency]]\ntarget = {}\nneeds = {}\nneeded-by = {}\n",
                q(&format!("{package}@{}", range_of(v, exact))),
                qa(needs.iter().copied()),
                qa(needers.iter().map(String::as_str))
            ));
        }
    }
    for (need, gap, needers) in gaps.values() {
        out.push('\n');
        out.push_str(&gap_text(need, gap, needers, &infos, exact));
    }

    // Forks.
    if shallow {
        let packages: Vec<&str> = candidates.iter().map(PkgId::as_str).collect();
        out.push_str(&format!(
            "\n# TODO: the clone of repo {} is shallow.\n\
             # Its history cannot show whether it forks {}.\n\
             # Run `git fetch --unshallow` in it, then `cactup package init {} -f`.\n",
            comment(repo),
            comment(&packages.join(", ")),
            comment(repo)
        ));
    }
    for f in &forks {
        out.push('\n');
        if f.missing.is_empty() {
            out.push_str(&format!("[[fork-of]]\ntarget = {}\n", q(&f.target)));
        } else {
            out.push_str(&format!(
                "# TODO: this repo contains {} but lacks its thorns {}.\n\
                 # A fork-of says it is a drop-in replacement. Add them, or leave this out:\n\
                 # [[fork-of]]\n# target = {}\n",
                comment(&f.target),
                comment(&f.missing.join(", ")),
                q(&f.target)
            ));
        }
    }

    // Smells.
    for line in include_smells(&own, &installed, &infos) {
        out.push('\n');
        out.push_str(&line);
    }
    for line in driver_smells(&own, &installed, &infos) {
        out.push('\n');
        out.push_str(&line);
    }
    if let Some(line) = flesh_fork(input, &infos, exact) {
        out.push('\n');
        out.push_str(&line);
    }

    // The installation's overrides are about other packages' claims: information only.
    if !input.meta.claim_overrides.is_empty() {
        out.push_str(&format!("\n# Claim overrides of installation {}:\n", comment(&input.inst.alias)));
        for (key, o) in &input.meta.claim_overrides {
            match &o.note {
                Some(note) => out.push_str(&format!("#   {}  ({})\n", comment(key), comment(note))),
                None => out.push_str(&format!("#   {}\n", comment(key))),
            }
        }
    }
    let todos = out.lines().filter(|l| l.starts_with("# TODO:")).count();
    Ok(Generated { text: out, todos, shallow })
}

/// The commit the config was built from for the repo of `c`, when the build recorded one; never
/// for the repo itself, which is read where its clone is.
fn built_commit(input: &Input, c: &Component) -> Option<String> {
    if c.repo == input.repo {
        return None;
    }
    input.sources.and_then(|s| s.get(&c.repo)).map(|s| crate::fetch::committed(s).to_owned())
}

/// Whether `url` may go to Cactarium: not when it is refused as written (a credential in it, a
/// local path, a form nobody else can fetch).
fn sendable(url: &str) -> bool {
    use cactup_ecosystem_common::url::{shareability, Shareability};
    matches!(shareability(url), Shareability::Shareable | Shareability::Ssh { https: Some(_) })
}

/// Text for a comment line: on one line, whatever it came with (a server's answer or a hand
/// edit could hold a newline, which would start a line of TOML the parse check would accept).
pub(crate) fn comment(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// A string as TOML writes it, quoted and escaped.
pub(crate) fn q(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// A TOML array of strings.
pub(crate) fn qa<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    format!("[{}]", items.into_iter().map(q).collect::<Vec<_>>().join(", "))
}

/// The range a dependency on `v` is written with: `~X.Y.Z` (`track:~X.Y.Z` on a track), or
/// exactly `v` with `exact`.
fn range_of(v: &TrackedVersion, exact: bool) -> String {
    if exact {
        return TrackedReq::exactly(v).to_string();
    }
    match &v.track {
        Some(t) => format!("{t}:~{}", v.version),
        None => format!("~{}", v.version),
    }
}

fn thorn_of(checkout: &str) -> &str {
    checkout.rsplit('/').next().unwrap_or(checkout)
}

fn arrangement_of(checkout: &str) -> &str {
    checkout.rsplit_once('/').map_or("", |(a, _)| a)
}

/// A requirement as a TODO names it.
fn need_label(n: &Need) -> String {
    let named = |kind: &str, name: &str| format!("{kind} {}", comment(name));
    match n {
        Need::Implementation(i) => named("implementation", i),
        Need::Capability(c) => named("capability", c),
        Need::Thorn(t) => named("thorn", t),
        Need::Function(f) => named("function", f),
        Need::Include(h) => named("header", h),
    }
}

/// What tells two gaps of one requirement apart (their candidates).
fn gap_signature(gap: &Gap) -> String {
    let list = |cs: &[Candidate]| cs.iter().map(|c| c.checkout.as_str()).collect::<Vec<_>>().join(",");
    match gap {
        Gap::Unmet => String::new(),
        Gap::Unregistered(c) | Gap::Unfrozen(c, _, _) | Gap::Own(c, _) | Gap::Unreadable(c, _, _) => c.checkout.clone(),
        Gap::SeveralInOne(_, cs) | Gap::AnyOf(cs) => list(cs),
    }
}

/// A candidate as a TODO names it: `CarpetX/Loop (et/carpetx@1.0.0)`, or its repo.
fn candidate_label(c: &Candidate, infos: &Pins) -> String {
    let label = match infos.get(&c.repo).and_then(|i| i.package.as_ref().map(|p| (p, i))) {
        Some((p, i)) => match &i.version {
            Some(v) => format!("{} ({p}@{v})", c.checkout),
            None => format!("{} ({p} at commit {})", c.checkout, short(&i.commit)),
        },
        None => format!("{} (repo {})", c.checkout, c.repo),
    };
    comment(&label)
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The TODO for a requirement of `needers` that got no dependency.
fn gap_text(need: &Need, gap: &Gap, needers: &BTreeSet<String>, infos: &Pins, exact: bool) -> String {
    let what = need_label(need);
    let by = comment(&needers.iter().map(String::as_str).collect::<Vec<_>>().join(", "));
    match gap {
        Gap::Unmet => format!("# TODO: {what}, for {by}: no installed thorn provides it.\n"),
        Gap::Unregistered(c) => format!(
            "# TODO: {what}, for {by}: provided by {} from repo {}, which is no registered package.\n\
             # Register that repo, or depend on a registered package that provides it.\n",
            comment(&c.checkout),
            comment(&c.repo)
        ),
        Gap::Unfrozen(c, package, commit) => format!(
            "# TODO: {what}, for {by}: provided by {} of {} at commit {}, which no version froze.\n\
             # Depend on a released version of {} that provides it.\n",
            comment(&c.checkout),
            comment(package),
            comment(short(commit)),
            comment(package)
        ),
        Gap::Own(c, package) => format!(
            "# TODO: {what}, for {by}: provided by {} of {}, this repo's own package.\n\
             # A package does not depend on itself.\n",
            comment(&c.checkout),
            comment(package)
        ),
        Gap::Unreadable(c, package, version) => format!(
            "# TODO: {what}, for {by}: provided by {} of {}@{}.\n# cactup cannot read that version.\n",
            comment(&c.checkout),
            comment(package),
            comment(version)
        ),
        Gap::SeveralInOne(package, cs) => format!(
            "# TODO: {what}, for {by}: several thorns of {} provide it ({}).\n\
             # Depend on {} for the one needed by these thorns.\n",
            comment(package),
            comment(&cs.iter().map(|c| c.thorn.as_str()).collect::<Vec<_>>().join(", ")),
            comment(package)
        ),
        Gap::AnyOf(cs) => {
            let mut s = format!(
                "# TODO: {what}, for {by}: several installed packages provide it.\n\
                 # Depend on the one needed by these thorns, or on any of them:\n\
                 # [[dependency]]\n# any-of = [\n"
            );
            for c in cs {
                match infos.get(&c.repo).and_then(|i| Some((i.package.as_ref()?, i.version.as_ref()?))) {
                    Some((p, v)) => match v.parse::<TrackedVersion>() {
                        Ok(v) => s.push_str(&format!(
                            "#     {{ target = {}, needs = [{}] }},\n",
                            q(&format!("{p}@{}", range_of(&v, exact))),
                            q(&c.thorn)
                        )),
                        Err(_) => s.push_str(&format!("#     # {}\n", candidate_label(c, infos))),
                    },
                    None => s.push_str(&format!("#     # {}: no released package\n", candidate_label(c, infos))),
                }
            }
            s.push_str(&format!("# ]\n# needed-by = {}\n", qa(needers.iter().map(String::as_str))));
            s
        }
    }
}

/// Where the repo keeps its thorns, as the manifest must say it.
#[derive(Default)]
struct Layout {
    root: Option<String>,
    arrangement: Option<String>,
    todos: Vec<String>,
}

/// The repo's thorns, found as the crate's `ccl::scan_tree` finds them (at the root, one level
/// down, or two), else under the folder where the thornlist's `!REPO_PATH` puts them; and the
/// layout the manifest must give for them: `root` when they are not at the repo's root, and
/// `arrangement` when their folders do not name the arrangement the installation builds them in.
fn own_thorns(dir: &Path, mine: &[&Component]) -> (Vec<Own>, Layout) {
    let mut layout = Layout::default();
    let mut found = super::resolution::scan_thorns(dir);
    if found.is_empty() {
        for c in mine {
            let (a, t) = (arrangement_of(&c.checkout), thorn_of(&c.checkout));
            let rel = c.repo_path.as_deref().map_or_else(|| c.checkout.clone(), |p| p.replace("$1", a).replace("$2", t));
            let rel = rel.trim_matches('/');
            let at = rel.strip_suffix(c.checkout.as_str()).or_else(|| rel.strip_suffix(t)).map(|r| r.trim_end_matches('/'));
            let Some(at) = at.filter(|r| !r.is_empty() && Path::new(r).components().all(|p| matches!(p, std::path::Component::Normal(_)))) else {
                continue;
            };
            let deeper = super::resolution::scan_thorns(&dir.join(at));
            if !deeper.is_empty() {
                layout.root = Some(at.to_owned());
                found = deeper;
                break;
            }
        }
    }
    let prefix = layout.root.as_deref().map(PathBuf::from).unwrap_or_default();
    // The installation's arrangement for each thorn it checks out from the repo.
    let checked_out = |name: &str| mine.iter().find(|c| thorn_of(&c.checkout) == name).map(|c| c.checkout.clone());
    let mut own: Vec<Own> = Vec::new();
    for s in found {
        let name = match &s.name {
            Some(n) => n.clone(),
            // The repo is the thorn: the installation names it.
            None => match mine.iter().find(|c| c.repo_path.as_deref().is_some_and(|p| p.starts_with("../"))) {
                Some(c) => thorn_of(&c.checkout).to_owned(),
                None => continue,
            },
        };
        let checkout = checked_out(&name);
        own.push(Own {
            name,
            checkout,
            arrangement: s.arrangement.clone(),
            rel: prefix.join(&s.dir),
            facts: s.facts,
            includes: Vec::new(),
        });
    }
    // An arrangement for every thorn whose folder does not give the one it is built in.
    let wanted: BTreeSet<&str> = (own.iter())
        .filter(|t| t.checkout.as_deref().is_some_and(|c| t.arrangement.as_deref() != Some(arrangement_of(c))))
        .filter_map(|t| t.checkout.as_deref().map(arrangement_of))
        .collect();
    let unplaced = own.iter().any(|t| t.arrangement.is_none() && t.checkout.is_none());
    match wanted.into_iter().collect::<Vec<_>>().as_slice() {
        [] if unplaced => {
            layout.todos.push("# TODO: name the arrangement of the thorns at the repo's root:\n# arrangement = \"…\"\n".to_owned())
        }
        [] => {}
        [one] => layout.arrangement = Some((*one).to_owned()),
        several => layout.todos.push(format!(
            "# TODO: the installation builds these thorns in several arrangements ({}).\n\
             # A package has one: set arrangement, or move the thorns into Arrangement/Thorn folders.\n",
            comment(&several.join(", "))
        )),
    }
    if let Some(a) = &layout.arrangement {
        for t in own.iter_mut() {
            t.arrangement = Some(a.clone());
        }
    }
    (own, layout)
}


/// Runs just after each source file the include scan reads (tests: a Ctrl-C part way through).
#[cfg(test)]
static AFTER_SOURCE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// How many times the crate's source scan was started (tests).
#[cfg(test)]
static CRATE_SCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// What reading a thorn's sources gives: the `USES INCLUDE` headers it cannot compile without
/// (the crate's rule), and every `#include` it writes, with its file.
type Scanned = (Vec<String>, Vec<(String, String)>);

/// One thorn's sources read, on the Registry's pool: Ctrl-C stops the include scan between
/// files, and the crate's scan (which takes no flag) is then not started.
fn scan_one(dir: &Path, t: &Own) -> Scanned {
    let at = dir.join(&t.rel);
    let includes = includes_of(&at);
    let mut facts = t.facts.clone();
    if !gix::interrupt::is_triggered() {
        #[cfg(test)]
        CRATE_SCANS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        facts.scan_sources(&at);
    }
    (facts.needs_includes, includes)
}

/// The `#include` names of a thorn's sources (under `src/`), each with its file: what the crate
/// reads for `USES INCLUDE`, by the same bounds (no links, no hidden directories, at most
/// [`ccl::MAX_SOURCE_FILES`] files of at most [`ccl::MAX_SOURCE_BYTES`]).
fn includes_of(thorn_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut budget = ccl::MAX_SOURCE_FILES;
    let mut stack = vec![thorn_dir.join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if gix::interrupt::is_triggered() || budget == 0 {
                return out;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ty) = e.file_type() else { continue };
            if name.starts_with('.') || ty.is_symlink() {
                continue;
            }
            if ty.is_dir() {
                stack.push(e.path());
                continue;
            }
            if !ty.is_file() || !is_source(&name) {
                continue;
            }
            budget -= 1;
            let path = e.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.len() > ccl::MAX_SOURCE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let file = path.strip_prefix(thorn_dir).unwrap_or(&path).to_string_lossy().into_owned();
            for line in String::from_utf8_lossy(&bytes).lines() {
                if let Some(name) = include_name(line) {
                    out.push((file.clone(), name.to_owned()));
                }
            }
            #[cfg(test)]
            if let Some(hook) = AFTER_SOURCE.get() {
                hook();
            }
        }
    }
    out
}

/// The name an `#include "x"` or `#include <x>` line includes.
fn include_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('#')?.trim_start().strip_prefix("include")?.trim_start();
    let (open, close) = match rest.chars().next()? {
        '"' => ('"', '"'),
        '<' => ('<', '>'),
        _ => return None,
    };
    let inner = rest.strip_prefix(open)?;
    let name = inner.split(close).next()?.trim();
    (!name.is_empty() && inner.contains(close)).then_some(name)
}

/// The sources the preprocessor reads, as the crate counts them.
fn is_source(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else { return false };
    matches!(
        ext,
        "c" | "cc" | "cxx" | "cpp" | "C" | "h" | "hh" | "hxx" | "hpp" | "inc" | "cu" | "cuh" | "F" | "F90" | "F77"
            | "f" | "f90" | "f77"
    )
}

/// `path` with `.` and `..` resolved lexically; `None` when it climbs above its start.
fn lexical(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}


/// Each git repo of the installation with a URL, once: the live list's and the base's, so a
/// replaced base repo is asked about too. Each is asked at the commit the config was built
/// from, else where its clone is, else its pin; the repo itself, where its clone is.
fn git_repos<'a>(input: &Input<'a>) -> Vec<&'a Component> {
    let mut out: Vec<&Component> = Vec::new();
    for c in input.live.components().iter().chain(input.base.components()) {
        if c.ty == ComponentType::Git && c.url.is_some() && !out.iter().any(|o| o.repo == c.repo) {
            out.push(c);
        }
    }
    out
}

/// A fork the repo is: of `target` (`owner/pkg@=X.Y.Z`), lacking `missing` of its thorns.
struct Fork {
    target: String,
    missing: Vec<String>,
}

/// The packages the repo may fork: each installed package (or replaced base repo's) sharing a
/// thorn name with it (a flesh, for a flesh), never its own. And whether its clone is shallow
/// (as cactup clones): such a history cannot show a fork.
fn fork_candidates(input: &Input, own: &[Own], flesh: bool, infos: &Pins) -> Res<(Vec<PkgId>, bool)> {
    let myself = infos.get(input.repo).and_then(|i| i.package.clone());
    let my_url = (input.live.components().iter()).find(|c| c.repo == input.repo).and_then(|c| c.url.clone()).unwrap_or_default();
    let names: BTreeSet<String> = own.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    let thorns_of = |repo: &str| -> BTreeSet<String> {
        (input.live.components().iter().chain(input.base.components()))
            .filter(|c| c.repo == repo)
            .map(|c| thorn_of(&c.checkout).to_ascii_lowercase())
            .collect()
    };
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for (repo, info) in infos {
        let Some(package) = &info.package else { continue };
        if repo == input.repo || Some(package) == myself.as_ref() || cactup_ecosystem_common::url::same_repo_lenient(&info.url, &my_url) {
            continue;
        }
        let shares = if flesh { info.flesh } else { !thorns_of(repo).is_disjoint(&names) };
        if shares {
            candidates.insert(package.clone());
        }
    }
    let ids: Vec<PkgId> = candidates.iter().filter_map(|p| PkgId::parse(p).ok()).collect();
    let shallow = !ids.is_empty() && gix::open(input.dir).is_ok_and(|r| r.is_shallow());
    Ok((ids, shallow))
}

/// The forks among `ids`: each package whose versions' frozen commits the repo's history
/// contains, at the newest such version (every frozen commit asked in one walk).
fn forks(reg: &mut Registry, input: &Input, ids: &[PkgId], own: &[Own]) -> Res<Vec<Fork>> {
    let dir = input.dir;
    let (_, head) = crate::fetch::git::head_of(dir)?;
    let names: BTreeSet<String> = own.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    reg.load(ids)?;
    let versions: Vec<(PkgId, Vec<Frozen>)> = (ids.iter())
        .map(|id| {
            let vs = (reg.releases(id).unwrap_or_default().iter())
                .filter_map(|r| {
                    let v = r.at.version()?.clone();
                    let commit = gix::ObjectId::from_hex(r.commit.as_bytes()).ok()?;
                    Some((v, commit, r.thorns.iter().map(|t| t.thorn.clone()).collect()))
                })
                .collect();
            (id.clone(), vs)
        })
        .collect();
    let walked = reg.local_under(HISTORY, &versions, "packages", |(id, _)| id.to_string(), "reading history", |(_, vs)| {
        let commits: Vec<gix::ObjectId> = vs.iter().map(|(_, c, _)| *c).collect();
        crate::fetch::git::is_ancestor(dir, &commits, head)
    })?;
    let mut out = Vec::new();
    for ((id, vs), contained) in versions.iter().zip(walked) {
        let contained = contained?;
        let inside: Vec<&Frozen> = vs.iter().zip(&contained).filter(|(_, c)| **c).map(|(v, _)| v).collect();
        // The newest on the default track, else on any (ties to the track first by name).
        let newest = (inside.iter().filter(|(v, _, _)| v.track.is_none()).max_by(|a, b| a.0.version.cmp(&b.0.version)))
            .or_else(|| {
                inside.iter().max_by(|a, b| {
                    a.0.version.cmp(&b.0.version).then_with(|| b.0.track.as_ref().map(|t| t.as_str().to_owned()).cmp(&a.0.track.as_ref().map(|t| t.as_str().to_owned())))
                })
            });
        let Some((v, _, thorns)) = newest else { continue };
        let upstream: Slug = id.as_str().parse().map_err(|e| anyhow!("{id}: {e}"))?;
        let target = ForkOf::exactly(&upstream, v).target.to_string();
        let missing: Vec<String> = thorns.iter().filter(|t| !names.contains(&t.to_ascii_lowercase())).cloned().collect();
        out.push(Fork { target, missing });
    }
    Ok(out)
}

/// A thorn as a smell names it: its checkout, with its package or repo.
fn installed_label(i: &Installed, infos: &Pins) -> String {
    let label = match infos.get(&i.repo).and_then(|p| p.package.as_ref()) {
        Some(p) => format!("{} ({p})", i.checkout),
        None => format!("{} (repo {})", i.checkout, i.repo),
    };
    comment(&label)
}

/// An `#include` that climbs out of its thorn's directory into another installed thorn's, as the
/// tree lays them out (`arrangements/<Arrangement>/<Thorn>`): code the CCL does not declare.
fn include_smells(own: &[Own], installed: &[Installed], infos: &Pins) -> Vec<String> {
    let mut said: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    for t in own {
        let Some(arrangement) = &t.arrangement else { continue };
        let home = format!("arrangements/{arrangement}/{}", t.name);
        for (file, path) in &t.includes {
            if !path.split('/').any(|p| p == "..") {
                continue;
            }
            let from = Path::new(file).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(at) = lexical(&format!("{home}/{from}/{path}")) else { continue };
            if at.starts_with(&format!("{home}/")) {
                continue;
            }
            let mut parts = at.split('/');
            let (Some("arrangements"), Some(a), Some(b)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let into = format!("{a}/{b}");
            let Some(other) = installed.iter().find(|i| i.checkout == into) else { continue };
            if !said.insert((t.name.clone(), into.clone())) {
                continue;
            }
            out.push(format!(
                "# TODO: {} includes {} from {}.\n# If it needs that thorn, name it in the needs of a dependency.\n",
                comment(&t.name),
                q(path),
                installed_label(other, infos)
            ));
        }
    }
    out
}

/// A thorn that inherits from (or befriends) the generic driver but includes a header only one
/// driver provides (misfit 19: `INCLUDE[S] … IN`, which the local scan reads; an include by path
/// into a driver's directory is an include smell already).
fn driver_smells(own: &[Own], installed: &[Installed], infos: &Pins) -> Vec<String> {
    let drivers: Vec<&Installed> =
        installed.iter().filter(|i| i.facts.implements.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(DRIVER))).collect();
    let mut out = Vec::new();
    for t in own {
        let generic = t.facts.inherits.iter().chain(&t.facts.friends).any(|i| i.eq_ignore_ascii_case(DRIVER));
        if !generic {
            continue;
        }
        let headers: BTreeSet<String> = (t.includes.iter())
            .map(|(_, p)| p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase())
            .chain(t.facts.uses_includes.iter().map(|h| h.to_ascii_lowercase()))
            .collect();
        for d in &drivers {
            let Some(h) = d.facts.provides_includes.iter().find(|h| headers.contains(&h.to_ascii_lowercase())) else {
                continue;
            };
            out.push(format!(
                "# TODO: {} inherits the generic {DRIVER}, but includes {}, provided by {}.\n\
                 # If it needs that driver, depend on its package for {}.\n",
                comment(&t.name),
                comment(h),
                installed_label(d, infos),
                comment(&d.thorn)
            ));
        }
    }
    out
}

/// A flesh fork in use (an added flesh package, or a base flesh whose package forks another):
/// these thorns may need it, which no CCL can say (misfit 11).
fn flesh_fork(input: &Input, infos: &Pins, exact: bool) -> Option<String> {
    let root = input.live.root();
    let flesh = (input.live.components().iter()).find(|c| crate::fetch::is_flesh(root, c)).map(|c| c.repo.as_str())?;
    if flesh == input.repo {
        return None;
    }
    let info = infos.get(flesh);
    let added = input.meta.added.iter().find(|a| a.is_flesh() && a.repo == flesh);
    let forks = info.is_some_and(|i| !i.fork_of.is_empty());
    let (package, version) = match (added, info) {
        (Some(a), _) => (a.package.clone(), info.and_then(|i| i.version.clone())),
        (None, Some(i)) if forks => (i.package.clone()?, i.version.clone()),
        _ => return None,
    };
    let of = info.filter(|_| forks).map(|i| format!(", a fork of {}", comment(&i.fork_of.join(", ")))).unwrap_or_default();
    let shown = comment(&package);
    let mut s = match &version {
        Some(v) => format!("# TODO: the flesh here is {shown}@{}{of}.\n", comment(v)),
        None => format!("# TODO: the flesh here is {shown}, at a commit not frozen by any version{of}.\n"),
    };
    match version.as_deref().and_then(|v| v.parse::<TrackedVersion>().ok()) {
        Some(v) => s.push_str(&format!(
            "# If these thorns need it, depend on it:\n# [[dependency]]\n# target = {}\n",
            q(&format!("{package}@{}", range_of(&v, exact)))
        )),
        None => s.push_str(&format!("# If these thorns need it, depend on a released version of {shown}.\n")),
    }
    Some(s)
}

/// A release tag of the repo: its name, its version and the commit it names.
type Tag = (String, TrackedVersion, gix::ObjectId);

/// The repo's release tags as Cactarium reads them for `package` (`tag-pattern`, else
/// `[track/]vX.Y.Z`; none below `since`), newest first: the default track first, then by
/// version.
fn release_tags(dir: &Path, package: &PackageSection) -> Res<Vec<Tag>> {
    let pattern = match &package.tag_pattern {
        Some(p) => Some(p.parse::<cactup_ecosystem_common::version::TagPattern>().map_err(|e| anyhow!("{e}"))?),
        None => None,
    };
    let mut tags: Vec<Tag> = (crate::fetch::git::local_tags(dir)?.into_iter())
        .filter_map(|(name, commit)| {
            let v = match &pattern {
                Some(p) => p.version(&name)?,
                None => TrackedVersion::from_tag(&name)?,
            };
            package.since.as_ref().is_none_or(|s| v.version >= *s).then_some((name, v, commit))
        })
        .collect();
    tags.sort_by(|a, b| {
        (a.1.track.is_some(), std::cmp::Reverse(&a.1.version), a.1.track.as_ref().map(|t| t.as_str()))
            .cmp(&(b.1.track.is_some(), std::cmp::Reverse(&b.1.version), b.1.track.as_ref().map(|t| t.as_str())))
    });
    Ok(tags)
}

/// The tag to suggest for the next release: the newest default-track version's next patch,
/// `v1.0.0` for the first, a placeholder under a `tag-pattern`.
fn next_tag(tags: &[Tag], package: &PackageSection) -> String {
    if package.tag_pattern.is_some() {
        return "<tag>".to_owned();
    }
    match tags.iter().find(|(_, v, _)| v.track.is_none()) {
        Some((_, v, _)) => format!("v{}.{}.{}", v.version.major, v.version.minor, v.version.patch + 1),
        None => "v1.0.0".to_owned(),
    }
}

/// `cactup package init <repo> --publish`: the repo's `cactarium.toml`, as its newest release tag
/// on its remote holds it, registered with Cactarium (`POST /repos`: a preview, then exactly
/// what it showed), or, when the package is registered already, checked for new releases.
/// Writes nothing.
fn publish(client: &Client, live: &Thornlist, repo: &str, dir: &Path, namespace: Option<&str>) -> Res<()> {
    let namespace = namespace.map(owner_given).transpose()?;
    let file = dir.join(PACKAGE_FILE);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(_) => bail!("Repo {repo} has no {PACKAGE_FILE}.\nWrite one with `cactup package init {repo}`."),
    };
    let manifest = Manifest::from_repo_file(PACKAGE_FILE, &text).map_err(|e| anyhow!("'{}': {e}", file.display()))?;
    let package = manifest.package.as_ref().expect("a package file holds a [package]");
    let name = package.name.to_string();
    let git = format!("git -C {}", shell_word(&dir.display().to_string()));
    let tags = release_tags(dir, package)?;
    let next = next_tag(&tags, package);
    // How to get the file on disk into a pushed release tag.
    let (_, head) = crate::fetch::git::head_of(dir)?;
    let committed = crate::fetch::git::file_in_commit(dir, head, PACKAGE_FILE)?;
    // Committed as it is on disk: only the tag is missing.
    let ready = committed.as_deref() == Some(text.as_bytes());
    let mut steps: Vec<String> = match committed {
        None => vec![format!("{git} add {PACKAGE_FILE}"), format!("{git} commit -m \"Add {PACKAGE_FILE}\"")],
        Some(committed) if committed != text.as_bytes() => {
            vec![format!("{git} commit -m \"Update {PACKAGE_FILE}\" {PACKAGE_FILE}")]
        }
        Some(_) => Vec::new(),
    };
    steps.extend([format!("{git} tag {next}"), format!("{git} push"), format!("{git} push origin {next}")]);
    let steps: String = steps.iter().map(|s| format!("\n  {s}")).collect();
    let how = match ready {
        true => "Tag a release and push it:".to_owned(),
        false => format!("Commit {PACKAGE_FILE}, tag a release and push both:"),
    };
    if tags.is_empty() {
        bail!("Repo {repo} has no release tag.\n{how}{steps}");
    }

    // What anybody can fetch, and which of its release tags are there.
    let url = (live.components().iter())
        .find(|c| c.repo == repo && c.ty == ComponentType::Git)
        .and_then(|c| c.url.clone())
        .or_else(|| crate::fetch::git::origin_url(dir))
        .ok_or_else(|| anyhow!("Repo {repo} has no URL."))?;
    let (url, remote) = ask_remote(repo, &url)?;
    let pushed = tags.iter().position(|(t, _, c)| remote.iter().any(|(rt, rc)| rt == t && rc == c));
    let Some(at) = pushed else {
        let newest = &tags[0].0;
        bail!("Tag {newest} of repo {repo} is not on its remote.\nPush it:\n  {git} push origin {newest}");
    };
    let (tag, _, commit) = &tags[at];
    match crate::fetch::git::file_in_commit(dir, *commit, PACKAGE_FILE)? {
        None => bail!("Tag {tag} of repo {repo} has no {PACKAGE_FILE}.\n{how}{steps}"),
        Some(tagged) if tagged != text.as_bytes() => bail!(
            "{PACKAGE_FILE} in repo {repo} differs from the one at tag {tag}.\n{how}{steps}"
        ),
        Some(_) => {}
    }
    for (newer, _, _) in &tags[..at] {
        println!("{}", format!("Tag {newer} is not on the remote.").yellow());
    }
    if at > 0 {
        println!("{}", format!("Cactarium reads tag {tag}.").yellow());
    }

    let request = RegisterRequest {
        url,
        tag: None,
        owner: namespace.clone(),
        only: vec![name.clone()],
        dry_run: true,
        expect: None,
    };
    let reg = Registry::new(client, ["register", "refresh"]);
    let answer = (|| -> Res<Done> {
        let preview = reg
            .one("register", "previewing", {
                let request = request.clone();
                move |c| c.register(&request)
            })
            .map_err(|e| register_refused(e, client.base(), namespace.as_deref(), &name))?;
        let entry = entry_of(&preview, &name)?;
        match entry.status.as_str() {
            "existing" => {
                let slug = entry.slug.clone();
                let (owner, pkg) = slug.split_once('/').ok_or_else(|| anyhow!("Cactarium named the package \"{slug}\""))?;
                let (owner, pkg) = (owner.to_owned(), pkg.to_owned());
                let refreshed = reg.one("refresh", "asking for new tags", move |c| c.refresh(&owner, &pkg))?;
                Ok(Done::Refreshed(refreshed))
            }
            "new" => {
                // Never a registration unguarded by what the preview showed.
                let Some(expect) = preview.expect.clone() else {
                    bail!("Cactarium's preview of {} came without a fingerprint, so nothing was registered.", entry.slug);
                };
                let real = RegisterRequest { dry_run: false, expect: Some(expect), ..request.clone() };
                let registered = reg
                    .one("register", "registering", move |c| c.register(&real))
                    .map_err(|e| register_refused(e, client.base(), namespace.as_deref(), &name))?;
                Ok(Done::Registered(registered))
            }
            _ => Err(refused_entry(entry)),
        }
    })();
    reg.finish();
    match answer? {
        Done::Refreshed(r) => super::import::print_refreshed(&r),
        Done::Registered(r) => {
            let entry = entry_of(&r, &name)?;
            if entry.status != "new" {
                return Err(refused_entry(entry));
            }
            match &entry.url {
                Some(url) => println!("Registered {}: {url}", entry.slug.bold()),
                None => println!("Registered {}.", entry.slug.bold()),
            }
            match (entry.new_versions, entry.awaiting_review) {
                (0, _) => println!("{}", "No versions yet: tag a release and push the tag.".yellow()),
                (n, 0) => println!("{n} {}.", if n == 1 { "version" } else { "versions" }),
                (n, w) => println!("{}", format!("{n} {}, {w} awaiting admin review.", if n == 1 { "version" } else { "versions" }).yellow()),
            }
            for w in entry.warnings.iter().chain(&r.warnings) {
                println!("{}", w.yellow());
            }
        }
    }
    Ok(())
}

/// The URL to register `url` under, as the crate's rules decide for anything shared (refused
/// with credentials, as a local path or git://, or when nobody could fetch it anonymously; an
/// ssh remote goes as its https equivalent), and the tags the remote has there. A line each.
fn ask_remote(repo: &str, url: &str) -> Res<(String, Vec<(String, gix::ObjectId)>)> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let layout = crate::progress::Layout::for_names([repo]);
    let asked = (|| -> Res<(String, Vec<(String, gix::ObjectId)>)> {
        let window = crate::fetch::git::AskWindow::now();
        let line = crate::progress::Line::over(progress.add_child(repo.to_owned()), repo, layout.clone());
        line.phase("asking for it anonymously".to_owned());
        let verdict = crate::fetch::anonymous::check_remote(url, window);
        drop(line);
        let published = match verdict? {
            crate::fetch::anonymous::Verdict::Publish(u) => u,
            crate::fetch::anonymous::Verdict::Refuse(why) => bail!("{why}"),
        };
        let line = crate::progress::Line::over(progress.add_child(repo.to_owned()), repo, layout.clone());
        line.phase("listing its tags".to_owned());
        let tags = crate::fetch::git::ask_remote_tags(&published, window);
        drop(line);
        Ok((published, tags?))
    })();
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    asked
}

/// What `--publish` did.
enum Done {
    Registered(Registered),
    Refreshed(cactup_ecosystem_common::api::Refreshed),
}

/// The package entry named `name` in a registration's answer.
fn entry_of<'r>(r: &'r Registered, name: &str) -> Res<&'r cactup_ecosystem_common::api::RegisteredEntry> {
    (r.entries.iter())
        .find(|e| e.kind == "package" && e.slug.rsplit('/').next() == Some(name))
        .ok_or_else(|| anyhow!("Cactarium found no package {name} in the repo"))
}

/// A registration Cactarium will not make, with its reason.
fn refused_entry(e: &cactup_ecosystem_common::api::RegisteredEntry) -> anyhow::Error {
    let why = comment(e.note.as_deref().unwrap_or("no reason given"));
    match e.status.as_str() {
        "needs-tag" | "skipped" => anyhow!("Cactarium cannot register {}: {why}\nTag a release and push the tag.", e.slug),
        _ => anyhow!("Cactarium cannot register {}: {why}", e.slug),
    }
}

/// A registration request Cactarium refused (400): its message, with the way past when it is
/// about tags. A Cactarium from before `only` named a registered package answers "No new entry
/// named …": too old to refresh this way.
fn register_refused(e: anyhow::Error, base: &str, namespace: Option<&str>, name: &str) -> anyhow::Error {
    let Some(refused) = e.downcast_ref::<crate::cactarium::Refused>().filter(|r| r.status == 400) else { return e };
    let message = refused.message.clone();
    if message.starts_with("No new entry named") {
        let owner = namespace.unwrap_or("OWNER");
        return anyhow!(
            "Cactarium at {base} is too old to refresh a registered package this way; it needs an update.\n\
             Until then, run `cactup refresh {owner}/{name}`."
        );
    }
    match message.contains("tag") {
        true => anyhow!("{message}\nTag a release and push the tag."),
        false => anyhow!("{message}"),
    }
}

/// `s` as one shell word: as it is when nothing in it is special, else single-quoted.
fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests;
