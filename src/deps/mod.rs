//! Dependency resolution over the Cactarium's claims (design/dependency-resolution-plan.md §2):
//! which releases an `add` brings in, and which packages a `remove` leaves unneeded.
//!
//! Pure: no I/O, no printing, no network, no filesystem. Everything the resolver knows about the
//! registry comes through [`Facts`] (served versions, snapshots at a commit or tip, providers of a
//! requirement), and what a thorn's CCL requires through [`Requirements`]; the caller implements
//! both (in parallel, with progress, honoring Ctrl-C) and prints or confirms what comes back. The
//! only global state read is the interrupt flag, polled once per step.
//!
//! - [`resolve`]: named specs plus the installation's present packages → a [`Plan`] of steps
//!   (add, re-add, add thorns, upgrade, replace, keep), each with why, or a typed [`Refusal`]
//!   that names its way past.
//! - [`plan_remove`]: the dependents that block a removal, and the packages it orphans.
//!
//! Determinism: every map is a `BTreeMap`/`BTreeSet` (or keeps insertion order), and any-of
//! dependencies are decided in a fixed order, so the same inputs give the same plan whatever the
//! order a hash map, the server or the caller happened to list them in.

// Wired into `add`/`remove` by the next phase (P3); until then only the tests call it.
#![cfg_attr(not(test), allow(dead_code))]

mod conflict;
mod overrides;
mod plan;
mod refusal;
mod remove;
mod resolve;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;

use cactup_ecosystem_common::api::{ClaimInfo, PackageVersion, ThornInfo};
use cactup_ecosystem_common::ccl::ThornFacts;
use cactup_ecosystem_common::name::{Name, Owner};
use cactup_ecosystem_common::slug::{is_commit_id, is_commit_prefix, Slug};
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};

use crate::Res;
use anyhow::{anyhow, bail};

// The module's API, for P3 (re-exports, so nothing in this module uses them).
#[allow(unused_imports)]
pub use overrides::{ClaimKind, ClaimPattern, Override, Rules};
#[allow(unused_imports)]
pub use plan::{
    Choice, ChoiceKey, ChoiceRule, Library, LineKind, Placement, Plan, Record, Replaced, Step, Untrusted,
};
#[allow(unused_imports)]
pub use refusal::{Live, Refusal, Unmet};
#[allow(unused_imports)]
pub use remove::{is_requested, plan_remove, AddedFor, Orphan, RemovePlan};
#[allow(unused_imports)]
pub use resolve::resolve;

/// A package: `owner/name`, or `repo:<name>` for a repo of the installation's base that no
/// registered package is known to be (a pseudo-package, so conflicts still see its thorns).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PkgId(String);

impl PkgId {
    /// `owner/name` (checked against the registry's name rules) or `repo:<name>`.
    pub fn parse(s: &str) -> Res<PkgId> {
        if let Some(repo) = s.strip_prefix("repo:") {
            if repo.is_empty() || repo.contains('/') {
                bail!("\"{s}\" is not a repo of the installation (repo:<name>)");
            }
            return Ok(PkgId(s.to_owned()));
        }
        let Some((owner, name)) = s.split_once('/') else {
            bail!("\"{s}\" is not a package (owner/name)");
        };
        owner.parse::<Owner>().map_err(|e| anyhow!("\"{s}\" is not a package: {e}"))?;
        name.parse::<Name>().map_err(|e| anyhow!("\"{s}\" is not a package: {e}"))?;
        Ok(PkgId(s.to_owned()))
    }

    /// The pseudo-package for an unmapped base repo.
    pub fn repo(name: &str) -> PkgId {
        PkgId(format!("repo:{name}"))
    }

    /// A pseudo-package (`repo:<name>`), which the registry knows nothing about.
    pub fn is_repo(&self) -> bool {
        self.0.starts_with("repo:")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The slug of a dependency on this package with `req`, for the crate's
    /// `ForkOf::stands_in_for`. `None` for a pseudo-package or a tip.
    fn slug(&self, req: &Req) -> Option<Slug> {
        if self.is_repo() {
            return None;
        }
        match req {
            Req::Range(q) => target_text(self.as_str(), Some(&q.to_string()), None),
            Req::Commit(sha) => target_text(self.as_str(), None, Some(sha)),
            Req::Tip(_) => return None,
        }
        .parse()
        .ok()
    }
}

impl fmt::Display for PkgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A claim's target as written: `owner/name`, `owner/name@range` or `owner/name#sha`.
fn target_text(target: &str, range: Option<&str>, sha: Option<&str>) -> String {
    match (range, sha) {
        (_, Some(sha)) => format!("{target}#{sha}"),
        (Some(r), None) => format!("{target}@{r}"),
        (None, None) => target.to_owned(),
    }
}

/// A claim's (or an alternative's) target and what it asks of the target's release.
fn claim_target(target: &str, range: Option<&str>, sha: Option<&str>) -> Result<(PkgId, Req), String> {
    let id = PkgId::parse(target).map_err(|e| e.to_string())?;
    if id.is_repo() {
        return Err(format!("\"{target}\" is not a registered package"));
    }
    Ok((id, Req::from_claim(range, sha)?))
}

/// `r`'s claims to be a fork of `upstream`, as the crate reads them; a claim whose target does
/// not read as one (a bad range or sha) is no claim.
fn fork_claims(r: &Release, upstream: &PkgId) -> Vec<cactup_ecosystem_common::manifest::ForkOf> {
    r.claims
        .iter()
        .filter(|c| c.kind == "fork-of" && c.target == upstream.as_str())
        .filter_map(|c| target_text(&c.target, c.range.as_deref(), c.sha.as_deref()).parse().ok())
        .map(|target| cactup_ecosystem_common::manifest::ForkOf { target, note: None })
        .collect()
}

/// Whether `r` declares itself a fork of `upstream` (with a claim that reads).
fn forks(r: &Release, upstream: &PkgId) -> bool {
    !fork_claims(r, upstream).is_empty()
}

/// Where a release sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum At {
    /// A published version.
    Version(TrackedVersion),
    /// The registry's read of the package at a commit no version froze (`#sha`): a snapshot the
    /// dependency check never ran on.
    Commit(String),
    /// The registry's read of a branch tip (`head`, `master`) at the commit it found then.
    Tip { name: String, commit: String },
    /// Installed at a commit no version froze and that the registry was not asked about (a base
    /// repo at a commit the pin lookup maps to no version, or a pseudo-package).
    Unknown(String),
}

impl At {
    pub fn version(&self) -> Option<&TrackedVersion> {
        match self {
            At::Version(v) => Some(v),
            _ => None,
        }
    }
}

/// The first 12 characters of a commit, as people read one (whatever the server sent).
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// One release of a package: what the resolver chooses among. Built from the Cactarium's
/// `PackageVersion` ([`Release::from_served`]), or by the caller for a present base repo.
#[derive(Clone, Debug)]
pub struct Release {
    pub id: PkgId,
    pub at: At,
    /// The full commit the release is at.
    pub commit: String,
    /// The git tag the version was frozen from (`None` for curated versions and snapshots).
    pub tag: Option<String>,
    pub url: String,
    /// Where to report problems. The versions answer does not carry it yet (asked of the
    /// Cactarium session), so this is `None` until the crate has it; P3 takes a named spec's
    /// from `/resolve`.
    pub contact: Option<String>,
    pub flesh: bool,
    pub library: bool,
    pub yanked: bool,
    /// Awaiting review: served only under `--risky-allow-untrusted-artifacts`.
    pub pending: bool,
    /// Its state is neither `invalid` nor `upstream-missing`.
    pub usable: bool,
    pub thorns: Vec<ThornInfo>,
    pub claims: Vec<ClaimInfo>,
}

impl Release {
    /// A release as the Cactarium serves it: a version, a commit no version froze (the version
    /// is the full commit), or a tip (`head`/`master`). `pending`: the version is awaiting review
    /// (the crate's `VersionInfo` at 2f2c8f4 has no `review` field yet, so the caller reads it).
    pub fn from_served(id: &PkgId, v: &PackageVersion, pending: bool) -> Res<Release> {
        let info = &v.version;
        let commit = info.commit.clone().unwrap_or_default();
        if !is_commit_id(&commit) {
            bail!("The Cactarium gave {id}@{} \"{}\" as its commit, which is none", info.version, commit.escape_debug());
        }
        let at = match info.version.as_str() {
            "head" | "master" => At::Tip { name: info.version.clone(), commit: commit.clone() },
            version if is_commit_id(version) => {
                if version != commit {
                    bail!("The Cactarium read {id} at {version} and answered with commit {commit}");
                }
                At::Commit(commit.clone())
            }
            version => {
                let v = version.parse().map_err(|e| anyhow!("The Cactarium gave {id} a version that is none: {e}"))?;
                At::Version(v)
            }
        };
        Ok(Release {
            id: id.clone(),
            at,
            commit,
            tag: info.tag.clone(),
            url: v.url.clone(),
            contact: None,
            flesh: v.flesh,
            library: v.library,
            yanked: info.yanked,
            pending,
            usable: !matches!(info.state.as_str(), "invalid" | "upstream-missing"),
            thorns: v.thorns.clone(),
            claims: v.claims.clone(),
        })
    }

    /// `owner/name@1.2.0`, `owner/name@head`, or `owner/name#0123456789ab` at a commit no version
    /// names.
    pub fn label(&self) -> String {
        match &self.at {
            At::Version(v) => format!("{}@{v}", self.id),
            At::Tip { name, .. } => format!("{}@{name}", self.id),
            At::Commit(c) | At::Unknown(c) => format!("{}#{}", self.id, short(c)),
        }
    }

    /// The thorn of that name, compared as Cactus compares names (case-insensitively).
    pub fn thorn(&self, name: &str) -> Option<&ThornInfo> {
        self.thorns.iter().find(|t| t.thorn.eq_ignore_ascii_case(name))
    }

    /// Every thorn name, as the release spells them.
    pub fn thorn_names(&self) -> BTreeSet<String> {
        self.thorns.iter().map(|t| t.thorn.clone()).collect()
    }

    fn same(&self, other: &Release) -> bool {
        self.id == other.id && self.commit == other.commit && self.at == other.at
    }
}

/// How a present package came to be in the installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A repo of the base thornlist (`repo` is its name there), mapped to a package by the pin
    /// lookup, or a pseudo-package.
    Base { repo: String },
    /// An `[[added]]` package.
    Added,
}

/// A library an added package has placed (an `[[added.libraries]]` entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledLibrary {
    pub id: PkgId,
    pub commit: String,
    pub into: String,
    /// The library whose clone it goes inside; `None`: the package's own.
    pub inside: Option<PkgId>,
}

/// A package the installation has, with the thorns of it that are enabled.
#[derive(Clone, Debug)]
pub struct Present {
    pub release: Release,
    pub enabled: BTreeSet<String>,
    pub origin: Origin,
    /// The libraries it placed, held where they are while it does not change.
    pub libraries: Vec<InstalledLibrary>,
}

/// What a need asks of its target's release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Req {
    Range(TrackedReq),
    /// One commit, full or abbreviated (lowercase hex).
    Commit(String),
    /// A branch tip (`head`, `master`): only a named spec asks for one.
    Tip(String),
}

impl Req {
    /// A claim's `range`/`sha` as a requirement: `#sha` a commit (7 to 64 lowercase hex digits),
    /// a range a range, `@<hex>` that names no range a commit (as the crate's
    /// `Slug::commit_fallback` reads it), nothing any version on the default track.
    pub fn from_claim(range: Option<&str>, sha: Option<&str>) -> Result<Req, String> {
        if let Some(sha) = sha {
            let sha = sha.to_ascii_lowercase();
            if !is_commit_prefix(&sha) {
                return Err(format!("\"{}\" is not a commit", sha.escape_debug()));
            }
            return Ok(Req::Commit(sha));
        }
        match range {
            None => Ok(Req::Range(TrackedReq::any())),
            Some(r) => match r.parse::<TrackedReq>() {
                Ok(q) => Ok(Req::Range(q)),
                Err(_) if is_commit_prefix(r) => Ok(Req::Commit(r.to_owned())),
                Err(e) => Err(e.to_string()),
            },
        }
    }

    /// One exact version or one commit: the only kind of constraint that may take a yanked release.
    pub fn is_exact(&self) -> bool {
        match self {
            Req::Range(q) => q.is_exact(),
            Req::Commit(_) => true,
            Req::Tip(_) => false,
        }
    }

    /// Whether `r` meets this. A release at no version meets a range only with a note
    /// ([`Fit::ByNote`]); a commit is met only by a release at that commit.
    fn fit(&self, r: &Release) -> Fit {
        match self {
            Req::Range(q) => match &r.at {
                At::Version(v) if q.matches(v) => Fit::Yes,
                At::Version(_) => Fit::No,
                At::Commit(_) | At::Tip { .. } | At::Unknown(_) => Fit::ByNote,
            },
            Req::Commit(sha) if !sha.is_empty() && r.commit.starts_with(sha.as_str()) => Fit::Yes,
            Req::Commit(_) => Fit::No,
            // Resolved to a commit before it becomes a constraint.
            Req::Tip(_) => Fit::No,
        }
    }
}

impl fmt::Display for Req {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Req::Range(q) => write!(f, "{q}"),
            Req::Commit(sha) => write!(f, "#{}", short(sha)),
            Req::Tip(t) => write!(f, "@{t}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    Yes,
    No,
    ByNote,
}

/// One package a command line names: `owner/pkg[/Thorn][@range|@tip|#sha]`.
#[derive(Clone, Debug)]
pub struct NamedSpec {
    pub id: PkgId,
    pub req: Req,
    /// `None`: the whole package; else the thorns named.
    pub thorns: Option<Vec<String>>,
}

impl NamedSpec {
    pub fn parse(spec: &str) -> Res<NamedSpec> {
        let bad =
            |e: &dyn fmt::Display| anyhow!("\"{spec}\" is not a package (owner/name[/Thorn][@version|#commit]): {e}");
        let slug: Slug = spec.parse().map_err(|e| bad(&e))?;
        let thorn = slug.as_thorn().map_err(|e| bad(&e))?.map(str::to_owned);
        let req = if let Some(sha) = slug.sha() {
            Req::Commit(sha.to_owned())
        } else if slug.tip().is_some() {
            Req::Tip(slug.at.clone().unwrap_or_default())
        } else {
            match slug.req() {
                Ok(q) => Req::Range(q),
                Err(e) => match slug.commit_fallback() {
                    Some(sha) => Req::Commit(sha.to_owned()),
                    None => return Err(bad(&e)),
                },
            }
        };
        Ok(NamedSpec { id: PkgId(slug.entry()), req, thorns: thorn.map(|t| vec![t]) })
    }
}

/// The flags that change what a plan may do.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// `--upgrade`: a present package the plan touches moves, when its release no longer fits,
    /// to its newest release that does.
    pub upgrade: bool,
    /// `--upgrade-all`: every present package moves to its newest release that fits.
    pub upgrade_all: bool,
    /// `--no-deps`: nothing beyond the named specs (libraries are still placed); a present
    /// package's declared range the plan breaks becomes a note.
    pub no_deps: bool,
    /// `--replace`: a planned package whose thorns collide with a base repo's replaces those
    /// checkouts (all of the repo when it upgrades or forks the repo's package).
    pub replace: bool,
}

/// What [`resolve`] works from. The caller asks `/resolve` for each named spec first, as `add`
/// does today (the entry's kind, the server's own error text, the contact); the resolver then
/// reads versions through [`Facts`].
pub struct Request<'a> {
    pub specs: &'a [NamedSpec],
    pub present: &'a [Present],
    pub rules: &'a Rules,
    pub options: Options,
}

/// Why a package is in the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// Named on the command line.
    Named,
    /// A dependency of another chosen release.
    Needed {
        /// `owner/name@version` of the dependent.
        by: String,
        by_id: PkgId,
        /// The dependency as people read it: `et/carpet@^1`, or `any of et/carpet, et/cactuspugh`.
        claim: String,
        /// What it asks of this package's release.
        req: String,
        /// The dependent's thorns it is for (empty: every thorn).
        needed_by: Vec<String>,
        /// For an any-of dependency, the rule that chose this alternative.
        rule: Option<ChoiceRule>,
    },
    /// `--upgrade-all` moved it.
    UpgradeAll,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Named => f.write_str("named"),
            Why::UpgradeAll => f.write_str("--upgrade-all"),
            Why::Needed { by, claim, req, needed_by, rule, .. } => {
                if let Some(rule) = rule {
                    return write!(f, "{by}: {claim} ({rule})");
                }
                write!(f, "needed by {by}")?;
                if !needed_by.is_empty() {
                    write!(f, " ({})", needed_by.join(", "))?;
                }
                if req != "*" {
                    write!(f, ", {req}")?;
                }
                Ok(())
            }
        }
    }
}

/// A hard CCL requirement of a thorn, as the Cactarium's dependency check reads one. The crate
/// will carry the shared type (`ccl::Requirement`, S7); until then this mirrors it, and the
/// semantics live behind [`Requirements`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Requirement {
    /// Inherits or befriends an implementation.
    Implementation(String),
    Capability(String),
    Thorn(String),
    Function(String),
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Requirement::Implementation(i) => write!(f, "inherits or befriends {i}"),
            Requirement::Capability(c) => write!(f, "requires the capability {c}"),
            Requirement::Thorn(t) => write!(f, "requires the thorn {t}"),
            Requirement::Function(x) => write!(f, "requires the function {x}"),
        }
    }
}

/// Which requirements a thorn's facts make, and whether a thorn meets one: one definition shared
/// with the server's check and `--trim` once the crate has it (S7).
pub trait Requirements {
    fn of(&self, facts: &ThornFacts) -> Vec<Requirement>;
    fn meets(&self, thorn: &str, facts: &ThornFacts, requirement: &Requirement) -> bool;
}

/// A registered thorn that provides what a requirement asks for (`GET /providers`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub package: PkgId,
    pub version: String,
    pub arrangement: String,
    pub thorn: String,
}

/// A package read at a commit or branch tip.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub release: Release,
    /// Nobody vouches for it (an untrusted package's tip or unreviewed commit). Only reported:
    /// the plan lists the snapshots it chose ([`Plan::untrusted`]) for the caller to confirm.
    pub untrusted: bool,
}

/// Everything the resolver asks the registry. The caller's implementation does the requests (in
/// waves, in parallel, with progress) and applies nothing: overrides and ignores are applied by
/// the resolver as releases are loaded. Named specs are not resolved here: the caller asks
/// `/resolve` for each first (see [`Request`]).
pub trait Facts {
    /// Fetches the served versions of these packages: one wave. Packages already loaded, and
    /// pseudo-packages, are never asked for. Returns an error when interrupted (the flag is
    /// polled per package), never a partial load: a package missing from a finished load is one
    /// the Cactarium does not have.
    fn load(&mut self, ids: &[PkgId]) -> Res<()>;
    /// A loaded package's releases, newest first within each track; `None` when the Cactarium
    /// has no such package.
    fn releases(&self, id: &PkgId) -> Option<&[Release]>;
    /// The package read at a commit (full or abbreviated) or a tip (`head`, `master`). Never
    /// asks the user anything: an untrusted snapshot is only marked, and confirmed by the caller
    /// once the plan is known.
    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Snapshot>;
    /// The registered thorns meeting a requirement (to list in a refusal; never planned).
    fn providers(&mut self, requirement: &Requirement) -> Res<Vec<Provider>>;
}
