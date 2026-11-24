//! The resolver: named specs plus the installation's present packages → a [`Plan`], by a
//! deterministic backtracking search with conflict-directed backjumping.
//!
//! The search state is an assignment: a release for each package decided so far, and an
//! alternative for each any-of decided so far. Everything else (which packages are in play, the
//! needs and their takers, the enabled thorns, the constraints) is derived from the assignment
//! alone at every node, so nothing stale survives a change of mind:
//! - needs come from the named specs and from the claims of the releases in play; a present
//!   package that is not changing turns its claims into passive constraints, which narrow at once
//!   when it can never move, and are checked once the assignment is complete when it might (and
//!   nothing came to it);
//! - a need is taken by its target when that is present or named; else by a package an override
//!   says satisfies it, a fork in play (or required) that stands in for it, or the target itself,
//!   which is then required and has to be decided.
//!
//! Decisions, in a fixed order: present packages something touched (they decide what their needs
//! bring in), then the packages the plan brings in (named ones first, each group by id), then
//! any-of dependencies (by dependent, then declaration order). A package's candidates come in
//! order of preference: a present package that may stay tries its present release first;
//! otherwise newest first, approved before awaiting review; yanked only for an exact constraint
//! (an installed release is never pushed out for being yanked). An any-of tries its alternatives in
//! the order of its rules: one present at a release that fits, one already in play, then the
//! listed order ("conflicts with nothing" being "the rest of the search succeeds").
//!
//! Conflicts that can only stay (fork and upstream, incompatible-with, thorn names, library
//! placements against installed ones, requirements nothing reachable provides) are pruned as soon
//! as the packages involved are settled; the rest is checked on a complete assignment. A failure
//! names the decisions involved in it, and the search jumps back to the deepest of those rather
//! than trying every sibling in between. When the search fails, the refusal is the most specific
//! one found (a specific refusal over a bare "these do not fit together", then the deepest); past
//! a step budget scaled with the number of packages, or a wall-time budget, it is that refusal if
//! one was specific, else "no consistent set" with the constraints of the deepest failure.
// Plan §2.2.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use cactup_ecosystem_common::api::{AlternativeInfo, ClaimInfo};
use cactup_ecosystem_common::ccl::{self, ThornFacts};
use cactup_ecosystem_common::manifest::ForkOf;
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use indexmap::{IndexMap, IndexSet};

use super::plan::{Choice, ChoiceRule, Library, Placement, Plan, Record, Replaced, Step, Untrusted};
use super::refusal::{Live, Refusal, UnreadFor, UnreadNeed, Unmet, Ways};
use super::{
    claim_target, conflict, fork_claims, short, target_text, At, Facts, Fit, NamedSpec, Options, Origin, PkgId,
    Present, Release, Req, Request, Requirement, Rules, Unanswered, UnreadKind, Why,
};
use crate::Res;
use anyhow::bail;

/// The step budget: about this many package-steps (a step costs time in proportion to the
/// packages in play), so the budget binds long before the wall-time net on any machine (some
/// 4 s in a release build with 80 packages), and the result never depends on the machine.
pub(super) const PACKAGE_STEPS: usize = 1_000_000;
/// The fewest steps, however many packages.
pub(super) const MIN_STEPS: usize = 2_000;
/// A safety net past the step budget: reported as giving up after it, with the closest refusal.
pub(super) const WALL_TIME: Duration = Duration::from_secs(30);
/// How often (in steps) the caller hears that the search is still going.
const TICK: usize = 64;

#[cfg(test)]
thread_local! {
    /// A step budget in place of the computed one (tests of running out).
    pub(super) static BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The steps the last search in this thread took.
    pub(super) static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type Rel = Rc<Release>;

/// How many searches with another flag a refusal's ways past may cost.
const WAY_CHECKS: usize = 3;

/// Resolves `request` against the registry `facts` gives.
/// A refusal comes back as a [`Refusal`] inside the error (downcast to read it); anything else
/// is a failure of `facts` or an interrupt. A flag a refusal suggests is suggested only when a
/// search with it, on the facts already read (no read at a commit more), finds a plan.
pub fn resolve(request: &Request, facts: &mut dyn Facts) -> Res<Plan> {
    let mut cat = Catalog::new(facts, request.rules);
    let mut refusal = match run(request, &mut cat, Mode::Main)? {
        Ok(plan) => return Ok(plan),
        Err(r) => r.expect("the main search says why"),
    };
    check_ways(request, &mut cat, &mut refusal)?;
    Err(refusal.into())
}

/// Whether a search is the command's own or one checking a way past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Main,
    /// No read at a commit beyond those made, and no refusal worked out.
    Check,
}

/// The flags `r` would suggest, and where it keeps whether each works.
fn suggested(r: &mut Refusal, o: Options) -> Vec<(Flag, &mut bool)> {
    let moves = !(o.upgrade || o.upgrade_all);
    match r {
        Refusal::GaveUp { best, .. } => suggested(best, o),
        Refusal::PresentOutsideRange { upgrade, .. } | Refusal::PresentLacksThorns { upgrade, .. } => {
            *upgrade &= moves;
            if *upgrade { vec![(Flag::Upgrade, upgrade)] } else { Vec::new() }
        }
        Refusal::SameThorn { replace, .. } | Refusal::ForkAndUpstream { replace, .. } => {
            *replace &= !o.replace;
            if *replace { vec![(Flag::Replace, replace)] } else { Vec::new() }
        }
        Refusal::FleshHasDependents { no_deps, .. } => {
            *no_deps &= !o.no_deps;
            if *no_deps { vec![(Flag::NoDeps, no_deps)] } else { Vec::new() }
        }
        Refusal::Unreadable { needs, ways } => {
            let fork = needs.iter().any(|n| matches!(n.what, UnreadFor::ForkCheck { .. }));
            *ways = Ways { upgrade: moves, replace: fork && !o.replace, no_deps: !o.no_deps };
            let Ways { upgrade, replace, no_deps } = ways;
            [(Flag::Upgrade, upgrade), (Flag::Replace, replace), (Flag::NoDeps, no_deps)]
                .into_iter()
                .filter(|(_, on)| **on)
                .collect()
        }
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Flag {
    Upgrade,
    Replace,
    NoDeps,
}

/// Keeps, of the flags `refusal` suggests, those a search with finds a plan (at most
/// [`WAY_CHECKS`] searches; one not checked is not suggested).
fn check_ways(request: &Request, cat: &mut Catalog, refusal: &mut Refusal) -> Res<()> {
    for (i, (flag, works)) in suggested(refusal, request.options).into_iter().enumerate() {
        if i >= WAY_CHECKS {
            *works = false;
            continue;
        }
        let mut options = request.options;
        match flag {
            Flag::Upgrade => options.upgrade = true,
            Flag::Replace => options.replace = true,
            Flag::NoDeps => options.no_deps = true,
        }
        let with = Request { options, ..*request };
        *works = run(&with, cat, Mode::Check)?.is_ok();
    }
    Ok(())
}

/// One search: a plan, or the refusal (`None` when checking a way past).
fn run(request: &Request, cat: &mut Catalog, mode: Mode) -> Res<Result<Plan, Option<Refusal>>> {
    let mut present: BTreeMap<PkgId, Rc<Present>> = BTreeMap::new();
    for p in request.present {
        let mut p = p.clone();
        request.rules.apply(&mut p.release);
        let id = p.release.id.clone();
        if present.insert(id.clone(), Rc::new(p)).is_some() {
            bail!("{id} is in the installation twice");
        }
    }
    let present_rel = present.iter().map(|(id, p)| (id.clone(), Rc::new(p.release.clone()))).collect();
    let budget = MIN_STEPS.max(PACKAGE_STEPS / (present.len() + request.specs.len() + 1));
    #[cfg(test)]
    let budget = BUDGET.with(|b| b.get()).unwrap_or(budget);
    cat.closed = mode == Mode::Check;
    cat.searching = false;
    let mut s = Search {
        request,
        cat,
        specs: Vec::new(),
        named_needs: Vec::new(),
        upgrade_needs: Vec::new(),
        holders: BTreeMap::new(),
        present_passive: BTreeMap::new(),
        parsed: RefCell::new(HashMap::new()),
        need_cache: RefCell::new(HashMap::new()),
        fork_cache: RefCell::new(HashMap::new()),
        universe: Vec::new(),
        forkable: BTreeSet::new(),
        flesh_possible: false,
        may_be_flesh: BTreeSet::new(),
        steps: 0,
        budget,
        started: Instant::now(),
        timed_out: false,
        exhausted: false,
        best: None,
        present,
        present_rel,
        unreadable: BTreeMap::new(),
        tried: BTreeSet::new(),
        wanted_thorns: BTreeMap::new(),
        followed: BTreeMap::new(),
        hits: BTreeSet::new(),
    };
    s.cat.ensure(request.specs.iter().map(|x| x.id.clone()))?;
    // A named tip is read once, up front: the need is for the commit it is at now.
    for spec in request.specs {
        let req = match &spec.req {
            Req::Tip(tip) if s.cat.known(&spec.id) => match s.cat.snapshot(&spec.id, tip)? {
                Ok(r) => Req::Commit(r.commit.clone()),
                Err(_) => spec.req.clone(),
            },
            req => req.clone(),
        };
        s.specs.push(NamedSpec { id: spec.id.clone(), req, thorns: spec.thorns.clone() });
    }
    // The universe first: which alternative a present fork holds is read
    // from its upstream's published versions, loaded with it.
    s.build_universe()?;
    s.prepare();
    // Every read is done: the search never reads (nor waits for the server's read limit), and
    // its wall-time net starts now.
    s.cat.searching = true;
    if mode == Mode::Main {
        s.cat.facts.searching();
    }
    s.started = Instant::now();
    let mut assign = Assign::default();
    let outcome = s.search(&mut assign)?;
    #[cfg(test)]
    if mode == Mode::Main {
        STEPS.with(|c| c.set(s.steps));
    }
    match outcome {
        // A plan found is a plan, whatever branches failed for want of a read on the way (its
        // notes say which releases that left out).
        Outcome::Found(plan) => Ok(Ok(plan)),
        Outcome::Failed(_) if mode == Mode::Check => Ok(Err(None)),
        // No plan, and some branch failed for want of a read: that is what to say.
        Outcome::Failed(_) if !s.hits.is_empty() => Ok(Err(Some(s.unreadable_refusal()))),
        Outcome::Failed(_) => Ok(Err(Some(s.refusal()?))),
    }
}

/// The releases among `candidates` that meet `narrow`: usable (or `present`, installed whatever
/// its state), fitting, and yanked only for an exact one of `exact` (or installed).
fn eligible<'r>(
    candidates: &'r [Rel],
    narrow: &[Req],
    exact: &[Req],
    present: Option<&Release>,
) -> Vec<&'r Rel> {
    let installed = |r: &Release| present.is_some_and(|p| p.same(r));
    candidates
        .iter()
        .filter(|r| r.usable || installed(r))
        .filter(|r| narrow.iter().all(|q| q.fit(r) != Fit::No))
        .filter(|r| !r.yanked || installed(r) || exact.iter().any(|q| q.is_exact() && q.fit(r) == Fit::Yes))
        .collect()
}

/// In order of preference: approved before awaiting review, then newest first (versions by
/// semver, above any release at no version); ties keep the order they came in.
fn preferred(mut rs: Vec<&Rel>) -> Vec<&Rel> {
    let rank = |r: &Release| (r.pending, std::cmp::Reverse(r.at.version().map(|v| v.version.clone())));
    rs.sort_by_key(|r| rank(r));
    rs
}

/// The registry as the resolver sees it: releases with the rules applied, loaded once.
struct Catalog<'a> {
    facts: &'a mut dyn Facts,
    rules: &'a Rules,
    /// `None`: Cactarium has no such package.
    loaded: BTreeMap<PkgId, Option<Vec<Rel>>>,
    /// Snapshots read (or found unreadable, with why), by package and what was asked, and by the
    /// commit found.
    snapshots: BTreeMap<(PkgId, String), Result<Rel, Unanswered>>,
    untrusted: BTreeSet<(PkgId, String)>,
    /// No read at a commit beyond those made (a search checking a way past).
    closed: bool,
    /// The search is on: every read it needs was made before it.
    searching: bool,
}

impl<'a> Catalog<'a> {
    fn new(facts: &'a mut dyn Facts, rules: &'a Rules) -> Catalog<'a> {
        let (loaded, snapshots, untrusted) = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
        Catalog { facts, rules, loaded, snapshots, untrusted, closed: false, searching: false }
    }

    /// Loads every package not loaded yet, in one wave.
    fn ensure(&mut self, ids: impl IntoIterator<Item = PkgId>) -> Res<()> {
        let want: BTreeSet<PkgId> =
            ids.into_iter().filter(|id| !id.is_repo() && !self.loaded.contains_key(id)).collect();
        if want.is_empty() {
            return Ok(());
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let want: Vec<PkgId> = want.into_iter().collect();
        self.facts.load(&want)?;
        let rules = self.rules;
        for id in want {
            let releases = self.facts.releases(&id).map(|rs| {
                rs.iter()
                    .cloned()
                    .map(|mut r| {
                        rules.apply(&mut r);
                        Rc::new(r)
                    })
                    .collect()
            });
            self.loaded.insert(id, releases);
        }
        Ok(())
    }

    fn loaded(&self, id: &PkgId) -> bool {
        id.is_repo() || self.loaded.contains_key(id)
    }

    fn releases(&self, id: &PkgId) -> &[Rel] {
        self.loaded.get(id).and_then(|r| r.as_deref()).unwrap_or(&[])
    }

    fn known(&self, id: &PkgId) -> bool {
        matches!(self.loaded.get(id), Some(Some(_)))
    }

    fn is_flesh(&self, id: &PkgId) -> bool {
        self.releases(id).iter().any(|r| r.flesh)
    }

    /// Every published version with its commit, for `ForkOf::stands_in_for`.
    fn published(&self, id: &PkgId) -> Vec<(TrackedVersion, Option<String>)> {
        let published = |r: &Rel| r.at.version().map(|v| (v.clone(), Some(r.commit.clone())));
        self.releases(id).iter().filter_map(published).collect()
    }

    /// The package read at a commit or tip; `Err(why)` when Cactarium cannot read it (a
    /// failed candidate, never a failed command).
    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Result<Rel, Unanswered>> {
        let key = (id.clone(), at.to_owned());
        if let Some(r) = self.snapshots.get(&key) {
            return Ok(r.clone());
        }
        if self.closed || self.searching {
            // Every read the search needs is made before it starts (`build_universe`).
            debug_assert!(self.closed, "{id} read at {at} during the search");
            let why = format!("{id} was not read at {at} in this command");
            return Ok(Err(Unanswered { kind: UnreadKind::Retry, why }));
        }
        let read = match self.facts.snapshot(id, at)? {
            None => Err(self.facts.unanswered(id, at).unwrap_or_else(|| Unanswered {
                kind: UnreadKind::Missing,
                why: format!("Cactarium has no commit {} of {id}", short(at)),
            })),
            Some(snap) => {
                let mut release = snap.release;
                // What was asked must be what came back: a commit that id names (a version frozen
                // there answers as that version), or the tip asked for.
                let answered = match &release.at {
                    At::Tip { name, .. } => name == at,
                    _ => cactup_ecosystem_common::slug::is_commit_prefix(at) && release.commit.starts_with(at),
                };
                if answered {
                    self.rules.apply(&mut release);
                    if snap.untrusted {
                        self.untrusted.insert((id.clone(), release.commit.clone()));
                    }
                    let release = Rc::new(release);
                    // A tip read is also the read at the commit it found.
                    self.snapshots.insert((id.clone(), release.commit.clone()), Ok(release.clone()));
                    Ok(release)
                } else {
                    let why = format!("it answered with {} at commit {}", release.label(), release.commit);
                    Err(Unanswered { kind: UnreadKind::Invalid, why })
                }
            }
        };
        self.snapshots.insert(key, read.clone());
        Ok(read)
    }

    /// The served releases of `t`, plus the read at each commit `reqs` name that no served
    /// release is at; and the commits that could not be read, with why.
    fn candidates(&mut self, t: &PkgId, reqs: &[Req]) -> Res<(Vec<Rel>, Vec<(String, String)>)> {
        let mut candidates: Vec<Rel> = self.releases(t).to_vec();
        let mut unreadable = Vec::new();
        for q in reqs {
            if let Req::Commit(sha) = q
                && self.known(t)
                && !candidates.iter().any(|r| r.commit.starts_with(sha.as_str()))
            {
                match self.snapshot(t, sha)? {
                    Ok(r) => candidates.push(r),
                    Err(u) => unreadable.push((sha.clone(), u.why)),
                }
            }
        }
        Ok((candidates, unreadable))
    }
}

/// What has been decided.
#[derive(Clone, Debug, Default)]
struct Assign {
    releases: BTreeMap<PkgId, Rel>,
    /// (dependent, index of the claim) → (alternative, the rule that put it first).
    any_of: BTreeMap<(PkgId, usize), (usize, ChoiceRule)>,
}

impl Assign {
    fn depth(&self) -> usize {
        self.releases.len() + self.any_of.len()
    }

    fn keys(&self) -> BTreeSet<DKey> {
        let mut out: BTreeSet<DKey> = self.releases.keys().map(|id| DKey::Pkg(id.clone())).collect();
        out.extend(self.any_of.keys().map(|(id, i)| DKey::AnyOf(id.clone(), *i)));
        out
    }
}

/// A decision, as a failure names the ones involved in it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DKey {
    Pkg(PkgId),
    AnyOf(PkgId, usize),
}

/// Where a need comes from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Named,
    UpgradeAll,
    /// A release's claim: `label` is `owner/name@version`.
    Release { label: String, id: PkgId },
}

/// How a need binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// It brings its target in, enables thorns, and constrains.
    Active,
    /// A claim of a present package that is not changing: it constrains a package that moves (it
    /// never refuses one that may not: that predates the command). Under `--no-deps`, a note.
    Passive,
    /// The same, of a present package that might still move and was not decided: checked once
    /// the assignment is complete.
    PassiveFinal,
}

#[derive(Clone, Debug)]
enum Thorns {
    All,
    Some(Vec<String>),
}

fn thorns_of(needs: &[String]) -> Thorns {
    if needs.is_empty() { Thorns::All } else { Thorns::Some(needs.to_vec()) }
}

/// The any-of an answering need comes from.
#[derive(Clone, Debug)]
struct FromAnyOf {
    key: (PkgId, usize),
    label: String,
    alternatives: Vec<PkgId>,
}

/// One need, as a release's claim (or a spec) makes it: built once and shared.
#[derive(Debug)]
struct Need {
    source: Source,
    target: PkgId,
    req: Req,
    thorns: Thorns,
    /// The claim as people read it, and its `needed-by`.
    claim: Option<(String, Vec<String>)>,
    /// The claim's index among its release's claims (`usize::MAX` for a spec).
    index: usize,
    any_of: Option<FromAnyOf>,
}

impl Need {
    fn live(&self, kind: Kind) -> Live {
        let (source, ignore) = match &self.source {
            Source::Named => ("the command line".to_owned(), None),
            Source::UpgradeAll => ("--upgrade-all".to_owned(), None),
            Source::Release { label, id } => {
                let source = if kind == Kind::Active { label.clone() } else { format!("installed {label}") };
                (source, Some(format!("{id}:dependency:{}", self.target)))
            }
        };
        Live { target: self.target.clone(), req: self.req.to_string(), source, ignore }
    }

    fn names(&self) -> Vec<String> {
        match &self.thorns {
            Thorns::All => Vec::new(),
            Thorns::Some(v) => v.clone(),
        }
    }

    fn label(&self) -> Option<&str> {
        match &self.source {
            Source::Release { label, .. } => Some(label),
            _ => None,
        }
    }

    fn source_id(&self) -> Option<&PkgId> {
        match &self.source {
            Source::Release { id, .. } => Some(id),
            _ => None,
        }
    }

    fn why(&self, rule: Option<ChoiceRule>) -> Why {
        match &self.source {
            Source::Named => Why::Named,
            Source::UpgradeAll => Why::UpgradeAll,
            Source::Release { label, id } => {
                let (claim, needed_by) = self.claim.clone().unwrap_or_default();
                let (by, by_id, req) = (label.clone(), id.clone(), self.req.to_string());
                Why::Needed { by, by_id, claim, req, needed_by, rule }
            }
        }
    }
}

/// Who takes a need.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Taker {
    Itself,
    /// A package in play standing in (a fork, or one an override names).
    Other { taker: PkgId, note: String },
    /// A fork the plan requires but has not decided yet.
    Fork(PkgId),
    /// Forks in play, none of which stands in for what the need asks.
    NotStandIn { fork: PkgId },
}

/// A need in a state: how it binds there and who takes it.
#[derive(Clone, Debug)]
struct Rec {
    need: Rc<Need>,
    kind: Kind,
    /// Its source cannot change while this branch stands (decided, named, or never moving).
    stable: bool,
    rule: Option<ChoiceRule>,
    taker: Taker,
    /// The forks and stand-ins looked at for it (their decisions bear on who takes it).
    via: Vec<PkgId>,
}

impl Rec {
    fn taken_by(&self) -> Option<&PkgId> {
        match &self.taker {
            Taker::Itself => Some(&self.need.target),
            Taker::Other { taker, .. } => Some(taker),
            Taker::Fork(f) => Some(f),
            Taker::NotStandIn { .. } => None,
        }
    }
}

/// A dependency claim of a release, read once.
#[derive(Debug)]
struct Parsed {
    index: usize,
    needed_by: Vec<String>,
    label: String,
    target: String,
    body: Body,
}

#[derive(Debug)]
enum Body {
    Single(PkgId, Req, Thorns),
    AnyOf(Vec<(PkgId, Req, Thorns)>),
    Bad(String),
}

fn parse_claims(r: &Release) -> Vec<Parsed> {
    let mut out = Vec::new();
    for (index, c) in r.claims.iter().enumerate() {
        if c.kind != "dependency" || c.into.is_some() {
            continue;
        }
        let body = if c.alternatives.is_empty() {
            match claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) {
                Ok((id, req)) => Body::Single(id, req, thorns_of(&c.needs)),
                Err(e) => Body::Bad(e),
            }
        } else {
            let alts: Result<Vec<_>, String> = (c.alternatives.iter())
                .map(|a| {
                    let (id, req) = claim_target(&a.target, a.range.as_deref(), a.sha.as_deref())?;
                    Ok((id, req, thorns_of(&a.needs)))
                })
                .collect();
            match alts {
                Ok(alts) => Body::AnyOf(alts),
                Err(e) => Body::Bad(e),
            }
        };
        let (needed_by, label, target) = (c.needed_by.clone(), claim_label(c), c.target.clone());
        out.push(Parsed { index, needed_by, label, target, body });
    }
    out
}

/// A package in play.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) release: Rel,
    pub(super) enabled: BTreeSet<String>,
    /// What the installation has; `None` for a package the plan brings in.
    pub(super) present: Option<Rc<Present>>,
    /// Its place in the plan (`usize::MAX`: no step).
    pub(super) order: usize,
    /// Its release was decided (rather than a present one nothing has come to yet).
    decided: bool,
}

impl Entry {
    /// Brought in by the plan, or present at another release now.
    pub(super) fn is_new(&self) -> bool {
        self.present.as_ref().is_none_or(|p| !p.release.same(&self.release))
    }

    /// Whether this thorn is new to the installation.
    pub(super) fn thorn_is_new(&self, thorn: &str) -> bool {
        self.is_new() || self.present.as_ref().is_some_and(|p| !contains(&p.enabled, thorn))
    }

    /// Brought in, moved, or given thorns it did not have.
    pub(super) fn changed(&self) -> bool {
        self.is_new() || self.enabled.iter().any(|t| self.thorn_is_new(t))
    }

    /// The base repo it is, unchanged.
    pub(super) fn base_repo(&self) -> Option<&str> {
        match self.present.as_ref().map(|p| &p.origin) {
            Some(Origin::Base { repo }) if !self.is_new() => Some(repo),
            _ => None,
        }
    }
}

/// An any-of waiting for a decision.
#[derive(Clone, Debug)]
struct OpenAnyOf {
    key: (PkgId, usize),
    release: Rel,
}

/// Everything the assignment implies.
#[derive(Clone, Debug, Default)]
struct Derived {
    chosen: BTreeMap<PkgId, Entry>,
    needs: Vec<Rec>,
    /// The packages the plan brings in (decided or not): every taker that is not present.
    required: BTreeSet<PkgId>,
    /// Thorns needs give each taker (`""`: all of them).
    given: BTreeMap<PkgId, BTreeSet<String>>,
    touched: BTreeSet<PkgId>,
    open: Vec<PkgId>,
    open_any_of: Vec<OpenAnyOf>,
    replaced: Vec<(Replaced, PkgId, Vec<String>, bool)>,
    /// Upstream → the forks in play claiming it, in id order.
    forks: BTreeMap<PkgId, Vec<PkgId>>,
    /// Packages whose versions are needed and not loaded yet.
    want: BTreeSet<PkgId>,
    failure: Option<Failure>,
}

/// A refusal waiting to be chosen as the one to report.
#[derive(Clone, Debug)]
enum Pending {
    Refusal(Refusal),
    /// Providers are asked for only if this is the one reported.
    Unclaimed(Vec<(String, PkgId, Requirement)>),
}

#[derive(Clone, Debug)]
struct Failure {
    refusal: Pending,
    /// The constraints behind it.
    lives: Vec<Live>,
    /// "These do not fit together", where another release would have met every constraint.
    weak: bool,
    /// The packages it is about, and the needs (indices into the state's) it reads.
    parties: Vec<PkgId>,
    needs: Vec<usize>,
    /// Any decision may bear on it (no jumping back past any).
    all: bool,
    /// Parties whose part in it no decision can change: present packages that keep the clashing
    /// thorn whatever release they end up at (see [`Search::keeps_thorn`]).
    fixed: Vec<PkgId>,
    /// It failed for want of the claims of a stand-in unread by Cactarium: what needed
    /// them, and the release (or the stand-in kept where it is) the branch had chosen.
    unread: Vec<UnreadNeed>,
}

impl Failure {
    fn of(refusal: Refusal, lives: Vec<Live>, parties: Vec<PkgId>) -> Failure {
        let refusal = Pending::Refusal(refusal);
        let (needs, fixed) = (Vec::new(), Vec::new());
        Failure { refusal, lives, weak: false, parties, needs, all: false, fixed, unread: Vec::new() }
    }

    fn everything(mut self) -> Failure {
        self.all = true;
        self
    }

    /// Requirements nothing meets: any decision may bear on what provides them.
    fn unclaimed(unmet: Vec<(String, PkgId, Requirement)>) -> Failure {
        let (lives, parties, needs) = (Vec::new(), Vec::new(), Vec::new());
        let fixed = Vec::new();
        Failure { refusal: Pending::Unclaimed(unmet), lives, weak: false, parties, needs, all: true, fixed, unread: Vec::new() }
    }
}

enum Outcome {
    Found(Plan),
    /// The decisions the failure below depends on.
    Failed(BTreeSet<DKey>),
}

struct Search<'a, 'c, 'f> {
    request: &'a Request<'a>,
    present: BTreeMap<PkgId, Rc<Present>>,
    /// Each present package's release, shared.
    present_rel: BTreeMap<PkgId, Rel>,
    cat: &'c mut Catalog<'f>,
    /// The named specs, tips read.
    specs: Vec<NamedSpec>,
    named_needs: Vec<Rc<Need>>,
    upgrade_needs: Vec<Rc<Need>>,
    /// For each present package's any-of (dependent, claim index), the alternative it holds.
    holders: BTreeMap<(PkgId, usize), usize>,
    /// Each present package's passive needs: its claims that apply to its enabled thorns.
    present_passive: BTreeMap<PkgId, Vec<(usize, Rc<Need>)>>,
    // Caches keyed by the address of a release, which stays put (and alive) for the whole search:
    // every release lives in the catalog, the present map or a snapshot.
    parsed: RefCell<HashMap<*const Release, Rc<Vec<Parsed>>>>,
    need_cache: RefCell<HashMap<(*const Release, usize, usize), Rc<Need>>>,
    fork_cache: RefCell<HashMap<*const Release, Rc<Vec<(PkgId, ForkOf)>>>>,
    /// Every release that could come into play: the present ones, and every version of every
    /// package reachable from them or the named specs through dependency claims (any
    /// alternative, any version, commits read), all loaded before the search.
    universe: Vec<Rel>,
    /// The packages some release of the universe claims to fork: a fork could take their place.
    forkable: BTreeSet<PkgId>,
    /// Some package the plan could bring in is a flesh: the installation's flesh could be replaced.
    flesh_possible: bool,
    /// The packages some release of which in the universe is a flesh.
    may_be_flesh: BTreeSet<PkgId>,
    steps: usize,
    budget: usize,
    started: Instant,
    /// The wall-time net was reached (rather than the step budget).
    timed_out: bool,
    exhausted: bool,
    /// The failure to report: (specific, depth, failure); the first of the best wins.
    best: Option<(bool, usize, Failure)>,
    /// Present stand-ins the search depends on that could not be read, with why.
    unreadable: BTreeMap<PkgId, Unanswered>,
    /// Present stand-ins already read (or tried).
    tried: BTreeSet<PkgId>,
    /// The thorns of each present package some need of the universe names (`""`: all of them):
    /// what may be enabled, whose claims count.
    wanted_thorns: BTreeMap<PkgId, BTreeSet<String>>,
    /// Each stand-in read, with the thorns its claims were followed for (`None`: all).
    followed: BTreeMap<PkgId, Option<BTreeSet<String>>>,
    /// What branches of the search failed for want of a read.
    hits: BTreeSet<UnreadNeed>,
}

/// The dependency as people read it: `et/carpet@^1.2`, or `any of et/carpet, et/cactuspugh`.
pub(super) fn claim_label(c: &ClaimInfo) -> String {
    if c.alternatives.is_empty() {
        target_text(&c.target, c.range.as_deref(), c.sha.as_deref())
    } else {
        let text = |a: &AlternativeInfo| target_text(&a.target, a.range.as_deref(), a.sha.as_deref());
        let alts: Vec<String> = c.alternatives.iter().map(text).collect();
        format!("any of {}", alts.join(", "))
    }
}

fn contains(set: &BTreeSet<String>, name: &str) -> bool {
    set.iter().any(|t| t.eq_ignore_ascii_case(name))
}

fn intersects(names: &[String], set: &BTreeSet<String>) -> bool {
    names.iter().any(|n| contains(set, n))
}

/// The thorns a need names that `r` lacks.
fn missing_thorns(r: &Release, n: &Need) -> Vec<String> {
    n.names().into_iter().filter(|th| r.thorn(th).is_none()).collect()
}

/// Whether `r` fails a need: outside its range, or without a thorn it names.
fn violates(r: &Release, n: &Need) -> bool {
    n.req.fit(r) == Fit::No || !missing_thorns(r, n).is_empty()
}

/// The ids of the entries `keep` accepts.
fn ids_where(chosen: &BTreeMap<PkgId, Entry>, keep: impl Fn(&PkgId, &Entry) -> bool) -> Vec<PkgId> {
    chosen.iter().filter(|(id, e)| keep(id, e)).map(|(id, _)| id.clone()).collect()
}

/// A library at a commit an installed placement holds: the served release there, if any.
fn held_release(cat: &Catalog, lib: &PkgId, commit: &str) -> Release {
    if let Some(r) = cat.releases(lib).iter().find(|r| r.commit == commit) {
        return (**r).clone();
    }
    Release {
        id: lib.clone(),
        at: At::Unknown(commit.to_owned()),
        commit: commit.to_owned(),
        tag: None,
        url: String::new(),
        contact: None,
        flesh: false,
        library: true,
        yanked: false,
        yank_reason: None,
        pending: false,
        usable: true,
        thorns: Vec::new(),
        claims: Vec::new(),
    }
}

impl Search<'_, '_, '_> {
    fn named(&self, id: &PkgId) -> Option<&NamedSpec> {
        self.specs.iter().find(|s| &s.id == id)
    }

    /// Whether a present package could ever move in this command: named, the flesh, or any under
    /// `--upgrade`/`--upgrade-all`. One that cannot has its claims narrow at once.
    fn may_move(&self, id: &PkgId) -> bool {
        let options = self.request.options;
        options.upgrade
            || options.upgrade_all
            || (self.named(id).is_some() && !self.held_base(id))
            || self.present_rel.get(id).is_some_and(|r| r.flesh)
    }

    /// A base package that stays where the base has it, named or not: no `--upgrade`,
    /// `--upgrade-all` or `--replace` says otherwise.
    fn held_base(&self, id: &PkgId) -> bool {
        let options = self.request.options;
        !(options.upgrade || options.upgrade_all || options.replace)
            && self.present.get(id).is_some_and(|p| matches!(p.origin, Origin::Base { .. }) && !p.release.flesh)
    }

    /// Reads a present stand-in at its commit, once: the release read takes its place (its
    /// enabled thorns keep the facts the tree has), and what it claims for the thorns that are
    /// or may be enabled joins the universe. One that cannot be read is noted: a branch that
    /// needs its claims then fails for want of them.
    fn read_stand_in(
        &mut self,
        id: &PkgId,
        releases: &mut Vec<Rel>,
        frontier: &mut BTreeSet<PkgId>,
        commits: &mut Vec<(PkgId, String)>,
    ) -> Res<()> {
        let Some(p) = self.present.get(id).filter(|p| p.stand_in && !id.is_repo()).cloned() else { return Ok(()) };
        self.tried.insert(id.clone());
        let commit = p.release.commit.clone();
        if commit.is_empty() {
            let why = format!("the commit of {id} is not known");
            self.unreadable.insert(id.clone(), Unanswered { kind: UnreadKind::Invalid, why });
            return Ok(());
        }
        match self.cat.snapshot(id, &commit)? {
            Ok(read) => {
                let mut release = (*read).clone();
                for t in release.thorns.iter_mut().filter(|t| contains(&p.enabled, &t.thorn)) {
                    if let Some(local) = p.release.thorn(&t.thorn) {
                        t.facts = local.facts.clone();
                    }
                }
                let read = Present { release, stand_in: false, ..(*p).clone() };
                let rel = Rc::new(read.release.clone());
                releases.retain(|r| r.id != *id);
                self.present.insert(id.clone(), Rc::new(read));
                self.present_rel.insert(id.clone(), rel.clone());
                // Only what its enabled thorns, and those a need names, claim: reading a
                // package does not bring in the needs of thorns nothing enables. Followed again
                // when more of its thorns come to be wanted.
                let counted = self.counted_thorns(id);
                self.follow_where(&rel, frontier, commits, counted.as_ref());
                self.followed.insert(id.clone(), counted);
                releases.push(rel);
            }
            Err(mut u) => {
                if u.kind == UnreadKind::Missing {
                    u.why = format!("{id} is at a commit unknown to Cactarium (is it pushed?)");
                }
                self.unreadable.insert(id.clone(), u);
            }
        }
        Ok(())
    }

    /// The thorns of present package `id` whose claims count: its enabled ones and those a need
    /// names; `None` when a need asks for every thorn.
    fn counted_thorns(&self, id: &PkgId) -> Option<BTreeSet<String>> {
        let wanted = self.wanted_thorns.get(id);
        if wanted.is_some_and(|w| w.contains("")) {
            return None;
        }
        let mut out: BTreeSet<String> = self.present.get(id).map(|p| p.enabled.clone()).unwrap_or_default();
        out.extend(wanted.into_iter().flatten().cloned());
        Some(out)
    }

    /// The refusal when no plan was found and some branch failed for want of a read: what needed
    /// which stand-in, for what, and why it was not read (its ways past are checked after).
    fn unreadable_refusal(&self) -> Refusal {
        Refusal::Unreadable { needs: self.hits.iter().cloned().collect(), ways: Ways::default() }
    }

    /// A present stand-in not read yet.
    fn is_stand_in(&self, id: &PkgId) -> bool {
        !id.is_repo() && self.present.get(id).is_some_and(|p| p.stand_in) && !self.tried.contains(id)
    }

    /// Records, for every present package, the thorns some need of a release of `releases` names
    /// of it (a dependency or any-of alternative; `""`: every thorn), to a fixpoint: a present
    /// release's claims count only for its enabled thorns and those wanted of it, which grow as
    /// they are found. An over-approximation of what the plan may enable. Returns the stand-ins
    /// not read yet that a need wants a thorn of that is not enabled (or every thorn).
    fn needed_stand_ins(&mut self, releases: &[Rel]) -> BTreeSet<PkgId> {
        let mut forkers: BTreeMap<String, Vec<PkgId>> = BTreeMap::new();
        for (id, r) in &self.present_rel {
            for c in r.claims.iter().filter(|c| c.kind == "fork-of") {
                forkers.entry(c.target.clone()).or_default().push(id.clone());
            }
        }
        loop {
            let mut wanted: Vec<(PkgId, String)> = Vec::new();
            for r in releases {
                let present = self.present_rel.get(&r.id).is_some_and(|p| Rc::ptr_eq(p, r));
                let counted = if present { self.counted_thorns(&r.id) } else { None };
                for c in self.parsed(r).iter() {
                    if let Some(counted) = &counted
                        && !(c.needed_by.is_empty() || intersects(&c.needed_by, counted))
                    {
                        continue;
                    }
                    let targets: Vec<(&PkgId, &Thorns)> = match &c.body {
                        Body::Single(t, _, th) => vec![(t, th)],
                        Body::AnyOf(alts) => alts.iter().map(|(t, _, th)| (t, th)).collect(),
                        Body::Bad(_) => Vec::new(),
                    };
                    for (target, th) in targets {
                        for t in self.may_take(target, &forkers) {
                            match th {
                                Thorns::All => wanted.push((t.clone(), String::new())),
                                Thorns::Some(names) => wanted.extend(names.iter().map(|n| (t.clone(), n.clone()))),
                            }
                        }
                    }
                }
            }
            let mut grew = false;
            for (id, thorn) in wanted {
                grew |= self.wanted_thorns.entry(id).or_default().insert(thorn);
            }
            if !grew {
                break;
            }
        }
        let wants = |id: &PkgId, p: &Present| {
            self.wanted_thorns.get(id).is_some_and(|w| w.iter().any(|n| n.is_empty() || !contains(&p.enabled, n)))
        };
        (self.present.iter())
            .filter(|(id, p)| self.is_stand_in(id) && wants(id, p))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The present packages that may take a need on `t`: itself, one an override says satisfies
    /// it, one claiming to fork it.
    /// `forkers`: the present packages claiming to fork each package.
    fn may_take(&self, t: &PkgId, forkers: &BTreeMap<String, Vec<PkgId>>) -> Vec<PkgId> {
        let mut out: Vec<PkgId> = Vec::new();
        if self.present.contains_key(t) {
            out.push(t.clone());
        }
        out.extend(self.request.rules.satisfiers(t).filter(|x| self.present.contains_key(*x)).cloned());
        out.extend(forkers.get(t.as_str()).into_iter().flatten().cloned());
        out
    }

    /// The present stand-ins that may be forks of a package the plan could bring in: no fork
    /// claim known, and an enabled thorn named as a thorn of a release of a package not present
    /// (whether it is a fork decides whether the two clash).
    fn fork_stand_ins(&self, releases: &[Rel]) -> BTreeSet<PkgId> {
        let mut out = BTreeSet::new();
        for (id, p) in self.present.iter().filter(|(id, p)| p.stand_in && !id.is_repo()) {
            if p.release.claims.iter().any(|c| c.kind == "fork-of") {
                continue;
            }
            let shares = releases
                .iter()
                .filter(|r| !self.present.contains_key(&r.id))
                .any(|r| r.thorns.iter().any(|t| contains(&p.enabled, &t.thorn)));
            if shares {
                out.insert(id.clone());
            }
        }
        out
    }

    fn parsed(&self, r: &Rel) -> Rc<Vec<Parsed>> {
        let key = Rc::as_ptr(r);
        if let Some(p) = self.parsed.borrow().get(&key) {
            return p.clone();
        }
        let p = Rc::new(parse_claims(r));
        self.parsed.borrow_mut().insert(key, p.clone());
        p
    }

    /// `r`'s fork-of claims that read, by upstream.
    fn fork_claims_of(&self, r: &Rel) -> Rc<Vec<(PkgId, ForkOf)>> {
        let key = Rc::as_ptr(r);
        if let Some(f) = self.fork_cache.borrow().get(&key) {
            return f.clone();
        }
        let upstreams: BTreeSet<PkgId> =
            r.claims.iter().filter(|c| c.kind == "fork-of").filter_map(|c| PkgId::parse(&c.target).ok()).collect();
        let mut out: Vec<(PkgId, ForkOf)> = Vec::new();
        for up in upstreams {
            out.extend(fork_claims(r, &up).into_iter().map(|f| (up.clone(), f)));
        }
        let out = Rc::new(out);
        self.fork_cache.borrow_mut().insert(key, out.clone());
        out
    }

    /// The need claim `p` of release `r` makes (for an any-of, its alternative `alt`), built once.
    fn need_of(&self, r: &Rel, p: &Parsed, alt: Option<usize>) -> Option<Rc<Need>> {
        let key = (Rc::as_ptr(r), p.index, alt.unwrap_or(usize::MAX));
        if let Some(n) = self.need_cache.borrow().get(&key) {
            return Some(n.clone());
        }
        let (target, req, thorns, any_of) = match (&p.body, alt) {
            (Body::Single(t, q, th), None) => (t.clone(), q.clone(), th.clone(), None),
            (Body::AnyOf(alts), Some(i)) => {
                let (t, q, th) = alts.get(i)?;
                let alternatives = alts.iter().map(|(id, _, _)| id.clone()).collect();
                let from = FromAnyOf { key: (r.id.clone(), p.index), label: p.label.clone(), alternatives };
                (t.clone(), q.clone(), th.clone(), Some(from))
            }
            _ => return None,
        };
        let need = Rc::new(Need {
            source: Source::Release { label: r.label(), id: r.id.clone() },
            target,
            req,
            thorns,
            claim: Some((p.label.clone(), p.needed_by.clone())),
            index: p.index,
            any_of,
        });
        self.need_cache.borrow_mut().insert(key, need.clone());
        Some(need)
    }

    /// What does not depend on the assignment: the named needs, `--upgrade-all`'s, the
    /// alternatives present packages hold, and their passive needs.
    fn prepare(&mut self) {
        for spec in &self.specs {
            let thorns = spec.thorns.clone().map_or(Thorns::All, Thorns::Some);
            let (target, req) = (spec.id.clone(), spec.req.clone());
            let (claim, index, any_of) = (None, usize::MAX, None);
            let need = Need { source: Source::Named, target, req, thorns, claim, index, any_of };
            self.named_needs.push(Rc::new(need));
        }
        if self.request.options.upgrade_all {
            for (id, p) in &self.present {
                let At::Version(v) = &p.release.at else { continue };
                // Any version on the track it is on.
                let mut req = TrackedReq::any();
                req.alts[0].track = v.track.clone();
                let thorns = Thorns::Some(p.enabled.iter().cloned().collect());
                let (target, req) = (id.clone(), Req::Range(req));
                let index = usize::MAX;
                let (claim, any_of) = (None, None);
                let need = Need { source: Source::UpgradeAll, target, req, thorns, claim, index, any_of };
                self.upgrade_needs.push(Rc::new(need));
            }
        }
        self.holders = self.present_holders();
        let mut passive = BTreeMap::new();
        for (id, p) in &self.present {
            let r = self.present_rel[id].clone();
            let mut needs = Vec::new();
            for c in self.parsed(&r).iter() {
                if !(c.needed_by.is_empty() || intersects(&c.needed_by, &p.enabled)) {
                    continue;
                }
                let alt = match &c.body {
                    Body::Single(..) => None,
                    Body::AnyOf(_) => match self.holders.get(&(id.clone(), c.index)) {
                        Some(alt) => Some(*alt),
                        None => continue,
                    },
                    // A passive claim nobody can read predates this command.
                    Body::Bad(_) => continue,
                };
                if let Some(n) = self.need_of(&r, c, alt) {
                    needs.push((c.index, n));
                }
            }
            passive.insert(id.clone(), needs);
        }
        self.present_passive = passive;
    }

    /// For each any-of of each present package, the first alternative a present package holds at
    /// a release that fits (or a present fork stands in for): its passive constraint goes there.
    fn present_holders(&self) -> BTreeMap<(PkgId, usize), usize> {
        let mut out = BTreeMap::new();
        for (id, r) in &self.present_rel {
            for c in self.parsed(r).iter() {
                let Body::AnyOf(alts) = &c.body else { continue };
                let held = alts.iter().position(|(target, req, _)| match self.present_rel.get(target) {
                    Some(t) => req.fit(t) != Fit::No,
                    None => self.present_rel.values().any(|f| {
                        let published = self.cat.published(target);
                        let stands = |(up, fork): &(PkgId, ForkOf)| {
                            let dep = target.slug(req);
                            up == target && dep.is_some_and(|dep| fork.stands_in_for(&dep, &published))
                        };
                        self.fork_claims_of(f).iter().any(stands)
                    }),
                });
                if let Some(alt) = held {
                    out.insert((id.clone(), c.index), alt);
                }
            }
        }
        out
    }

    /// Loads every release that could come into play, in waves: the packages the named specs
    /// name, the present packages that may move, and whatever any version of those (or a present
    /// release) depends on, through every alternative and every commit a claim names; then notes
    /// which packages a fork could displace and whether a flesh could come in.
    fn build_universe(&mut self) -> Res<()> {
        let mut releases: Vec<Rel> = self.present_rel.values().cloned().collect();
        let mut frontier: BTreeSet<PkgId> = self.specs.iter().map(|s| s.id.clone()).collect();
        frontier.extend(self.present.keys().filter(|id| self.may_move(id)).cloned());
        let mut commits: Vec<(PkgId, String)> = (self.specs.iter())
            .filter_map(|s| match &s.req {
                Req::Commit(sha) => Some((s.id.clone(), sha.clone())),
                _ => None,
            })
            .collect();
        for r in releases.clone() {
            self.follow(&r, &mut frontier, &mut commits);
        }
        let named: Vec<PkgId> = self.specs.iter().map(|s| s.id.clone()).collect();
        // A named present package's thorns are wanted of it.
        for spec in self.specs.iter().filter(|s| self.present.contains_key(&s.id)) {
            let names = spec.thorns.clone().unwrap_or_else(|| vec![String::new()]);
            self.wanted_thorns.entry(spec.id.clone()).or_default().extend(names);
        }
        let no_deps = self.request.options.no_deps;
        let mut seen: BTreeSet<PkgId> = BTreeSet::new();
        let mut read: BTreeSet<(PkgId, String)> = BTreeSet::new();
        // Reads follow what the plan depends on, to a fixpoint, in a stable order: the named
        // stand-ins first (what one has decides what naming it means), then those a need names a
        // thorn of, each group by id; after every read, the triggers are looked at again, and a
        // read stand-in some of whose thorns came to be wanted has their claims followed. Each is
        // tried once, whatever its read does.
        let mut tried: BTreeSet<PkgId> = BTreeSet::new();
        loop {
            let named_first = named.iter().filter(|id| self.is_stand_in(id) && !tried.contains(*id)).min().cloned();
            let needed = if no_deps { BTreeSet::new() } else { self.needed_stand_ins(&releases) };
            if let Some(id) = named_first.or_else(|| needed.into_iter().find(|id| !tried.contains(id))) {
                if gix::interrupt::is_triggered() {
                    bail!("Interrupted");
                }
                tried.insert(id.clone());
                self.read_stand_in(&id, &mut releases, &mut frontier, &mut commits)?;
                continue;
            }
            let grown: Vec<(PkgId, Option<BTreeSet<String>>)> = (self.followed.iter())
                .map(|(id, before)| (id, before, self.counted_thorns(id)))
                .filter(|(_, before, now)| before != &now)
                .map(|(id, _, now)| (id.clone(), now))
                .collect();
            for (id, now) in grown {
                let rel = self.present_rel[&id].clone();
                self.follow_where(&rel, &mut frontier, &mut commits, now.as_ref());
                self.followed.insert(id, now);
            }
            self.follow_present_libraries(&mut frontier, &mut commits);
            // A present package that cannot move is only its present release.
            let wave: Vec<PkgId> = (frontier.iter())
                .filter(|id| !seen.contains(*id) && !id.is_repo())
                .filter(|id| !self.present.contains_key(*id) || self.may_move(id))
                .cloned()
                .collect();
            let reads: Vec<(PkgId, String)> =
                std::mem::take(&mut commits).into_iter().filter(|k| !read.contains(k)).collect();
            if wave.is_empty() && reads.is_empty() {
                // Nothing left to load: the stand-ins that may be forks of what the universe
                // could bring in are read, and what they claim is followed in turn.
                let forks: BTreeSet<PkgId> =
                    (self.fork_stand_ins(&releases).into_iter()).filter(|id| !no_deps && !tried.contains(id)).collect();
                let Some(id) = forks.into_iter().next() else { break };
                if gix::interrupt::is_triggered() {
                    bail!("Interrupted");
                }
                tried.insert(id.clone());
                self.read_stand_in(&id, &mut releases, &mut frontier, &mut commits)?;
                continue;
            }
            frontier.clear();
            seen.extend(wave.iter().cloned());
            // Each commit read once; its package's versions load with the wave's.
            let reads: Vec<(PkgId, String)> = reads.into_iter().filter(|k| read.insert(k.clone())).collect();
            self.cat.ensure(wave.iter().cloned().chain(reads.iter().map(|(id, _)| id.clone())))?;
            for id in &wave {
                for r in self.cat.releases(id).to_vec() {
                    self.follow(&r, &mut frontier, &mut commits);
                    releases.push(r);
                }
            }
            // One request per commit read, so an interrupt is honored between them.
            for (id, sha) in reads {
                if gix::interrupt::is_triggered() {
                    bail!("Interrupted");
                }
                let served = self.cat.releases(&id).iter().any(|r| r.commit.starts_with(sha.as_str()));
                if !self.cat.known(&id) || served {
                    continue;
                }
                if let Ok(r) = self.cat.snapshot(&id, &sha)? {
                    self.follow(&r, &mut frontier, &mut commits);
                    releases.push(r);
                }
            }
        }
        for r in &releases {
            self.forkable.extend(self.fork_claims_of(r).iter().map(|(up, _)| up.clone()));
        }
        self.flesh_possible = releases.iter().any(|r| r.flesh && !self.present.contains_key(&r.id));
        self.may_be_flesh = releases.iter().filter(|r| r.flesh).map(|r| r.id.clone()).collect();
        self.universe = releases;
        Ok(())
    }

    /// The packages `r`'s dependency claims name (every alternative), and the commits they ask.
    fn follow(&self, r: &Rel, frontier: &mut BTreeSet<PkgId>, commits: &mut Vec<(PkgId, String)>) {
        self.follow_where(r, frontier, commits, None);
    }

    /// [`Self::follow`], counting only the claims of `counted` thorns (and those of every thorn);
    /// the libraries placed too.
    fn follow_where(
        &self,
        r: &Rel,
        frontier: &mut BTreeSet<PkgId>,
        commits: &mut Vec<(PkgId, String)>,
        counted: Option<&BTreeSet<String>>,
    ) {
        for c in self.parsed(r).iter() {
            if let Some(counted) = counted
                && !(c.needed_by.is_empty() || intersects(&c.needed_by, counted))
            {
                continue;
            }
            let targets: Vec<(&PkgId, &Req)> = match &c.body {
                Body::Single(t, q, _) => vec![(t, q)],
                Body::AnyOf(alts) => alts.iter().map(|(t, q, _)| (t, q)).collect(),
                Body::Bad(_) => Vec::new(),
            };
            for (t, q) in targets {
                if let Req::Commit(sha) = q {
                    commits.push((t.clone(), sha.clone()));
                }
                frontier.insert(t.clone());
            }
        }
        // The libraries it places, and the commits it places them at: placing them reads nothing
        // more once the search is on. A present release's are followed for the thorns that may
        // be new to it (see [`Self::follow_present_libraries`]).
        if self.present_rel.get(&r.id).is_some_and(|p| Rc::ptr_eq(p, r)) {
            return;
        }
        for c in r.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_some()) {
            if let Some(counted) = counted
                && !(c.needed_by.is_empty() || intersects(&c.needed_by, counted))
            {
                continue;
            }
            let Ok((lib, q)) = claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) else { continue };
            if let Req::Commit(sha) = q {
                commits.push((lib.clone(), sha));
            }
            frontier.insert(lib);
        }
    }

    /// The libraries present packages place for thorns that may be new to them (wanted of them,
    /// not enabled now), and the commits they place them at: what placing a present package's
    /// new thorns reads.
    fn follow_present_libraries(&self, frontier: &mut BTreeSet<PkgId>, commits: &mut Vec<(PkgId, String)>) {
        for (id, p) in &self.present {
            let Some(wanted) = self.wanted_thorns.get(id) else { continue };
            let all = wanted.contains("");
            let new: BTreeSet<String> = wanted.iter().filter(|n| !contains(&p.enabled, n)).cloned().collect();
            for c in self.present_rel[id].claims.iter().filter(|c| c.kind == "dependency" && c.into.is_some()) {
                if !(all || intersects(&c.needed_by, &new)) {
                    continue;
                }
                let Ok((lib, q)) = claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) else { continue };
                if let Req::Commit(sha) = q {
                    commits.push((lib.clone(), sha));
                }
                frontier.insert(lib);
            }
        }
    }

    /// Whether a fork, or a package an override names, could take this package's place.
    fn displaceable(&self, id: &PkgId) -> bool {
        self.request.rules.satisfiers(id).next().is_some() || self.forkable.contains(id)
    }

    /// Whether some decision other than this package's own could take it out of play, or change
    /// what it holds: a package the plan brings in that could be displaced; a present flesh, when
    /// a flesh could come in; a base package under `--replace`; a present package not decided
    /// that may still move. A failure involving one is not jumped back past anything.
    fn removable(&self, id: &PkgId, d: &Derived) -> bool {
        if !self.present.contains_key(id) {
            return self.displaceable(id);
        }
        !self.stays(id, d) || (!d.chosen.get(id).is_some_and(|e| e.decided) && self.may_move(id))
    }

    /// Whether a flesh the plan brings in could replace this present package: a flesh could come
    /// in, and the package is at a flesh release (decided), or could end up at one (not decided:
    /// any release of it in the universe is a flesh).
    fn flesh_may_replace(&self, id: &PkgId, d: &Derived) -> bool {
        self.flesh_possible
            && match d.chosen.get(id) {
                Some(e) if e.decided => e.release.flesh,
                _ => self.may_be_flesh.contains(id),
            }
    }

    /// Whether a present package stays in play whatever any other decision does: no flesh could
    /// replace it, and it is not a base package `--replace` could give up.
    fn stays(&self, id: &PkgId, d: &Derived) -> bool {
        let Some(p) = self.present.get(id) else { return false };
        let replaced_base = self.request.options.replace && matches!(p.origin, Origin::Base { .. });
        !(self.flesh_may_replace(id, d) || replaced_base)
    }

    /// Whether a present package keeps `thorn` in every plan: it has it enabled now, and no
    /// decision can take it away. An unnamed move carries every enabled thorn (see
    /// [`Self::candidates`]), so only naming the package or its leaving play (see [`Self::stays`])
    /// could. A thorn-name clash with such a package does not depend on its release, nor on
    /// whether it moves.
    fn keeps_thorn(&self, id: &PkgId, thorn: &str, d: &Derived) -> bool {
        self.present.get(id).is_some_and(|p| contains(&p.enabled, thorn))
            && self.named(id).is_none()
            && self.stays(id, d)
    }

    /// A conflict found among `parties`, with the parties whose part in it no decision can change
    /// marked fixed: one that keeps the clashing thorn whatever happens, or an upstream that stays
    /// in play (whether a release forks it does not depend on the upstream's release).
    fn conflict_failure(&self, r: Refusal, parties: Vec<PkgId>, d: &Derived) -> Failure {
        if let Some(f) = self.fork_unknown(&r, &parties, d) {
            return f;
        }
        let fixed = match &r {
            Refusal::SameThorn { thorn, .. } => {
                parties.iter().filter(|p| self.keeps_thorn(p, thorn, d)).cloned().collect()
            }
            Refusal::ForkAndUpstream { upstream, .. } if self.stays(upstream, d) => vec![upstream.clone()],
            _ => Vec::new(),
        };
        Failure { fixed, ..Failure::of(r, Vec::new(), parties) }
    }

    /// Why a present package still stands in (Cactarium did not read it), when it does.
    fn unread(&self, id: &PkgId) -> Option<Unanswered> {
        if !self.present.get(id).is_some_and(|p| p.stand_in) || id.is_repo() {
            return None;
        }
        Some(self.unreadable.get(id).cloned().unwrap_or_else(|| {
            // Every stand-in a need may give thorns is read (or tried) before the search.
            debug_assert!(false, "{id} was never read");
            Unanswered { kind: UnreadKind::Retry, why: format!("{id} was not read") }
        }))
    }

    /// A branch enabling thorns of a present stand-in unread by Cactarium (a need naming
    /// thorns it does not have enabled, or every thorn when it has more): what those thorns depend
    /// on is unknown, so the branch fails for want of it. Its thorns are known (the tree's): a
    /// thorn it does not have is a plain failure. Waits while the stand-in may still move (a
    /// release's claims are known). What failed is the decisions behind the needs, and the
    /// stand-in's own where it could move: never a verdict on the stand-in.
    fn unread_thorns(&self, d: &Derived) -> Option<Failure> {
        // Every stand-in it would need, each with every need asking: what is said does not
        // depend on which comes first.
        let (mut unread, mut needs, mut parties) = (Vec::new(), BTreeSet::new(), Vec::new());
        for (id, names) in &d.given {
            let Some(p) = self.present.get(id).filter(|p| p.stand_in && !id.is_repo()) else { continue };
            let Some(e) = d.chosen.get(id).filter(|e| !e.is_new()) else { continue };
            if !e.decided && self.may_move(id) {
                continue;
            }
            let new: BTreeSet<String> = if names.contains("") {
                e.release.thorn_names().into_iter().filter(|t| !contains(&p.enabled, t)).collect()
            } else {
                names.iter().filter(|n| !contains(&p.enabled, n) && e.release.thorn(n).is_some()).cloned().collect()
            };
            // Thorns it has enabled already: nothing new depends on what it claims.
            if new.is_empty() {
                continue;
            }
            let Some(why) = self.unread(id) else { continue };
            let asks = |r: &Rec| match &r.need.thorns {
                Thorns::All => true,
                Thorns::Some(v) => intersects(v, &new),
            };
            for (i, rec) in (d.needs.iter().enumerate())
                .filter(|(_, r)| r.kind == Kind::Active && r.taken_by() == Some(id) && asks(r))
            {
                needs.insert(i);
                let what = match &rec.need.thorns {
                    Thorns::All => UnreadFor::Every,
                    Thorns::Some(v) => UnreadFor::Thorns(v.iter().filter(|n| contains(&new, n)).cloned().collect()),
                };
                let by = rec.need.live(rec.kind).source;
                unread.push(UnreadNeed { id: id.clone(), package: e.release.label(), by, what, why: why.clone() });
            }
            // Moving it (to a release whose claims are known) is the other way.
            if e.decided && self.movable(id, d) {
                parties.push(id.clone());
            }
        }
        if unread.is_empty() {
            return None;
        }
        let r = Refusal::Unreadable { needs: unread.clone(), ways: Ways::default() };
        let needs = needs.into_iter().collect();
        Some(Failure { needs, unread, ..Failure::of(r, Vec::new(), parties) })
    }

    /// A thorn-name clash between a present stand-in unread by Cactarium and a package a
    /// need brings in (not named, not present): were the stand-in a fork of it, it would stand in
    /// for it and nothing would clash, and only a read tells. What failed is bringing that
    /// package in.
    fn fork_unknown(&self, r: &Refusal, parties: &[PkgId], d: &Derived) -> Option<Failure> {
        let Refusal::SameThorn { thorn, .. } = r else { return None };
        let stand_in = |p: &PkgId| {
            !p.is_repo()
                && self.present.get(p).is_some_and(|x| x.stand_in)
                && d.chosen.get(p).is_some_and(|e| !e.is_new())
        };
        let x = parties.iter().find(|p| stand_in(p))?;
        let other = parties.iter().find(|p| *p != x)?;
        if self.present.contains_key(other) || self.named(other).is_some() {
            return None;
        }
        let why = self.unread(x)?;
        let (i, rec) = (d.needs.iter().enumerate())
            .find(|(_, r)| r.kind == Kind::Active && r.taker == Taker::Itself && &r.need.target == other)?;
        let by = rec.need.live(rec.kind).source;
        let what = UnreadFor::ForkCheck { other: other.clone(), thorn: thorn.clone() };
        let need = UnreadNeed { id: x.clone(), package: d.chosen[x].release.label(), by, what, why };
        let f = Failure::of(r.clone(), Vec::new(), vec![other.clone()]);
        Some(Failure { needs: vec![i], unread: vec![need], ..f })
    }

    /// The notes of a plan: every release the search would have preferred to the one the plan
    /// has (or keeping a present package where it is, before moving it) that needs what
    /// Cactarium did not read. Worked out from the plan and the facts alone, never from which
    /// branches the search happened to explore, so they do not depend on package ids; and from
    /// what each release asks itself, never from constraints the plan has only because it went
    /// without that release.
    fn skipped_notes(&self, d: &Derived) -> Vec<String> {
        if self.unreadable.is_empty() {
            return Vec::new();
        }
        let mut out: BTreeSet<String> = BTreeSet::new();
        for (id, e) in &d.chosen {
            // Only a release the search chose: brought in, or present and free to move.
            if e.present.is_some() && !(e.decided && self.movable(id, d)) {
                continue;
            }
            let ranked = self.ranking(id, e);
            if !ranked.iter().any(|r| r.same(&e.release)) {
                continue;
            }
            for r in ranked.iter().take_while(|r| !r.same(&e.release)) {
                let keeping = e.present.as_ref().is_some_and(|p| p.release.same(r));
                let needs = if keeping { self.keeping_needs(id, d) } else { self.unread_needs_of(r, &e.enabled, d) };
                if !needs.is_empty() {
                    out.insert(UnreadNeed::note(&r.label(), keeping, &needs));
                }
            }
        }
        out.into_iter().collect()
    }

    /// The releases of `t` in the order the search would try them, narrowed only by what the
    /// command line asks of it and by the thorns the plan gives it.
    fn ranking(&self, t: &PkgId, e: &Entry) -> Vec<Rel> {
        let named = self.named(t);
        let reqs: Vec<Req> = named.map(|s| s.req.clone()).into_iter().collect();
        let present = self.present_rel.get(t).cloned();
        let mut all: Vec<Rel> = self.cat.releases(t).to_vec();
        if let Some(p) = &present
            && !all.iter().any(|r| r.same(p))
        {
            all.push(p.clone());
        }
        let ok = eligible(&all, &reqs, &reqs, present.as_deref());
        let ok: Vec<&Rel> = ok.into_iter().filter(|r| !r.library && e.enabled.iter().all(|th| r.thorn(th).is_some())).collect();
        let ok = preferred(ok);
        let newest_first = match present.as_ref().map(|_| &self.present[t].origin) {
            None => true,
            Some(Origin::Added) => named.is_some() || self.request.options.upgrade_all,
            Some(Origin::Base { .. }) => {
                (named.is_some() && self.request.options.upgrade) || self.request.options.upgrade_all
            }
        };
        let flesh = ok.iter().any(|r| r.flesh);
        let mut out: Vec<Rel> = Vec::new();
        if let Some(p) = present.as_ref().filter(|_| !newest_first || flesh)
            && let Some(r) = ok.iter().find(|r| r.same(p))
        {
            out.push((*r).clone());
        }
        let rest: Vec<Rel> = ok.into_iter().filter(|r| !out.iter().any(|o| o.same(r))).cloned().collect();
        out.extend(rest);
        out
    }

    /// What release `r` needs, for the thorns `thorns`, of present stand-ins unread by Cactarium:
    /// thorns of one it has not enabled (or every thorn, when it has more), or a package
    /// with a thorn named as one it has enabled (whether it forks that is unknown).
    fn unread_needs_of(&self, r: &Rel, thorns: &BTreeSet<String>, d: &Derived) -> Vec<UnreadNeed> {
        let mut out: Vec<UnreadNeed> = Vec::new();
        let unread = |x: &PkgId| {
            let p = self.present.get(x).filter(|p| p.stand_in && !x.is_repo())?;
            let why = self.unreadable.get(x)?;
            d.chosen.get(x).filter(|e| !e.is_new()).map(|e| (p, e, why))
        };
        for c in self.parsed(r).iter() {
            if !(c.needed_by.is_empty() || intersects(&c.needed_by, thorns)) {
                continue;
            }
            // An any-of may take another alternative.
            let Body::Single(t, _, th) = &c.body else { continue };
            if let Some((p, e, why)) = unread(t) {
                let new: BTreeSet<String> = match th {
                    Thorns::All => e.release.thorn_names().into_iter().filter(|n| !contains(&p.enabled, n)).collect(),
                    Thorns::Some(v) => v.iter().filter(|n| !contains(&p.enabled, n) && e.release.thorn(n).is_some()).cloned().collect(),
                };
                if !new.is_empty() {
                    let what = match th {
                        Thorns::All => UnreadFor::Every,
                        Thorns::Some(_) => UnreadFor::Thorns(new.into_iter().collect()),
                    };
                    out.push(UnreadNeed { id: t.clone(), package: e.release.label(), by: r.label(), what, why: why.clone() });
                }
                continue;
            }
            // A package it brings in with a thorn named as one an unread stand-in has enabled.
            if self.present.contains_key(t) || d.chosen.contains_key(t) {
                continue;
            }
            for (x, p) in self.present.iter() {
                let Some((_, e, why)) = unread(x) else { continue };
                let shared = (self.cat.releases(t).iter())
                    .flat_map(|q| q.thorns.iter())
                    .find(|q| contains(&p.enabled, &q.thorn))
                    .map(|q| q.thorn.clone());
                if let Some(thorn) = shared {
                    let what = UnreadFor::ForkCheck { other: t.clone(), thorn };
                    out.push(UnreadNeed { id: x.clone(), package: e.release.label(), by: r.label(), what, why: why.clone() });
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// What needs the plan gives a present stand-in it moved, kept where it is instead: thorns it
    /// has not enabled, whose claims Cactarium did not read.
    fn keeping_needs(&self, id: &PkgId, d: &Derived) -> Vec<UnreadNeed> {
        let (Some(p), Some(why)) = (self.present.get(id).filter(|p| p.stand_in), self.unreadable.get(id)) else {
            return Vec::new();
        };
        let mut out: Vec<UnreadNeed> = Vec::new();
        for rec in d.needs.iter().filter(|r| r.kind == Kind::Active && r.taken_by() == Some(id)) {
            let new: Vec<String> = match &rec.need.thorns {
                Thorns::All => p.release.thorn_names().into_iter().filter(|n| !contains(&p.enabled, n)).collect(),
                Thorns::Some(v) => v.iter().filter(|n| !contains(&p.enabled, n) && p.release.thorn(n).is_some()).cloned().collect(),
            };
            if new.is_empty() {
                continue;
            }
            let what = match &rec.need.thorns {
                Thorns::All => UnreadFor::Every,
                Thorns::Some(_) => UnreadFor::Thorns(new),
            };
            let by = rec.need.live(rec.kind).source;
            out.push(UnreadNeed { id: id.clone(), package: p.release.label(), by, what, why: why.clone() });
        }
        out.sort();
        out.dedup();
        out
    }

    fn record(&mut self, depth: usize, f: Failure) {
        self.hits.extend(f.unread.iter().cloned());
        let specific = !f.weak;
        if self.best.as_ref().is_none_or(|(s, d, _)| (specific, depth) > (*s, *d)) {
            self.best = Some((specific, depth, f));
        }
    }

    fn refusal(&mut self) -> Res<Refusal> {
        let Some((specific, _, f)) = self.best.take() else {
            return Ok(Refusal::NoConsistentSet { constraints: Vec::new() });
        };
        if self.exhausted && !specific {
            return Ok(Refusal::NoConsistentSet { constraints: f.lives });
        }
        let best = match f.refusal {
            Pending::Refusal(r) => r,
            Pending::Unclaimed(unmet) => {
                let mut out: Vec<Unmet> = Vec::new();
                let mut asked: BTreeMap<Requirement, Vec<super::Provider>> = BTreeMap::new();
                for (thorn, package, requirement) in unmet {
                    if gix::interrupt::is_triggered() {
                        bail!("Interrupted");
                    }
                    if !asked.contains_key(&requirement) {
                        let found = self.cat.facts.providers(&requirement)?;
                        asked.insert(requirement.clone(), found);
                    }
                    let providers = asked[&requirement].clone();
                    // `--ignore-claim` patterns that dropped dependencies of the package: one of
                    // them may have declared what it needs.
                    let ignored = (self.request.rules.ignore.iter())
                        .filter(|p| {
                            p.package == package
                                && p.kind.as_ref().is_none_or(|(k, _)| *k == super::ClaimKind::Dependency)
                        })
                        .map(ToString::to_string)
                        .collect();
                    out.push(Unmet { thorn, package, requirement, providers, ignored });
                }
                Refusal::Unclaimed(out)
            }
        };
        if self.timed_out {
            return Ok(Refusal::GaveUp { seconds: WALL_TIME.as_secs(), best: Box::new(best) });
        }
        Ok(best)
    }

    /// The decisions a failure depends on: those of the packages it is about and of the needs it
    /// reads, and, through every need a package takes, of the packages (and any-ofs) that brought
    /// it in and of the forks looked at for those needs, all the way up.
    /// Every decision, when some package in that closure is [`Self::removable`] (another decision
    /// could take it out of play, so the failure may vanish without any of the decisions named).
    /// A `fixed` party (one that keeps its part in the failure whatever happens) is neither
    /// followed nor tested, unless something else in the closure leads to it, and its own
    /// decision counts only where its release could make it leave play (a flesh).
    fn involved(
        &self,
        d: &Derived,
        a: &Assign,
        parties: &[PkgId],
        needs: &[usize],
        fixed: &[PkgId],
    ) -> BTreeSet<DKey> {
        let mut out = BTreeSet::new();
        let mut queue: VecDeque<PkgId> = parties.iter().filter(|p| !fixed.contains(p)).cloned().collect();
        let mut seen: BTreeSet<PkgId> = BTreeSet::new();
        let add_need = |rec: &Rec, out: &mut BTreeSet<DKey>, queue: &mut VecDeque<PkgId>| {
            if let Some(f) = &rec.need.any_of {
                out.insert(DKey::AnyOf(f.key.0.clone(), f.key.1));
            }
            queue.extend(rec.need.source_id().cloned());
            queue.extend(rec.via.iter().cloned());
        };
        for i in needs {
            add_need(&d.needs[*i], &mut out, &mut queue);
        }
        // A fixed party's own release bears on the failure only where it decides whether the
        // party stays: a release that is a flesh, which a flesh coming in replaces.
        for p in fixed.iter().filter(|p| a.releases.contains_key(*p)) {
            if self.flesh_possible && self.may_be_flesh.contains(p) {
                out.insert(DKey::Pkg(p.clone()));
            }
        }
        while let Some(p) = queue.pop_front() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if self.removable(&p, d) {
                return a.keys();
            }
            if a.releases.contains_key(&p) {
                out.insert(DKey::Pkg(p.clone()));
            }
            for rec in d.needs.iter().filter(|r| r.taken_by() == Some(&p) || r.need.target == p) {
                add_need(rec, &mut out, &mut queue);
            }
        }
        out
    }

    fn conflict_set(&self, d: &Derived, a: &Assign, f: &Failure) -> BTreeSet<DKey> {
        if f.all { a.keys() } else { self.involved(d, a, &f.parties, &f.needs, &f.fixed) }
    }

    /// One node of the search.
    fn search(&mut self, a: &mut Assign) -> Res<Outcome> {
        if self.exhausted {
            return Ok(Outcome::Failed(a.keys()));
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        self.steps += 1;
        if self.steps % TICK == 0 {
            self.cat.facts.tick(self.steps);
        }
        if self.steps > self.budget {
            self.exhausted = true;
            return Ok(Outcome::Failed(a.keys()));
        }
        if self.started.elapsed() > WALL_TIME {
            (self.exhausted, self.timed_out) = (true, true);
            return Ok(Outcome::Failed(a.keys()));
        }
        let depth = a.depth();
        let d = self.derive_loaded(a)?;
        if let Some(f) = self.check(&d, false) {
            let cs = self.conflict_set(&d, a, &f);
            self.record(depth, f);
            return Ok(Outcome::Failed(cs));
        }
        if let Some(t) = d.open.first().cloned() {
            let key = DKey::Pkg(t.clone());
            let candidates = match self.candidates(&t, &d)? {
                Ok(c) => c,
                Err(f) => {
                    let cs = self.conflict_set(&d, a, &f);
                    self.record(depth, f);
                    return Ok(Outcome::Failed(cs));
                }
            };
            // Running out of candidates is down to why the package is required and what narrowed
            // its candidates.
            let mut cs = self.involved(&d, a, std::slice::from_ref(&t), &[], &[]);
            cs.remove(&key);
            drop(d);
            for c in candidates {
                a.releases.insert(t.clone(), c);
                let out = self.search(a)?;
                a.releases.remove(&t);
                match out {
                    Outcome::Found(plan) => return Ok(Outcome::Found(plan)),
                    // Jump back past this package when the failure below does not involve it.
                    Outcome::Failed(child) if self.exhausted || !child.contains(&key) => {
                        return Ok(Outcome::Failed(child));
                    }
                    Outcome::Failed(child) => cs.extend(child.into_iter().filter(|k| k != &key)),
                }
            }
            return Ok(Outcome::Failed(cs));
        }
        if let Some(open) = d.open_any_of.first().cloned() {
            let key = DKey::AnyOf(open.key.0.clone(), open.key.1);
            let order = match self.alternatives(&open, &d)? {
                Ok(o) => o,
                Err(f) => {
                    let cs = self.conflict_set(&d, a, &f);
                    self.record(depth, f);
                    return Ok(Outcome::Failed(cs));
                }
            };
            let mut cs = self.involved(&d, a, std::slice::from_ref(&open.key.0), &[], &[]);
            drop(d);
            for (i, rule) in order {
                a.any_of.insert(open.key.clone(), (i, rule));
                let out = self.search(a)?;
                a.any_of.remove(&open.key);
                match out {
                    Outcome::Found(plan) => return Ok(Outcome::Found(plan)),
                    Outcome::Failed(child) if self.exhausted || !child.contains(&key) => {
                        return Ok(Outcome::Failed(child));
                    }
                    Outcome::Failed(child) => cs.extend(child.into_iter().filter(|k| k != &key)),
                }
            }
            return Ok(Outcome::Failed(cs));
        }
        match self.complete(&d)? {
            Ok(plan) => Ok(Outcome::Found(plan)),
            Err(f) => {
                let cs = self.conflict_set(&d, a, &f);
                self.record(depth, f);
                Ok(Outcome::Failed(cs))
            }
        }
    }

    /// [`Self::derive`], loading what it finds it needs, in waves, until it needs nothing more.
    fn derive_loaded(&mut self, a: &Assign) -> Res<Derived> {
        loop {
            let d = self.derive(a)?;
            if d.want.is_empty() {
                return Ok(d);
            }
            self.cat.ensure(d.want.iter().cloned())?;
        }
    }

    /// The state the assignment implies: a fixpoint over which packages are required and which
    /// thorns are enabled, since each feeds the other. Both only grow from one round to the next
    /// but for a need moving to a fork that came in, so a few rounds settle it; the cap is a
    /// guard, and reaching it is an error rather than a guess.
    fn derive(&self, a: &Assign) -> Res<Derived> {
        let mut required = BTreeSet::new();
        let mut given = BTreeMap::new();
        for _ in 0..256 {
            let mut d = self.derive_once(a, &required, &given);
            if d.required == required && d.given == given {
                // Present packages something touched first, then what the plan brings in; named
                // ones first within each, each by id.
                let present = |id: &PkgId| self.present.contains_key(id);
                d.open.sort_by_key(|id| (!present(id), self.named(id).is_none(), id.clone()));
                return Ok(d);
            }
            required = d.required;
            given = d.given;
        }
        bail!("The dependency state did not settle (a cycle of stand-ins?); report this as a cactup bug")
    }

    fn derive_once(
        &self,
        a: &Assign,
        required: &BTreeSet<PkgId>,
        given: &BTreeMap<PkgId, BTreeSet<String>>,
    ) -> Derived {
        let options = self.request.options;
        let mut d = Derived::default();
        // The packages in play, and the thorns they have.
        for (id, p) in &self.present {
            let pr = &self.present_rel[id];
            let (release, decided) = match a.releases.get(id) {
                Some(r) => (r.clone(), true),
                None => (pr.clone(), false),
            };
            let whole = self.named(id).is_some_and(|s| s.thorns.is_none());
            let enabled = if pr.same(&release) {
                p.enabled.clone()
            } else if whole {
                release.thorn_names()
            } else {
                p.enabled.iter().filter_map(|n| release.thorn(n).map(|t| t.thorn.clone())).collect()
            };
            let present = Some(p.clone());
            d.chosen.insert(id.clone(), Entry { release, enabled, present, order: usize::MAX, decided });
        }
        for id in required {
            if let (None, Some(r)) = (self.present.get(id), a.releases.get(id)) {
                let (release, enabled) = (r.clone(), BTreeSet::new());
                let entry = Entry { release, enabled, present: None, order: usize::MAX, decided: true };
                d.chosen.insert(id.clone(), entry);
            }
        }
        for (id, names) in given {
            let Some(e) = d.chosen.get_mut(id) else { continue };
            let all = names.contains("");
            let add: Vec<String> = (e.release.thorns.iter())
                .filter(|t| all || contains(names, &t.thorn))
                .map(|t| t.thorn.clone())
                .collect();
            e.enabled.extend(add);
        }
        self.replace_fleshes(&mut d);
        if options.replace {
            self.absorb(&mut d);
        }
        for (id, e) in &d.chosen {
            for (up, _) in self.fork_claims_of(&e.release).iter() {
                let forks = d.forks.entry(up.clone()).or_default();
                if !forks.contains(id) {
                    forks.push(id.clone());
                }
            }
        }
        // The needs, in a fixed order: named, --upgrade-all, then each package's claims.
        let mut needs: Vec<(Rc<Need>, Kind, bool, Option<ChoiceRule>)> = Vec::new();
        for n in &self.named_needs {
            needs.push((n.clone(), Kind::Active, true, None));
        }
        for n in self.upgrade_needs.iter().filter(|n| d.chosen.contains_key(&n.target)) {
            needs.push((n.clone(), Kind::Active, true, None));
        }
        let (mut open_any_of, mut failure) = (Vec::new(), d.failure.take());
        for (id, e) in &d.chosen {
            let parsed = self.parsed(&e.release);
            let mut active = |c: &Parsed, stable: bool, needs: &mut Vec<_>| match &c.body {
                Body::Single(..) => {
                    needs.extend(self.need_of(&e.release, c, None).map(|n| (n, Kind::Active, stable, None)))
                }
                Body::AnyOf(_) => {
                    let key = (id.clone(), c.index);
                    match a.any_of.get(&key) {
                        Some((alt, rule)) => {
                            let n = self.need_of(&e.release, c, Some(*alt));
                            needs.extend(n.map(|n| (n, Kind::Active, stable, Some(*rule))));
                        }
                        None => open_any_of.push(OpenAnyOf { key, release: e.release.clone() }),
                    }
                }
                Body::Bad(detail) => {
                    if failure.is_none() {
                        let (by, claim, detail) = (e.release.label(), c.label.clone(), detail.clone());
                        let ignore = format!("{id}:dependency:{}", c.target);
                        let r = Refusal::BadClaim { by, claim, detail, ignore };
                        failure = Some(Failure::of(r, Vec::new(), vec![id.clone()]));
                    }
                }
            };
            match &e.present {
                Some(p) if !e.is_new() => {
                    // A present package not changing: passive claims, and active ones for the
                    // thorns new to it. Stable once decided, or when it can never move.
                    let stable = e.decided || !self.may_move(id);
                    let kind = if stable { Kind::Passive } else { Kind::PassiveFinal };
                    let new: BTreeSet<String> =
                        e.enabled.iter().filter(|t| !contains(&p.enabled, t)).cloned().collect();
                    let for_new_thorns = |c: &&Parsed| !c.needed_by.is_empty() && intersects(&c.needed_by, &new);
                    let for_new: BTreeSet<usize> = (parsed.iter())
                        .filter(|c| !options.no_deps && for_new_thorns(c))
                        .map(|c| c.index)
                        .collect();
                    for (i, n) in self.present_passive.get(id).into_iter().flatten() {
                        if !for_new.contains(i) {
                            needs.push((n.clone(), kind, stable, None));
                        }
                    }
                    for c in parsed.iter().filter(|c| for_new.contains(&c.index)) {
                        active(c, stable, &mut needs);
                    }
                }
                _ if !options.no_deps => {
                    for c in parsed.iter() {
                        if c.needed_by.is_empty() || intersects(&c.needed_by, &e.enabled) {
                            active(c, e.decided, &mut needs);
                        }
                    }
                }
                _ => {}
            }
        }
        d.open_any_of = open_any_of;
        d.failure = failure;
        // Who takes each need.
        let mut new_required: BTreeSet<PkgId> = BTreeSet::new();
        let mut new_given: BTreeMap<PkgId, BTreeSet<String>> = BTreeMap::new();
        for (need, kind, stable, rule) in needs {
            let (taker, via) = self.taker(&need, kind, &d, required);
            let give = |given: &mut BTreeMap<PkgId, BTreeSet<String>>, who: &PkgId| {
                let names = match &need.thorns {
                    Thorns::All => BTreeSet::from([String::new()]),
                    Thorns::Some(v) => v.iter().cloned().collect(),
                };
                given.entry(who.clone()).or_default().extend(names);
            };
            if kind == Kind::Active {
                match &taker {
                    Taker::Itself if self.present.contains_key(&need.target) => {
                        match d.chosen.get(&need.target) {
                            Some(e) if !e.decided => {
                                d.touched.insert(need.target.clone());
                            }
                            Some(_) => {}
                            // Needed, and replaced by the flesh the plan brings in.
                            None => {
                                let origin = &self.present[&need.target].origin;
                                let by = d.replaced.iter().find(|(r, _, _, _)| match r {
                                    Replaced::Added { id } => id == &need.target,
                                    Replaced::Base { repo } => {
                                        matches!(origin, Origin::Base { repo: o } if o == repo)
                                    }
                                });
                                if let (Some((_, by, _, _)), None) = (by, &d.failure) {
                                    let first = self.present_rel[&need.target].label();
                                    let second =
                                        d.chosen.get(by).map(|e| e.release.label()).unwrap_or_default();
                                    let ignore = need.live(kind).ignore;
                                    let r = Refusal::TwoFleshes { first, second, ignore };
                                    d.failure = Some(Failure::of(r, Vec::new(), vec![by.clone()]).everything());
                                }
                            }
                        }
                        give(&mut new_given, &need.target);
                    }
                    Taker::Itself => {
                        new_required.insert(need.target.clone());
                        give(&mut new_given, &need.target);
                        if !self.cat.loaded(&need.target) {
                            d.want.insert(need.target.clone());
                        }
                    }
                    Taker::Other { taker, .. } => {
                        if d.chosen.get(taker).is_some_and(|e| e.present.is_some() && !e.decided) {
                            d.touched.insert(taker.clone());
                        }
                        give(&mut new_given, taker);
                    }
                    Taker::Fork(f) => {
                        new_required.insert(f.clone());
                        give(&mut new_given, f);
                    }
                    Taker::NotStandIn { .. } => {}
                }
            }
            let stands_in = matches!(taker, Taker::Other { .. } | Taker::NotStandIn { .. });
            if stands_in && !self.cat.loaded(&need.target) {
                d.want.insert(need.target.clone());
            }
            d.needs.push(Rec { need, kind, stable, rule, taker, via });
        }
        d.required = new_required;
        d.given = new_given;
        // The claim that brings the second flesh in, when one does.
        if let Some(Failure { refusal: Pending::Refusal(Refusal::TwoFleshes { second, ignore, .. }), .. }) =
            &mut d.failure
            && ignore.is_none()
        {
            *ignore = (d.needs.iter())
                .find(|r| {
                    r.kind == Kind::Active
                        && r.need.label().is_some()
                        && d.chosen.get(&r.need.target).is_some_and(|e| &e.release.label() == second)
                })
                .and_then(|r| r.need.live(r.kind).ignore);
        }
        // What is still to be decided.
        for id in &d.required {
            if !a.releases.contains_key(id) && !self.present.contains_key(id) {
                d.open.push(id.clone());
            }
        }
        for (id, e) in &d.chosen {
            let open = !e.decided
                && e.present.is_some()
                && (d.touched.contains(id) || self.named(id).is_some() || options.upgrade_all);
            if open {
                d.open.push(id.clone());
            }
        }
        d
    }

    /// Who takes a need, and the stand-ins looked at: a named or present target itself; else a
    /// package an override says satisfies it; else a fork in play that stands in (any of its
    /// fork-of claims, the forks in id order); else an undecided fork the plan requires; else the
    /// target itself (also when the forks in play do not stand in for a flesh: the flesh is
    /// replaced). Whether a planned target is already decided does not matter, so who stands in
    /// does not depend on the order packages are decided in (their ids).
    fn taker(&self, need: &Need, kind: Kind, d: &Derived, required: &BTreeSet<PkgId>) -> (Taker, Vec<PkgId>) {
        let t = &need.target;
        let present_in_play = self.present.contains_key(t) && d.chosen.contains_key(t);
        if need.source == Source::Named || present_in_play || self.named(t).is_some() {
            return (Taker::Itself, Vec::new());
        }
        let mut via = Vec::new();
        for x in self.request.rules.satisfiers(t) {
            if d.chosen.contains_key(x) {
                let note = format!("{x} stands in for {t} (a claims override)");
                return (Taker::Other { taker: x.clone(), note }, vec![x.clone()]);
            }
        }
        let mut not_standing: Option<PkgId> = None;
        if let Some(forks) = d.forks.get(t) {
            let published = self.cat.published(t);
            for id in forks {
                let e = &d.chosen[id];
                via.push(id.clone());
                let stands = |(up, fork): &(PkgId, ForkOf)| {
                    up == t && t.slug(&need.req).is_some_and(|dep| fork.stands_in_for(&dep, &published))
                };
                if self.fork_claims_of(&e.release).iter().any(stands) {
                    let note = format!("{} stands in for {t}", e.release.label());
                    return (Taker::Other { taker: id.clone(), note }, via);
                }
                not_standing.get_or_insert_with(|| id.clone());
            }
        }
        if kind == Kind::Active {
            for f in required.iter().filter(|f| !d.chosen.contains_key(*f)) {
                if self.cat.releases(f).iter().any(|r| self.fork_claims_of(r).iter().any(|(up, _)| up == t)) {
                    via.push(f.clone());
                    return (Taker::Fork(f.clone()), via);
                }
            }
        }
        let taker = match not_standing {
            Some(_) if self.cat.is_flesh(t) => Taker::Itself,
            Some(fork) => Taker::NotStandIn { fork },
            None => Taker::Itself,
        };
        (taker, via)
    }

    /// A present flesh replaced by the plan (by a flesh brought in) that a package staying still
    /// needs: no fork standing in, no override saying another satisfies it. Its claim binds as
    /// passive claims do (never under `--no-deps`); a dependent moved by the plan asks anew.
    fn displaced_dependents(&self, d: &Derived, last: bool) -> Option<Failure> {
        let replacer = |t: &PkgId| {
            let origin = &self.present.get(t)?.origin;
            let (_, by, _, _) = d.replaced.iter().find(|(r, _, _, flesh)| {
                *flesh
                    && match r {
                        Replaced::Added { id } => id == t,
                        Replaced::Base { repo } => matches!(origin, Origin::Base { repo: o } if o == repo),
                    }
            })?;
            Some(by.clone())
        };
        let mut found: Option<(PkgId, PkgId)> = None;
        let mut dependents: Vec<(String, String)> = Vec::new();
        let (mut lives, mut needs, mut parties) = (Vec::new(), Vec::new(), Vec::new());
        for (i, rec) in d.needs.iter().enumerate() {
            let t = &rec.need.target;
            let staying = rec.need.source_id().is_some_and(|s| d.chosen.contains_key(s));
            if rec.kind == Kind::Active || rec.taker != Taker::Itself || d.chosen.contains_key(t) || !staying {
                continue;
            }
            if !self.binds(rec, last) {
                continue;
            }
            let Some(by) = replacer(t) else { continue };
            if found.as_ref().is_some_and(|(flesh, _)| flesh != t) {
                continue;
            }
            found.get_or_insert_with(|| (t.clone(), by));
            let live = rec.need.live(rec.kind);
            dependents.push((live.source.clone(), format!("{} {}", live.target, live.req)));
            parties.extend(rec.need.source_id().cloned());
            lives.push(live);
            needs.push(i);
        }
        let (flesh, by) = found?;
        let by_label = d.chosen.get(&by).map(|e| e.release.label()).unwrap_or_else(|| by.to_string());
        let flesh_label = self.present_rel[&flesh].label();
        parties.push(by);
        let r = Refusal::FleshHasDependents { flesh: flesh_label, by: by_label, dependents, no_deps: true };
        Some(Failure { needs, ..Failure::of(r, lives, parties).everything() })
    }

    /// A flesh the plan brings in replaces the flesh the installation has (a base repo, or an
    /// added flesh package); two fleshes brought in are refused.
    fn replace_fleshes(&self, d: &mut Derived) {
        let planned = ids_where(&d.chosen, |_, e| e.present.is_none() && e.release.flesh);
        match planned.as_slice() {
            [] => {}
            [by] => {
                for id in ids_where(&d.chosen, |id, e| id != by && e.release.flesh) {
                    let e = d.chosen.remove(&id).expect("listed above");
                    let replaced = match e.present.as_ref().map(|p| &p.origin) {
                        Some(Origin::Base { repo }) => Replaced::Base { repo: repo.clone() },
                        _ => Replaced::Added { id },
                    };
                    d.replaced.push((replaced, by.clone(), Vec::new(), true));
                }
            }
            [first, second, ..] => {
                let parties = vec![first.clone(), second.clone()];
                let (first, second) = (d.chosen[first].release.label(), d.chosen[second].release.label());
                let r = Refusal::TwoFleshes { first, second, ignore: None };
                d.failure.get_or_insert(Failure::of(r, Vec::new(), parties));
            }
        }
    }

    /// Under `--replace`, a package the plan brings in or moves takes the place of every unchanged
    /// base package it forks, with every thorn the installation has enabled from it.
    fn absorb(&self, d: &mut Derived) {
        for n in ids_where(&d.chosen, |_, e| e.is_new()) {
            let Some(release) = d.chosen.get(&n).map(|e| e.release.clone()) else { continue };
            let forks = self.fork_claims_of(&release);
            let forked_base =
                |id: &PkgId, e: &Entry| *id != n && e.base_repo().is_some() && forks.iter().any(|(u, _)| u == id);
            for u in ids_where(&d.chosen, forked_base) {
                let ue = d.chosen.remove(&u).expect("listed above");
                let repo = ue.base_repo().unwrap_or_default().to_owned();
                let missing: Vec<String> =
                    ue.enabled.iter().filter(|t| release.thorn(t).is_none()).cloned().collect();
                if !missing.is_empty() {
                    let (by, by_label) = (n.clone(), release.label());
                    let (upstream, base_repo, thorns) = (u, repo, missing);
                    let r = Refusal::ReplacementLacksThorns { by, by_label, upstream, base_repo, thorns };
                    d.failure.get_or_insert(Failure::of(r, Vec::new(), vec![n.clone()]));
                    return;
                }
                let names: Vec<String> =
                    ue.enabled.iter().filter_map(|t| release.thorn(t).map(|x| x.thorn.clone())).collect();
                d.chosen.get_mut(&n).expect("a newcomer").enabled.extend(names);
                d.replaced.push((Replaced::Base { repo }, n.clone(), Vec::new(), false));
            }
        }
    }

    /// Whether a present package may move now: named, under `--upgrade-all`, touched (or
    /// decided) under `--upgrade`, or a flesh. Never a pseudo-package.
    fn movable(&self, t: &PkgId, d: &Derived) -> bool {
        let options = self.request.options;
        !t.is_repo()
            && ((self.named(t).is_some() && !self.held_base(t))
                || options.upgrade_all
                || (options.upgrade && (d.touched.contains(t) || d.chosen.get(t).is_some_and(|e| e.decided)))
                || d.chosen.get(t).is_some_and(|e| e.release.flesh))
    }

    /// Whether a need binds in this check: active ones from sources that stay (the others wait for
    /// their source to be decided); passive ones that narrow now (not under `--no-deps`); those
    /// of present packages never decided only once the assignment is complete.
    fn binds(&self, rec: &Rec, last: bool) -> bool {
        let no_deps = self.request.options.no_deps;
        match rec.kind {
            Kind::Active => rec.stable || last,
            Kind::Passive => !no_deps,
            Kind::PassiveFinal => last && !no_deps,
        }
    }

    /// The first failure in the state: one found while deriving it, a fork that does not stand in,
    /// a stand-in lacking thorns, a decided package outside its constraints, or (before the
    /// assignment is complete) a conflict among settled packages that can only stay.
    fn check(&self, d: &Derived, last: bool) -> Option<Failure> {
        if let Some(f) = &d.failure {
            return Some(f.clone());
        }
        if let Some(f) = self.unread_thorns(d) {
            return Some(f);
        }
        if let Some(f) = self.displaced_dependents(d, last) {
            return Some(f);
        }
        for (i, rec) in d.needs.iter().enumerate() {
            if !self.binds(rec, last) {
                continue;
            }
            let source: Vec<PkgId> = rec.need.source_id().cloned().into_iter().collect();
            match &rec.taker {
                Taker::NotStandIn { fork } => {
                    let Some(e) = d.chosen.get(fork) else { continue };
                    let (fork, fork_label, live) = (fork.clone(), e.release.label(), rec.need.live(rec.kind));
                    let parties = [vec![fork.clone()], source].concat();
                    let r = Refusal::ForkDoesNotStandIn { fork, fork_label, live: live.clone() };
                    return Some(Failure { needs: vec![i], ..Failure::of(r, vec![live], parties) });
                }
                Taker::Other { taker, .. } => {
                    let Some(e) = d.chosen.get(taker) else { continue };
                    let missing = missing_thorns(&e.release, &rec.need);
                    if !missing.is_empty() {
                        let (label, live) = (e.release.label(), rec.need.live(rec.kind));
                        let parties = [vec![taker.clone()], source].concat();
                        let r = Refusal::MissingThorns { taker: label, thorns: missing, live: live.clone() };
                        return Some(Failure { needs: vec![i], ..Failure::of(r, vec![live], parties) });
                    }
                }
                Taker::Itself | Taker::Fork(_) => {}
            }
        }
        for (t, e) in &d.chosen {
            let fixed = e.present.is_some() && !self.movable(t, d);
            // A present package that cannot move here is at its release whether decided or not:
            // a constraint it fails is that conflict now, before anything that only follows from
            // it (a requirement only its other releases would meet) is reported in its place.
            if !e.decided && (!fixed || self.may_move(t)) {
                continue;
            }
            // One a decision still to come could take out of play (a fork the plan brings in
            // replacing its base repo under --replace, a flesh coming in) is checked once every
            // decision is made: then it is in play or not, and a fork may stand in for it.
            if fixed && !last && !self.stays(t, d) {
                continue;
            }
            let on_t: Vec<(usize, &Rec)> = (d.needs.iter().enumerate())
                .filter(|(_, r)| &r.need.target == t && r.taker == Taker::Itself && self.binds(r, last))
                .filter(|(_, r)| !(fixed && r.kind != Kind::Active))
                .collect();
            let Some((_, first)) = on_t.iter().find(|(_, r)| violates(&e.release, &r.need)) else { continue };
            let lives: Vec<Live> = on_t.iter().map(|(_, r)| r.need.live(r.kind)).collect();
            let indices: Vec<usize> = on_t.iter().map(|(i, _)| *i).collect();
            let parties = vec![t.clone()];
            if fixed {
                let (installed, live) = (e.release.label(), first.need.live(first.kind));
                let missing = missing_thorns(&e.release, &first.need);
                let r = if first.need.req.fit(&e.release) != Fit::No && !missing.is_empty() {
                    Refusal::PresentLacksThorns { installed, thorns: missing, live, upgrade: true }
                } else {
                    Refusal::PresentOutsideRange { installed, live, upgrade: true }
                };
                return Some(Failure { needs: indices, ..Failure::of(r, lives, parties) });
            }
            // Would another release have met them all? Then this is only a wrong turn.
            let mut releases: Vec<Rel> = self.cat.releases(t).to_vec();
            releases.extend(self.present_rel.get(t).cloned());
            releases.push(e.release.clone());
            let reqs: Vec<Req> = on_t.iter().map(|(_, r)| r.need.req.clone()).collect();
            let present = self.present_rel.get(t).map(|r| &**r);
            let fits_all = eligible(&releases, &reqs, &reqs, present)
                .into_iter()
                .any(|r| on_t.iter().all(|(_, n)| !violates(r, &n.need)));
            if fits_all {
                let r = Refusal::NoConsistentSet { constraints: lives.clone() };
                return Some(Failure { weak: true, needs: indices, ..Failure::of(r, lives, parties) });
            }
            let r = self.no_release(t, lives.clone());
            return Some(Failure { needs: indices, ..Failure::of(r, lives, parties) });
        }
        if last {
            return None;
        }
        self.prune(d)
    }

    /// Conflicts among settled packages (decided, or present and never moving) that nothing a
    /// deeper decision does can take away: fork and upstream, incompatible-with, thorn names (not
    /// under `--replace`, which gives base checkouts up), libraries against installed placements,
    /// and requirements nothing reachable provides. A package brought in counts only when no known
    /// release claims to fork it (a fork coming in later could take its place). An upstream counts
    /// when it stays in play, settled or not (a fork claim reads only the fork's release). For thorn names,
    /// a present package's enabled thorn counts too when it keeps it whatever happens, moving or
    /// not (under `--upgrade` every present package may move, and the clash would otherwise wait
    /// for a complete assignment).
    fn prune(&self, d: &Derived) -> Option<Failure> {
        let settled = |id: &PkgId| {
            d.chosen.get(id).is_some_and(|e| {
                (e.decided || (e.present.is_some() && !self.may_move(id)))
                    && match &e.present {
                        None => self.named(id).is_some() || !self.displaceable(id),
                        // A present flesh a planned one could replace, a base repo --replace could
                        // take over: neither stays for sure.
                        Some(_) => self.stays(id, d),
                    }
            })
        };
        let holds = |id: &PkgId, thorn: &str| settled(id) || self.keeps_thorn(id, thorn, d);
        let same_thorn: Option<conflict::CountsThorn> =
            if self.request.options.replace { None } else { Some(&holds) };
        let upstream = |id: &PkgId| self.stays(id, d);
        if let Some((mut r, parties)) = conflict::conflicts(&d.chosen, &settled, &upstream, same_thorn) {
            self.fill_ignore(&mut r, d);
            return Some(self.conflict_failure(r, parties, d));
        }
        if let Some(f) = self.prune_libraries(d, &settled) {
            return Some(f);
        }
        self.prune_unclaimed(d, &settled)
    }

    /// The claim that brings the newcomer in, for the generic same-thorn refusal.
    fn fill_ignore(&self, r: &mut Refusal, d: &Derived) {
        if let Refusal::SameThorn { newcomer, base_repo, ignore, needed, .. } = r {
            let claim = (d.needs.iter())
                .find(|r| r.kind == Kind::Active && r.taken_by() == Some(newcomer) && r.need.label().is_some());
            *needed = claim.is_some();
            if base_repo.is_none() {
                *ignore = claim.and_then(|r| r.need.live(r.kind).ignore);
            }
        }
    }

    /// A settled package's placement of a library that unchanged, settled packages hold at a
    /// commit it does not accept (or at two commits).
    fn prune_libraries(&self, d: &Derived, settled: &dyn Fn(&PkgId) -> bool) -> Option<Failure> {
        let mut held: BTreeMap<&PkgId, Vec<(&str, &PkgId, &Entry)>> = BTreeMap::new();
        for (id, e) in d.chosen.iter().filter(|(id, e)| !e.is_new() && settled(id)) {
            for l in e.present.as_ref().map(|p| p.libraries.as_slice()).unwrap_or_default() {
                held.entry(&l.id).or_default().push((&l.commit, id, e));
            }
        }
        if held.is_empty() {
            return None;
        }
        for (id, e) in d.chosen.iter().filter(|(id, e)| e.is_new() && settled(id)) {
            for c in e.release.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_some()) {
                let target = claim_target(&c.target, c.range.as_deref(), c.sha.as_deref());
                let Ok((lib, req)) = target else { continue };
                let Some(holders) = held.get(&lib) else { continue };
                let commits: BTreeSet<&str> = holders.iter().map(|(c, _, _)| *c).collect();
                let release = held_release(self.cat, &lib, holders[0].0);
                if commits.len() > 1 || req.fit(&release) == Fit::No {
                    let ignore = format!("{id}:dependency:{lib}");
                    let (target, source) = (lib.clone(), e.release.label());
                    let mut lives = vec![Live { target, req: req.to_string(), source, ignore: Some(ignore) }];
                    for (commit, _, h) in holders {
                        let source = format!("installed {}", h.release.label());
                        lives.push(Live { target: lib.clone(), req: format!("#{}", short(commit)), source, ignore: None });
                    }
                    let mut parties = vec![id.clone()];
                    parties.extend(holders.iter().map(|(_, hid, _)| (*hid).clone()));
                    let r = Refusal::LibraryClash { library: lib.clone(), placements: lives.clone() };
                    return Some(Failure::of(r, lives, parties));
                }
            }
        }
        None
    }

    /// A requirement of a settled package's new thorn that nothing in play meets and that no
    /// release of the universe offers: no deeper decision can meet it.
    fn prune_unclaimed(&self, d: &Derived, settled: &dyn Fn(&PkgId) -> bool) -> Option<Failure> {
        let unmet: Vec<(String, PkgId, Requirement)> = (conflict::unmet_requirements(&d.chosen))
            .into_iter()
            .filter(|(_, p, _)| settled(p))
            .collect();
        if unmet.is_empty() {
            return None;
        }
        // Any thorn of any release that could come into play (enabled or not).
        let offered = |r: &Requirement| {
            self.universe.iter().any(|rel| rel.thorns.iter().any(|t| ccl::meets(&t.thorn, &t.facts, r)))
        };
        let hopeless: Vec<(String, PkgId, Requirement)> =
            unmet.into_iter().filter(|(_, _, r)| !offered(r)).collect();
        if hopeless.is_empty() {
            return None;
        }
        Some(Failure::unclaimed(hopeless))
    }

    fn no_release(&self, t: &PkgId, constraints: Vec<Live>) -> Refusal {
        if self.cat.is_flesh(t) {
            Refusal::FleshUnmet { target: t.clone(), constraints }
        } else {
            Refusal::NoReleaseFits { target: t.clone(), constraints }
        }
    }

    /// The releases to try for `t`, in order of preference, or why there are none. Needs from
    /// sources that stay narrow them; every need on it may fetch the read at a commit it names or
    /// admit a yanked release it names exactly.
    fn candidates(&mut self, t: &PkgId, d: &Derived) -> Res<Result<Vec<Rel>, Failure>> {
        let on_t: Vec<&Rec> =
            d.needs.iter().filter(|r| &r.need.target == t && r.taker == Taker::Itself).collect();
        let lives: Vec<Live> = on_t.iter().map(|r| r.need.live(r.kind)).collect();
        let fail = |r: Refusal| Ok(Err(Failure::of(r, lives.clone(), vec![t.clone()])));
        let narrowing: Vec<&Rec> = on_t.iter().copied().filter(|r| r.stable && self.binds(r, false)).collect();
        let narrow: Vec<Req> = narrowing.iter().map(|r| r.need.req.clone()).collect();
        let every: Vec<Req> = on_t.iter().filter(|r| self.binds(r, true)).map(|r| r.need.req.clone()).collect();
        let mut required: Vec<String> = narrowing.iter().flat_map(|r| r.need.names()).collect();
        let named = self.named(t).cloned();
        let present = self.present_rel.get(t).cloned();
        if let Some(p) = &present {
            if !self.movable(t, d) {
                return Ok(Ok(vec![p.clone()]));
            }
            // An unnamed move carries every thorn the package has enabled.
            if named.is_none() {
                required.extend(self.present[t].enabled.iter().cloned());
            }
        }
        if let Some(Req::Tip(tip)) = named.as_ref().map(|s| &s.req) {
            let r = if self.cat.known(t) {
                let detail = "Cactarium cannot read that branch".to_owned();
                Refusal::NotAvailable { target: t.clone(), at: tip.clone(), by: None, detail }
            } else {
                Refusal::NotRegistered { target: t.clone(), by: None }
            };
            return fail(r);
        }
        self.cat.ensure([t.clone()])?;
        if present.is_none() && !self.cat.known(t) {
            let by = on_t.iter().find(|r| r.need.source != Source::Named).map(|r| r.need.live(r.kind));
            return fail(Refusal::NotRegistered { target: t.clone(), by });
        }
        let (mut all, unreadable) = self.cat.candidates(t, &every)?;
        if let Some(p) = &present {
            // The installed release is the one the caller gave (its claims, its index of them).
            match all.iter_mut().find(|r| r.same(p)) {
                Some(r) => *r = p.clone(),
                None => all.push(p.clone()),
            }
        }
        let ok = eligible(&all, &narrow, &every, present.as_deref());
        let has_required = |r: &&Rel| required.iter().all(|th| r.thorn(th).is_some());
        let ok: Vec<&Rel> = ok.into_iter().filter(has_required).collect();
        if !ok.is_empty() && ok.iter().all(|r| r.library) {
            let r = match on_t.iter().find_map(|r| match &r.need.source {
                Source::Release { label, id } => Some((label.clone(), id.clone())),
                _ => None,
            }) {
                Some((by, id)) => Refusal::BadClaim {
                    by,
                    claim: t.to_string(),
                    detail: format!("{t} is a library, but the dependency has no `into`"),
                    ignore: format!("{id}:dependency:{t}"),
                },
                None => Refusal::NamedLibrary { target: t.clone() },
            };
            return fail(r);
        }
        let ok: Vec<&Rel> = preferred(ok.into_iter().filter(|r| !r.library).collect());
        if ok.is_empty() {
            if let Some((at, detail)) = unreadable.into_iter().next() {
                let by = on_t.iter().find(|r| r.need.source != Source::Named).map(|r| r.need.live(r.kind));
                return fail(Refusal::NotAvailable { target: t.clone(), at, by, detail });
            }
            return fail(self.no_release(t, lives.clone()));
        }
        // A present package that may stay tries its present release first, unless it re-resolves
        // to its newest (a named added package, or --upgrade-all).
        let newest_first = match present.as_ref().map(|_| &self.present[t].origin) {
            None => true,
            Some(Origin::Added) => named.is_some() || self.request.options.upgrade_all,
            Some(Origin::Base { .. }) => {
                (named.is_some() && self.request.options.upgrade) || self.request.options.upgrade_all
            }
        };
        let flesh = ok.iter().any(|r| r.flesh);
        let mut out: Vec<Rel> = Vec::new();
        if let Some(p) = present.as_ref().filter(|_| !newest_first || flesh)
            && let Some(r) = ok.iter().find(|r| r.same(p))
        {
            out.push((*r).clone());
        }
        let rest: Vec<Rel> = ok.into_iter().filter(|r| !out.iter().any(|o| o.same(r))).cloned().collect();
        out.extend(rest);
        Ok(Ok(out))
    }

    /// The alternatives of an any-of in the order its rules try them, each with its rule.
    fn alternatives(
        &mut self,
        open: &OpenAnyOf,
        d: &Derived,
    ) -> Res<Result<Vec<(usize, ChoiceRule)>, Failure>> {
        let parsed = self.parsed(&open.release);
        let Some(Body::AnyOf(alts)) = parsed.iter().find(|c| c.index == open.key.1).map(|c| &c.body) else {
            return Ok(Ok(Vec::new()));
        };
        self.cat.ensure(alts.iter().map(|(id, _, _)| id.clone()))?;
        // Present: a present package at a release that fits (or a present fork standing in);
        // planned: one in play or required.
        let class = |(id, req, _): &(PkgId, Req, Thorns)| -> Option<ChoiceRule> {
            let need = Need {
                source: Source::Release { label: open.release.label(), id: open.key.0.clone() },
                target: id.clone(),
                req: req.clone(),
                thorns: Thorns::All,
                claim: None,
                index: usize::MAX,
                any_of: None,
            };
            match self.taker(&need, Kind::Active, d, &d.required).0 {
                Taker::Itself => match d.chosen.get(id) {
                    Some(e) if req.fit(&e.release) == Fit::No => None,
                    Some(e) if e.present.is_some() => Some(ChoiceRule::Present),
                    Some(_) => Some(ChoiceRule::Planned),
                    None if d.required.contains(id) => Some(ChoiceRule::Planned),
                    None => None,
                },
                Taker::Other { taker, .. } => {
                    let present = d.chosen.get(&taker).is_some_and(|e| e.present.is_some());
                    Some(if present { ChoiceRule::Present } else { ChoiceRule::Planned })
                }
                Taker::Fork(_) => Some(ChoiceRule::Planned),
                Taker::NotStandIn { .. } => None,
            }
        };
        let classes: Vec<Option<ChoiceRule>> = alts.iter().map(class).collect();
        let mut order: Vec<(usize, ChoiceRule)> = Vec::new();
        for want in [ChoiceRule::Present, ChoiceRule::Planned] {
            let these = classes.iter().enumerate().filter(|(_, c)| **c == Some(want));
            order.extend(these.map(|(i, _)| (i, want)));
        }
        for i in 0..alts.len() {
            if !order.iter().any(|(j, _)| *j == i) {
                let rule = if i == 0 { ChoiceRule::FirstListed } else { ChoiceRule::FirstWithoutConflict };
                order.push((i, rule));
            }
        }
        Ok(Ok(order))
    }

    /// The checks on a complete assignment, then the plan.
    fn complete(&mut self, d: &Derived) -> Res<Result<Plan, Failure>> {
        if let Some(f) = self.check(d, true) {
            return Ok(Err(f));
        }
        let mut chosen = d.chosen.clone();
        let mut replaced = d.replaced.clone();
        let (order, why, brought_by) = self.order(d);
        for (i, id) in order.iter().enumerate() {
            if let Some(e) = chosen.get_mut(id) {
                e.order = i;
            }
        }
        if self.request.options.replace {
            replace_colliding(&mut chosen, &mut replaced);
        }
        if let Some((mut r, parties)) = conflict::conflicts(&chosen, &|_| true, &|_| true, Some(&|_, _| true)) {
            self.fill_ignore(&mut r, d);
            return Ok(Err(self.conflict_failure(r, parties, d)));
        }
        let libraries = match self.place_libraries(&chosen)? {
            Ok(l) => l,
            Err(f) => return Ok(Err(f)),
        };
        let mut unmet = conflict::unmet_requirements(&chosen);
        // Replacing checkouts (or the flesh) can take away what installed thorns needed.
        if !replaced.is_empty() {
            let before: Vec<(String, ThornFacts)> = (self.present.values())
                .flat_map(|p| p.enabled.iter().filter_map(|t| p.release.thorn(t)))
                .map(|t| (t.thorn.clone(), t.facts.clone()))
                .collect();
            unmet.extend(conflict::broken_requirements(&chosen, &before));
        }
        if !unmet.is_empty() {
            return Ok(Err(Failure::unclaimed(unmet)));
        }
        let mut plan = self.plan(d, chosen, replaced, libraries, &order, &why, &brought_by);
        plan.skipped = self.skipped_notes(d);
        Ok(Ok(plan))
    }

    /// The order of the plan's steps: the named packages, then what their single-target
    /// dependencies bring, breadth first, then what each any-of brings (by dependent and
    /// declaration order); with each package's reason and the dependents that brought it in. The
    /// reason is, of the needs reaching it from the shallowest depth, a named one, else a
    /// single-target claim before an any-of, then the dependent first by id, then the claim's
    /// index: never the order claims were declared in.
    #[allow(clippy::type_complexity)]
    fn order(
        &self,
        d: &Derived,
    ) -> (IndexSet<PkgId>, BTreeMap<PkgId, Why>, BTreeMap<PkgId, IndexSet<String>>) {
        let active: Vec<(&Rec, PkgId)> = (d.needs.iter())
            .filter(|r| r.kind == Kind::Active)
            .filter_map(|r| r.taken_by().map(|t| (r, t.clone())))
            .collect();
        let by_source = |id: &PkgId| -> Vec<(&Rec, PkgId)> {
            active.iter().filter(|(r, _)| r.need.source_id() == Some(id)).cloned().collect()
        };
        let mut order: IndexSet<PkgId> = IndexSet::new();
        let mut depth: BTreeMap<PkgId, usize> = BTreeMap::new();
        let mut queue: VecDeque<PkgId> = VecDeque::new();
        let mut reach = |r: &Rec, t: &PkgId, order: &mut IndexSet<PkgId>, queue: &mut VecDeque<PkgId>| {
            if order.insert(t.clone()) {
                let below = r.need.source_id().and_then(|s| depth.get(s)).map_or(0, |d| d + 1);
                depth.insert(t.clone(), below);
                queue.push_back(t.clone());
            }
        };
        for (r, t) in active.iter().filter(|(r, _)| r.need.source_id().is_none()) {
            reach(r, t, &mut order, &mut queue);
        }
        let mut any_ofs: Vec<(&Rec, PkgId)> =
            active.iter().filter(|(r, _)| r.need.any_of.is_some()).cloned().collect();
        any_ofs.sort_by_key(|(r, _)| r.need.any_of.as_ref().map(|f| f.key.clone()));
        let mut next_any_of = any_ofs.into_iter();
        loop {
            while let Some(p) = queue.pop_front() {
                for (r, t) in by_source(&p).iter().filter(|(r, _)| r.need.any_of.is_none()) {
                    reach(r, t, &mut order, &mut queue);
                }
            }
            match next_any_of.next() {
                Some((r, t)) => reach(r, &t, &mut order, &mut queue),
                None => break,
            }
        }
        let mut why: BTreeMap<PkgId, Why> = BTreeMap::new();
        for t in &order {
            let from_depth = |r: &&(&Rec, PkgId)| match r.0.need.source_id() {
                None => depth[t] == 0,
                Some(s) => depth.get(s).is_some_and(|d| d + 1 == depth[t]),
            };
            let reason = (active.iter())
                .filter(|(_, taker)| taker == t)
                .filter(from_depth)
                .min_by_key(|(r, taker)| {
                    let n = &r.need;
                    // A dependency on the package itself before one it stands in for, then by
                    // what it names: the order claims are declared in decides nothing.
                    let through = &n.target != taker;
                    (n.source_id().is_some(), n.any_of.is_some(), n.source_id(), through, n.target.clone(), n.req.to_string(), n.index)
                });
            if let Some((r, _)) = reason {
                why.insert(t.clone(), r.need.why(r.rule));
            }
        }
        // The dependents in id order, not in the order the specs were named.
        let mut brought_by: BTreeMap<PkgId, IndexSet<String>> = BTreeMap::new();
        let mut sources: Vec<&PkgId> = order.iter().collect();
        sources.sort();
        for p in sources {
            for (r, t) in by_source(p) {
                brought_by.entry(t).or_default().insert(r.need.label().unwrap_or_default().to_owned());
            }
        }
        (order, why, brought_by)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan(
        &self,
        d: &Derived,
        chosen: BTreeMap<PkgId, Entry>,
        replaced: Vec<(Replaced, PkgId, Vec<String>, bool)>,
        libraries: Vec<Library>,
        order: &IndexSet<PkgId>,
        why: &BTreeMap<PkgId, Why>,
        brought_by: &BTreeMap<PkgId, IndexSet<String>>,
    ) -> Plan {
        let options = self.request.options;
        let mut notes: IndexSet<String> = IndexSet::new();
        // A release at no version is taken to meet a range, with a note.
        for id in order {
            let Some(e) = chosen.get(id).filter(|e| e.release.at.version().is_none()) else { continue };
            let on_id = |r: &&Rec| &r.need.target == id && r.taker == Taker::Itself && r.kind == Kind::Active;
            for r in d.needs.iter().filter(on_id) {
                if matches!(&r.need.req, Req::Range(q) if *q != TrackedReq::any()) {
                    let l = r.need.live(r.kind);
                    let label = e.release.label();
                    notes.insert(format!("{label} is at an unreleased commit; taken to meet {} (from {})", l.req, l.source));
                }
            }
        }
        // Under --no-deps, each present package's declared range or thorns the plan breaks.
        if options.no_deps {
            for r in &d.needs {
                let breaking = |e: &&Entry| e.is_new() && r.taker == Taker::Itself;
                let Some(e) = chosen.get(&r.need.target).filter(breaking) else {
                    continue;
                };
                if r.kind != Kind::Active && violates(&e.release, &r.need) {
                    let by = r.need.label().unwrap_or_default();
                    notes.insert(format!("{by} needs {} {}; --no-deps leaves it unmet", r.need.target, r.need.req));
                }
            }
        }
        // Who stands in for whom: a keep line for a present package, a note otherwise.
        let mut keep: IndexMap<PkgId, IndexSet<String>> = IndexMap::new();
        for r in d.needs.iter().filter(|r| r.kind == Kind::Active) {
            let Taker::Other { taker, note } = &r.taker else { continue };
            match chosen.get(taker) {
                Some(e) if e.present.is_some() && !e.is_new() => {
                    keep.entry(taker.clone()).or_default().insert(note.clone());
                }
                _ => {
                    notes.insert(note.clone());
                }
            }
        }
        let mut plan = Plan::default();
        let mut choices: Vec<Choice> = (d.needs.iter())
            .filter_map(|r| {
                let f = r.need.any_of.as_ref()?;
                Some(Choice {
                    dependent: f.key.0.clone(),
                    dependent_label: r.need.label().unwrap_or_default().to_owned(),
                    claim: f.key.1,
                    label: f.label.clone(),
                    alternatives: f.alternatives.clone(),
                    chosen: r.taken_by().cloned().unwrap_or_else(|| r.need.target.clone()),
                    rule: r.rule.unwrap_or(ChoiceRule::FirstListed),
                })
            })
            .collect();
        choices.sort_by(|a, b| (&a.dependent, a.claim).cmp(&(&b.dependent, b.claim)));
        for id in order {
            let Some(e) = chosen.get(id) else { continue };
            let named = self.named(id).is_some();
            let why = why.get(id).cloned().unwrap_or(Why::Named);
            let enabled: Vec<String> = e.enabled.iter().cloned().collect();
            let mut recorded = true;
            match &e.present {
                None => plan.steps.push(Step::Add { release: (*e.release).clone(), thorns: enabled, why }),
                Some(p) if e.is_new() => {
                    // A named package, a flesh, or one --upgrade-all moved says nothing more.
                    let unflagged = named || e.release.flesh || why == Why::UpgradeAll;
                    let flag = match () {
                        _ if unflagged => None,
                        _ if options.upgrade_all => Some("--upgrade-all"),
                        _ if options.upgrade => Some("--upgrade"),
                        _ => None,
                    };
                    let (from, to) = (p.release.clone(), (*e.release).clone());
                    plan.steps.push(Step::Upgrade { from, to, thorns: enabled, why, flag });
                    if let Origin::Base { repo } = &p.origin {
                        let replaced = Replaced::Base { repo: repo.clone() };
                        let (by, flesh, thorns) = (id.clone(), e.release.flesh, Vec::new());
                        plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                    }
                }
                Some(p) => {
                    let newly: Vec<String> =
                        e.enabled.iter().filter(|t| !contains(&p.enabled, t)).cloned().collect();
                    match &p.origin {
                        Origin::Added if named => {
                            plan.steps.push(Step::Readd { release: (*e.release).clone(), thorns: enabled })
                        }
                        _ if !newly.is_empty() => {
                            let (id, label, release) = (id.clone(), e.release.label(), (*e.release).clone());
                            plan.steps.push(Step::AddThorns { id, label, release, thorns: newly, why });
                        }
                        Origin::Base { .. } if named => {
                            let note = format!("{} is in the installation's base already", e.release.label());
                            plan.steps.push(Step::Keep { id: id.clone(), note });
                            recorded = false;
                        }
                        _ => recorded = false,
                    }
                }
            }
            for (what, by, thorns, flesh) in &replaced {
                if by == id {
                    let (replaced, by) = (what.clone(), by.clone());
                    let (thorns, flesh) = (thorns.clone(), *flesh);
                    plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                }
            }
            for note in keep.get(id).into_iter().flatten() {
                plan.steps.push(Step::Keep { id: id.clone(), note: note.clone() });
            }
            if recorded {
                let choices = choices.iter().filter(|c| &c.chosen == id).map(Choice::key).collect();
                let by = brought_by.get(id).map(|b| b.iter().cloned().collect()).unwrap_or_default();
                let requested = match () {
                    _ if named => Some(true),
                    _ if e.present.is_none() => Some(false),
                    _ => None,
                };
                let commit = e.release.commit.clone();
                plan.records.push(Record { id: id.clone(), commit, requested, by, choices });
            }
            if e.is_new() {
                if e.release.pending {
                    plan.pending.push(e.release.label());
                }
                plan.untrusted.extend(self.untrusted(&e.release));
            }
        }
        for l in &libraries {
            if l.placements.iter().any(|p| p.new) {
                if l.release.pending && !plan.pending.contains(&l.release.label()) {
                    plan.pending.push(l.release.label());
                }
                plan.untrusted.extend(self.untrusted(&l.release));
            }
        }
        plan.notes = notes.into_iter().collect();
        plan.libraries = libraries;
        plan.rebuild = plan.steps.iter().any(|s| match s {
            Step::Add { release, .. } | Step::Upgrade { to: release, .. } => release.flesh,
            Step::Replace { flesh, .. } => *flesh,
            _ => false,
        });
        plan.choices = choices;
        plan
    }

    /// The snapshot `r` is, when nobody vouches for it.
    fn untrusted(&self, r: &Release) -> Option<Untrusted> {
        if !self.cat.untrusted.contains(&(r.id.clone(), r.commit.clone())) {
            return None;
        }
        let tip = match &r.at {
            At::Tip { name, .. } => Some(name.clone()),
            _ => None,
        };
        Some(Untrusted { id: r.id.clone(), commit: r.commit.clone(), tip })
    }

    /// Every library the plan places: the libraries of packages it brings in or moves (and the
    /// libraries those go inside), at one release every placement accepts. A library an unchanged
    /// package installed stays at its commit, and a new placement must accept it (and when
    /// unchanged packages hold it at different commits, nothing can). Placement goes on under
    /// `--no-deps`.
    fn place_libraries(&mut self, chosen: &BTreeMap<PkgId, Entry>) -> Res<Result<Vec<Library>, Failure>> {
        let mut installed: BTreeMap<PkgId, Vec<(String, Placement, Live)>> = BTreeMap::new();
        for (id, e) in chosen {
            let Some(p) = e.present.as_ref().filter(|_| !e.is_new()) else { continue };
            for l in &p.libraries {
                let placement = Placement {
                    by: id.clone(),
                    by_label: e.release.label(),
                    inside: l.inside.clone(),
                    into: l.into.clone(),
                    req: None,
                    new: false,
                };
                let source = format!("installed {}", e.release.label());
                let live = Live { target: l.id.clone(), req: format!("#{}", short(&l.commit)), source, ignore: None };
                installed.entry(l.id.clone()).or_default().push((l.commit.clone(), placement, live));
            }
        }
        // A package brought in or moved places every library it claims; a present one given
        // more thorns, those its new thorns need (a claim whose needed-by names one of them).
        let mut roots: Vec<&Entry> = chosen.values().filter(|e| e.changed()).collect();
        roots.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.release.id.cmp(&b.release.id)));
        let roots: Vec<(PkgId, Rel, Option<BTreeSet<String>>)> = (roots.iter())
            .map(|e| {
                let only = (!e.is_new()).then(|| e.enabled.iter().filter(|t| e.thorn_is_new(t)).cloned().collect());
                (e.release.id.clone(), e.release.clone(), only)
            })
            .collect();
        let mut selected: BTreeMap<PkgId, Rel> = BTreeMap::new();
        // Each round reads every placing release (the packages, then the libraries selected so
        // far, inside each package that placed them) and selects each library again, until
        // nothing changes.
        for _ in 0..64 {
            let mut placements: BTreeMap<PkgId, Vec<(Placement, Live)>> = BTreeMap::new();
            let mut frontier: VecDeque<(PkgId, Option<PkgId>, Rel, Option<BTreeSet<String>>)> =
                roots.iter().map(|(id, r, only)| (id.clone(), None, r.clone(), only.clone())).collect();
            let mut seen: BTreeSet<(PkgId, PkgId)> = BTreeSet::new();
            while let Some((by, inside, r, only)) = frontier.pop_front() {
                for c in r.claims.iter().filter(|c| c.kind == "dependency") {
                    let Some(into) = &c.into else { continue };
                    if let Some(only) = &only
                        && !c.needed_by.iter().any(|n| contains(only, n))
                    {
                        continue;
                    }
                    let (label, ignore) = (r.label(), format!("{}:dependency:{}", r.id, c.target));
                    let (lib, req) = match claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) {
                        Ok(x) => x,
                        Err(detail) => {
                            let claim = claim_label(c);
                            let r = Refusal::BadClaim { by: label, claim, detail, ignore };
                            return Ok(Err(Failure::of(r, Vec::new(), vec![by.clone()])));
                        }
                    };
                    let (target, source) = (lib.clone(), label.clone());
                    let live = Live { target, req: req.to_string(), source, ignore: Some(ignore) };
                    let placement = Placement {
                        by: by.clone(),
                        by_label: label,
                        inside: inside.clone(),
                        into: into.clone(),
                        req: Some(req),
                        new: true,
                    };
                    placements.entry(lib.clone()).or_default().push((placement, live));
                    // An installed library's own libraries are installed with it.
                    if let Some(sel) = selected.get(&lib)
                        && !installed.contains_key(&lib)
                        && seen.insert((by.clone(), lib.clone()))
                    {
                        frontier.push_back((by.clone(), Some(lib.clone()), sel.clone(), None));
                    }
                }
            }
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            self.cat.ensure(placements.keys().cloned())?;
            let mut next: BTreeMap<PkgId, Rel> = BTreeMap::new();
            for (lib, ps) in &placements {
                let reqs: Vec<Req> = ps.iter().filter_map(|(p, _)| p.req.clone()).collect();
                let mut lives: Vec<Live> = ps.iter().map(|(_, l)| l.clone()).collect();
                let parties: Vec<PkgId> = ps.iter().map(|(p, _)| p.by.clone()).collect();
                let target = lib.clone();
                if let Some(held) = installed.get(lib) {
                    lives.extend(held.iter().map(|(_, _, l)| l.clone()));
                    let commits: BTreeSet<&String> = held.iter().map(|(c, _, _)| c).collect();
                    let release = held_release(self.cat, lib, &held[0].0);
                    if commits.len() > 1 || reqs.iter().any(|q| q.fit(&release) == Fit::No) {
                        let r = Refusal::LibraryClash { library: target, placements: lives.clone() };
                        let mut parties = parties;
                        parties.extend(held.iter().map(|(_, p, _)| p.by.clone()));
                        return Ok(Err(Failure::of(r, lives, parties)));
                    }
                    next.insert(lib.clone(), Rc::new(release));
                    continue;
                }
                let (candidates, _) = self.cat.candidates(lib, &reqs)?;
                let fitting = eligible(&candidates, &reqs, &reqs, None);
                let not_a_library = !fitting.is_empty() && fitting.iter().all(|r| !r.library);
                let ok: Vec<&Rel> = fitting.into_iter().filter(|r| r.library).collect();
                let Some(best) = preferred(ok).first().map(|r| (*r).clone()) else {
                    let r = if !self.cat.known(lib) {
                        Refusal::NotRegistered { target, by: lives.first().cloned() }
                    } else if not_a_library {
                        // Releases fit, but as packages with thorns: the claim
                        // gives a place (`into`) to something that is no library.
                        let (p, _) = &ps[0];
                        let (by, into) = (p.by_label.clone(), p.into.clone());
                        let ignore = lives.first().and_then(|l| l.ignore.clone());
                        Refusal::NotALibrary { target, by, into, ignore }
                    } else if ps.iter().map(|(p, _)| &p.by).collect::<BTreeSet<_>>().len() > 1 {
                        Refusal::LibraryClash { library: target, placements: lives.clone() }
                    } else {
                        Refusal::NoReleaseFits { target, constraints: lives.clone() }
                    };
                    return Ok(Err(Failure::of(r, lives, parties)));
                };
                next.insert(lib.clone(), best);
            }
            let same = next.len() == selected.len()
                && next.iter().zip(&selected).all(|((a, ra), (b, rb))| a == b && ra.same(rb));
            if same {
                let mut out = Vec::new();
                for (lib, release) in selected {
                    let new = placements.remove(&lib).unwrap_or_default();
                    let mut ps: Vec<Placement> = new.into_iter().map(|(p, _)| p).collect();
                    if let Some(held) = installed.remove(&lib) {
                        ps.extend(held.into_iter().map(|(_, p, _)| p));
                    }
                    // New placements first, then by package: not in the order specs were named.
                    let key = |p: &Placement| (!p.new, p.by.clone(), p.inside.clone(), p.into.clone());
                    ps.sort_by_key(key);
                    out.push(Library { release: (*release).clone(), placements: ps });
                }
                return Ok(Ok(out));
            }
            selected = next;
        }
        bail!("Libraries placed inside each other without end")
    }
}

/// Under `--replace`, a base repo sharing a thorn name with a package the plan brings in or moves
/// gives up those checkouts (the whole repo, once none is left), as `compose` drops only the
/// checkouts an added package claims.
fn replace_colliding(
    chosen: &mut BTreeMap<PkgId, Entry>,
    replaced: &mut Vec<(Replaced, PkgId, Vec<String>, bool)>,
) {
    let mut newcomers: Vec<(usize, PkgId)> =
        chosen.iter().filter(|(_, e)| e.changed()).map(|(id, e)| (e.order, id.clone())).collect();
    newcomers.sort();
    for (_, by) in newcomers {
        let Some(e) = chosen.get(&by) else { continue };
        let new: BTreeSet<String> = e.enabled.iter().filter(|t| e.thorn_is_new(t)).cloned().collect();
        let bases: Vec<PkgId> = (chosen.iter())
            .filter(|(id, b)| *id != &by && b.base_repo().is_some())
            .filter(|(_, b)| b.enabled.iter().any(|t| contains(&new, t)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in bases {
            let b = chosen.get_mut(&id).expect("listed above");
            let repo = b.base_repo().unwrap_or_default().to_owned();
            let taken: Vec<String> = b.enabled.iter().filter(|t| contains(&new, t)).cloned().collect();
            b.enabled.retain(|t| !contains(&new, t));
            let thorns = if b.enabled.is_empty() {
                chosen.remove(&id);
                Vec::new()
            } else {
                taken
            };
            replaced.push((Replaced::Base { repo }, by.clone(), thorns, false));
        }
    }
}
