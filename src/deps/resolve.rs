//! The resolver: named specs plus the installation's present packages → a [`Plan`].
//!
//! A pass seeds the chosen set with the present packages (their declared dependencies become
//! passive constraints) and works off a queue of needs, the named specs first: single-target
//! dependencies to a fixpoint, then any-of dependencies one at a time in a fixed order
//! (dependent, then declaration order), the fixpoint re-run after each. Each need is taken by
//! its target itself, a package an override says satisfies it, or a chosen fork that stands in
//! for it; its range joins the target's constraints, and the target keeps its release if that
//! meets them all, else moves to the newest release that does (when it may move).
//!
//! Restarts keep the result independent of the order needs arrive in:
//! - a release that changes after its dependencies were pushed restarts the pass;
//! - so does a dropped constraint (its release changed) that a decision or a failure used;
//! - a release picked at a restart is reused while it fits, and dropped (with another pass)
//!   once a constraint it was picked under is gone; a present package with a pick seeds no
//!   passive constraints, since it is about to move;
//! - a fork that comes in after its upstream was planned, and stands in for it, leaves a hint:
//!   the next pass holds the upstream's needs until the fork is chosen.
//!
//! A refusal found during a pass waits for the pass to finish, since a restart may take away
//! the constraint behind it. Passes scale with the number of packages; steps are bounded.
// Plan §2.2. Greedy: no backtracking into other packages' choices beyond the restarts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cactup_ecosystem_common::api::ClaimInfo;
use cactup_ecosystem_common::manifest::ForkOf;
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use indexmap::IndexSet;

use super::plan::{Choice, ChoiceRule, Library, Placement, Plan, Record, Replaced, Step, Untrusted};
use super::refusal::{Live, Refusal, Unmet};
use super::{
    claim_target, conflict, forks, short, target_text, At, Facts, Fit, Origin, PkgId, Present, Release, Req,
    Request, Requirement, Requirements, Rules, Why,
};
use crate::Res;
use anyhow::bail;

/// Passes (restarts) before "no consistent set", at the least; more with more packages, since
/// a restart settles at most one package.
pub(super) const MIN_PASSES: usize = 32;
/// Needs and decisions processed over every pass before "no consistent set".
pub(super) const MAX_STEPS: usize = 50_000;

/// Resolves `request` against the registry `facts` gives, checking CCL requirements with `ccl`.
/// A refusal comes back as a [`Refusal`] inside the error (downcast to read it); anything else
/// is a failure of `facts` or an interrupt.
pub fn resolve(request: &Request, facts: &mut dyn Facts, ccl: &dyn Requirements) -> Res<Plan> {
    let mut present: Vec<Present> = request.present.to_vec();
    for p in &mut present {
        request.rules.apply(&mut p.release);
    }
    present.sort_by(|a, b| a.release.id.cmp(&b.release.id));
    let mut r = Resolver {
        request,
        present,
        cat: Catalog::new(facts, request.rules),
        ccl,
        picks: BTreeMap::new(),
        hints: BTreeMap::new(),
        steps: 0,
        restarted: BTreeSet::new(),
        s: Pass::default(),
    };
    r.cat.ensure(request.specs.iter().map(|s| s.id.clone()))?;
    let mut passes = 0;
    loop {
        passes += 1;
        if passes > r.max_passes() {
            return Err(r.no_consistent_set().into());
        }
        match r.pass()? {
            Outcome::Done => break,
            Outcome::Restart(t) => {
                r.restarted.insert(t);
            }
        }
    }
    r.finish()
}

/// The releases among `candidates` that meet every requirement: usable (or `present`, which is
/// installed whatever its state), fitting, and yanked only for an exact requirement.
fn eligible<'r>(candidates: &'r [Release], reqs: &[Req], present: Option<&Release>) -> Vec<&'r Release> {
    candidates
        .iter()
        .filter(|r| r.usable || present.is_some_and(|p| p.same(r)))
        .filter(|r| reqs.iter().all(|q| q.fit(r) != Fit::No))
        .filter(|r| !r.yanked || reqs.iter().any(|q| q.is_exact() && q.fit(r) == Fit::Yes))
        .collect()
}

/// Approved releases, or (when none) the ones awaiting review.
fn approved_first(ok: Vec<&Release>) -> Vec<&Release> {
    let approved: Vec<&Release> = ok.iter().copied().filter(|r| !r.pending).collect();
    if approved.is_empty() { ok } else { approved }
}

/// The newest: versions by semver (within a range, tracks are the range's to choose), above any
/// release at no version; ties go to the one listed first.
fn newest<'r>(pool: &[&'r Release]) -> Option<&'r Release> {
    let newer = |a: &Release, b: &Release| match (&a.at, &b.at) {
        (At::Version(x), At::Version(y)) => x.version > y.version,
        (At::Version(_), _) => true,
        _ => false,
    };
    let mut best: Option<&Release> = None;
    for r in pool {
        if best.is_none_or(|b| newer(r, b)) {
            best = Some(r);
        }
    }
    best
}

/// The registry as the resolver sees it: releases with the rules applied, loaded once.
struct Catalog<'a> {
    facts: &'a mut dyn Facts,
    rules: &'a Rules,
    /// `None`: the Cactarium has no such package.
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    /// Snapshots read, by package and what was asked (a commit or a tip), and by the commit found.
    snapshots: BTreeMap<(PkgId, String), Release>,
    untrusted: BTreeSet<(PkgId, String)>,
}

impl<'a> Catalog<'a> {
    fn new(facts: &'a mut dyn Facts, rules: &'a Rules) -> Catalog<'a> {
        let (loaded, snapshots, untrusted) = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
        Catalog { facts, rules, loaded, snapshots, untrusted }
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
                        r
                    })
                    .collect()
            });
            self.loaded.insert(id, releases);
        }
        Ok(())
    }

    fn releases(&self, id: &PkgId) -> &[Release] {
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
        let published = |r: &Release| r.at.version().map(|v| (v.clone(), Some(r.commit.clone())));
        self.releases(id).iter().filter_map(published).collect()
    }

    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Release> {
        let key = (id.clone(), at.to_owned());
        if let Some(r) = self.snapshots.get(&key) {
            return Ok(r.clone());
        }
        let snap = self.facts.snapshot(id, at)?;
        let mut release = snap.release;
        // What was asked must be what came back: a commit that id names (a version frozen there
        // answers as that version), or the tip asked for.
        let answered = match &release.at {
            At::Tip { name, .. } => name == at,
            _ => cactup_ecosystem_common::slug::is_commit_prefix(at) && release.commit.starts_with(at),
        };
        if !answered {
            let (label, commit) = (release.label(), &release.commit);
            bail!("Asked the Cactarium for {id} at {at}, and it answered with {label} at commit {commit}");
        }
        self.rules.apply(&mut release);
        if snap.untrusted {
            self.untrusted.insert((id.clone(), release.commit.clone()));
        }
        // A tip read is also the read at the commit it found.
        self.snapshots.insert((id.clone(), release.commit.clone()), release.clone());
        self.snapshots.insert(key, release.clone());
        Ok(release)
    }
}

enum Outcome {
    Done,
    /// This package's decision has to be made again, with what this pass learned.
    Restart(PkgId),
}

/// Where a constraint comes from: the key it is dropped by.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Named,
    UpgradeAll,
    /// A release's claim: `label` is `owner/name@version`.
    Release { label: String, id: PkgId },
}

#[derive(Clone, Debug)]
struct Constraint {
    source: Source,
    req: Req,
    thorns: Vec<String>,
    /// From a present package that is not changing: it narrows what a moving package may take,
    /// but a package that may not move is not refused over it (that predates this command).
    /// Under `--no-deps` it narrows nothing, and a plan that breaks it says so in a note.
    passive: bool,
}

#[derive(Clone, Debug)]
enum Thorns {
    All,
    Some(Vec<String>),
}

#[derive(Clone, Debug)]
struct Need {
    target: PkgId,
    req: Req,
    thorns: Thorns,
    why: Why,
    source: Source,
    /// Held once for a hinted fork; never again.
    held: bool,
}

#[derive(Clone, Debug)]
struct AnyOf {
    claim: ClaimInfo,
    index: usize,
    dependent: PkgId,
    label: String,
}

/// A release decided on at a restart, with the constraints live on its package then.
#[derive(Clone, Debug)]
struct Pick {
    release: Release,
    sources: BTreeSet<Source>,
}

/// A package in the chosen set.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) release: Release,
    pub(super) enabled: BTreeSet<String>,
    /// What the installation has; `None` for a package the plan brings in.
    pub(super) present: Option<Present>,
    /// When the pass first touched it (`usize::MAX`: never): the order of the steps.
    pub(super) order: usize,
    why: Option<Why>,
    /// Its dependencies were pushed in this pass.
    expanded: bool,
    /// Every thorn of whichever release it ends up at.
    whole: bool,
    keep: Vec<String>,
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

/// A failure found during a pass, with the constraints it was found under.
struct Failure {
    refusal: Refusal,
    on: Option<(PkgId, BTreeSet<Source>)>,
}

/// One pass's state, thrown away on a restart.
#[derive(Default)]
struct Pass {
    chosen: BTreeMap<PkgId, Entry>,
    next_order: usize,
    constraints: BTreeMap<PkgId, Vec<Constraint>>,
    singles: VecDeque<Need>,
    /// Needs held until a hinted fork is chosen.
    held: Vec<Need>,
    any_of: BTreeMap<(PkgId, usize), AnyOf>,
    /// Claims already pushed: (dependent, index of the claim).
    pushed: BTreeSet<(PkgId, usize)>,
    /// What was dropped in favor of whom: (what, by, the base thorns taken over, flesh).
    replaced: Vec<(Replaced, PkgId, Vec<String>, bool)>,
    failure: Option<Failure>,
    notes: IndexSet<String>,
    choices: Vec<Choice>,
    brought_by: BTreeMap<PkgId, IndexSet<String>>,
    named: BTreeSet<PkgId>,
    touched: BTreeSet<PkgId>,
    /// Moving packages that already took their newest release in this pass.
    settled: BTreeSet<PkgId>,
    /// Constraints a decision or a failure read: (target, source).
    used: BTreeSet<(PkgId, Source)>,
    /// Present packages whose passive constraints were not seeded, because of a pick.
    unseeded: BTreeSet<PkgId>,
}

struct Resolver<'a> {
    request: &'a Request<'a>,
    present: Vec<Present>,
    cat: Catalog<'a>,
    ccl: &'a dyn Requirements,
    picks: BTreeMap<PkgId, Pick>,
    /// Upstream → the fork that stood in for it in an earlier pass.
    hints: BTreeMap<PkgId, PkgId>,
    steps: usize,
    /// Packages whose decision restarted a pass.
    restarted: BTreeSet<PkgId>,
    s: Pass,
}

/// Who takes a need.
enum Identity {
    Itself,
    Other { taker: PkgId, note: String },
    NotStandIn { fork: PkgId },
}

/// The dependency as people read it: `et/carpet@^1.2`, or `any of et/carpet, et/cactuspugh`.
pub(super) fn claim_label(c: &ClaimInfo) -> String {
    if c.alternatives.is_empty() {
        target_text(&c.target, c.range.as_deref(), c.sha.as_deref())
    } else {
        let text = |a: &cactup_ecosystem_common::api::AlternativeInfo| {
            target_text(&a.target, a.range.as_deref(), a.sha.as_deref())
        };
        let alts: Vec<String> = c.alternatives.iter().map(text).collect();
        format!("any of {}", alts.join(", "))
    }
}

/// `r`'s claim to be a fork of `upstream`, as the crate reads one.
fn fork_claim(r: &Release, upstream: &PkgId) -> Option<ForkOf> {
    let c = r.claims.iter().find(|c| c.kind == "fork-of" && c.target == upstream.as_str())?;
    let target = target_text(&c.target, c.range.as_deref(), c.sha.as_deref()).parse().ok()?;
    Some(ForkOf { target, note: None })
}

/// Whether a dependency applies to these thorns: `needed-by` names one of them, or (when the
/// package first enters) it names none.
fn applies(c: &ClaimInfo, scope: &BTreeSet<String>, first: bool) -> bool {
    if c.needed_by.is_empty() {
        return first;
    }
    c.needed_by.iter().any(|n| scope.iter().any(|t| t.eq_ignore_ascii_case(n)))
}

fn contains(set: &BTreeSet<String>, name: &str) -> bool {
    set.iter().any(|t| t.eq_ignore_ascii_case(name))
}

fn thorns_of(needs: &[String]) -> Thorns {
    if needs.is_empty() { Thorns::All } else { Thorns::Some(needs.to_vec()) }
}

impl Resolver<'_> {
    fn max_passes(&self) -> usize {
        MIN_PASSES.max(4 * (self.present.len() + self.cat.loaded.len()))
    }

    fn fail(&mut self, r: Refusal) {
        if self.s.failure.is_none() {
            self.s.failure = Some(Failure { refusal: r, on: None });
        }
    }

    /// A failure over the constraints on `t`: dropped if one of them goes.
    fn fail_on(&mut self, t: &PkgId, r: Refusal) {
        if self.s.failure.is_none() {
            let sources = self.live_sources(t);
            self.s.failure = Some(Failure { refusal: r, on: Some((t.clone(), sources)) });
        }
    }

    fn pass(&mut self) -> Res<Outcome> {
        self.s = Pass::default();
        for p in &self.present {
            let entry = Entry {
                release: p.release.clone(),
                enabled: p.enabled.clone(),
                present: Some(p.clone()),
                order: usize::MAX,
                why: None,
                expanded: false,
                whole: false,
                keep: Vec::new(),
            };
            self.s.chosen.insert(p.release.id.clone(), entry);
        }
        self.seed_passive();
        for spec in self.request.specs {
            self.s.named.insert(spec.id.clone());
            let thorns = spec.thorns.clone().map_or(Thorns::All, Thorns::Some);
            let (target, req) = (spec.id.clone(), spec.req.clone());
            let need = Need { target, req, thorns, why: Why::Named, source: Source::Named, held: false };
            self.s.singles.push_back(need);
        }
        if self.request.options.upgrade_all {
            for p in &self.present {
                let At::Version(v) = &p.release.at else { continue };
                // Any version on the track it is on.
                let mut req = TrackedReq::any();
                req.alts[0].track = v.track.clone();
                let thorns = Thorns::Some(p.enabled.iter().cloned().collect());
                let target = p.release.id.clone();
                let (req, why, source) = (Req::Range(req), Why::UpgradeAll, Source::UpgradeAll);
                self.s.singles.push_back(Need { target, req, thorns, why, source, held: false });
            }
        }
        loop {
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return Err(self.no_consistent_set().into());
            }
            if let Some(need) = self.s.singles.pop_front() {
                self.load_wave(&need)?;
                if let Some(t) = self.process(need)? {
                    return Ok(Outcome::Restart(t));
                }
                continue;
            }
            // Needs held for a fork that has come in since.
            let chosen = &self.s.chosen;
            let hints = &self.hints;
            let (ready, wait): (Vec<Need>, Vec<Need>) = std::mem::take(&mut self.s.held)
                .into_iter()
                .partition(|n| hints.get(&n.target).is_some_and(|f| chosen.contains_key(f)));
            self.s.held = wait;
            if !ready.is_empty() {
                self.s.singles.extend(ready);
                continue;
            }
            if let Some(key) = self.s.any_of.keys().next().cloned() {
                let a = self.s.any_of.remove(&key).expect("the key was just read");
                self.decide(a)?;
                continue;
            }
            if !self.s.held.is_empty() {
                // The fork never came: the upstream takes its needs after all.
                self.s.singles.extend(std::mem::take(&mut self.s.held));
                continue;
            }
            break;
        }
        if let Some(t) = self.stale_pick() {
            return Ok(Outcome::Restart(t));
        }
        if let Some(f) = self.s.failure.take() {
            if let Some((t, sources)) = &f.on
                && !sources.is_subset(&self.live_sources(t))
            {
                return Ok(Outcome::Restart(t.clone()));
            }
            return Err(f.refusal.into());
        }
        Ok(Outcome::Done)
    }

    /// The present packages' declared dependencies, as passive constraints: a single target's,
    /// and an any-of's on the alternative holding it. None for a package with a pick (it is about
    /// to move, and its present claims would mislead every decision before it does).
    fn seed_passive(&mut self) {
        let present: Vec<Present> = self.present.clone();
        for p in &present {
            let id = &p.release.id;
            if self.picks.get(id).is_some_and(|pick| !pick.release.same(&p.release)) {
                self.s.unseeded.insert(id.clone());
                continue;
            }
            let source = Source::Release { label: p.release.label(), id: id.clone() };
            for c in p.release.claims.iter().filter(|c| c.kind == "dependency") {
                if c.into.is_some() || !applies(c, &p.enabled, true) {
                    continue;
                }
                let target = if c.alternatives.is_empty() {
                    let target = claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()).ok();
                    target.map(|t| (t, c.needs.clone()))
                } else {
                    c.alternatives
                        .iter()
                        .filter_map(|a| {
                            let target = claim_target(&a.target, a.range.as_deref(), a.sha.as_deref()).ok()?;
                            Some((target, a.needs.clone()))
                        })
                        .find(|((alt, req), _)| self.holds_itself(alt, req, true))
                };
                let Some(((target, req), thorns)) = target else { continue };
                let c = Constraint { source: source.clone(), req, thorns, passive: true };
                self.s.constraints.entry(target).or_default().push(c);
            }
        }
    }

    /// Whether `t` itself is chosen (present or planned, as asked) at a release meeting `req`.
    fn holds_itself(&self, t: &PkgId, req: &Req, present: bool) -> bool {
        self.s.chosen.get(t).is_some_and(|e| e.present.is_some() == present && req.fit(&e.release) != Fit::No)
    }

    /// Loads, in one wave, the need's target and every target still queued.
    fn load_wave(&mut self, need: &Need) -> Res<()> {
        let mut ids = vec![need.target.clone()];
        ids.extend(self.s.singles.iter().map(|n| n.target.clone()));
        for a in self.s.any_of.values() {
            ids.extend(a.claim.alternatives.iter().filter_map(|alt| PkgId::parse(&alt.target).ok()));
        }
        self.cat.ensure(ids)
    }

    /// Whether a constraint narrows anything: a passive one does not under `--no-deps`.
    fn counts(&self, c: &Constraint) -> bool {
        !(c.passive && self.request.options.no_deps)
    }

    fn live_sources(&self, t: &PkgId) -> BTreeSet<Source> {
        let all = self.s.constraints.get(t).into_iter().flatten();
        all.filter(|c| self.counts(c)).map(|c| c.source.clone()).collect()
    }

    fn live(&self, target: &PkgId, c: &Constraint) -> Live {
        let (source, ignore) = match &c.source {
            Source::Named => ("the command line".to_owned(), None),
            Source::UpgradeAll => ("--upgrade-all".to_owned(), None),
            Source::Release { label, id } => {
                let source = if c.passive { format!("installed {label}") } else { label.clone() };
                (source, Some(format!("{id}:dependency:{target}")))
            }
        };
        Live { target: target.clone(), req: c.req.to_string(), source, ignore }
    }

    fn lives(&self, target: &PkgId) -> Vec<Live> {
        let mut out: Vec<Live> = Vec::new();
        for c in self.s.constraints.get(target).into_iter().flatten().filter(|c| self.counts(c)) {
            let l = self.live(target, c);
            if !out.contains(&l) {
                out.push(l);
            }
        }
        out
    }

    /// The live constraints of the packages that kept restarting (of every package, if none did).
    fn no_consistent_set(&self) -> Refusal {
        let mut constraints: Vec<Live> = self.restarted.iter().flat_map(|t| self.lives(t)).collect();
        if constraints.is_empty() {
            constraints = self.s.constraints.keys().flat_map(|t| self.lives(t)).collect();
        }
        Refusal::NoConsistentSet { constraints }
    }

    fn no_release(&self, t: &PkgId) -> Refusal {
        if self.cat.is_flesh(t) {
            Refusal::FleshUnmet { target: t.clone(), constraints: self.lives(t) }
        } else {
            Refusal::NoReleaseFits { target: t.clone(), constraints: self.lives(t) }
        }
    }

    fn need_live(&self, need: &Need) -> Live {
        let (source, req) = (need.source.clone(), need.req.clone());
        let c = Constraint { source, req, thorns: Vec::new(), passive: false };
        self.live(&need.target, &c)
    }

    /// Records that a decision on `t` read the constraints on it now.
    fn note_used(&mut self, t: &PkgId) {
        for s in self.live_sources(t) {
            self.s.used.insert((t.clone(), s));
        }
    }

    /// Whether a package may move to another release: brought in by the plan, named, touched
    /// under `--upgrade`, any present package under `--upgrade-all`, or a flesh (an unmet flesh
    /// dependency plans the flesh's replacement). Never a pseudo-package.
    fn movable(&self, t: &PkgId) -> bool {
        let Some(e) = self.s.chosen.get(t) else { return true };
        if t.is_repo() {
            return false;
        }
        e.present.is_none()
            || self.s.named.contains(t)
            || self.request.options.upgrade_all
            || (self.request.options.upgrade && self.s.touched.contains(t))
            || e.release.flesh
    }

    /// Whether `r` meets the constraints on `t`'s versions (`active_only`: leaving out those of
    /// present packages that are not changing), and the thorns they name that it lacks.
    fn fits(&mut self, t: &PkgId, r: &Release, active_only: bool) -> (bool, Vec<String>) {
        self.note_used(t);
        let mut version_ok = true;
        let mut missing: Vec<String> = Vec::new();
        for c in self.s.constraints.get(t).into_iter().flatten().filter(|c| self.counts(c)) {
            if active_only && c.passive {
                continue;
            }
            if c.req.fit(r) == Fit::No {
                version_ok = false;
            }
            for th in &c.thorns {
                if r.thorn(th).is_none() && !missing.iter().any(|m| m.eq_ignore_ascii_case(th)) {
                    missing.push(th.clone());
                }
            }
        }
        (version_ok, missing)
    }

    /// The served releases of `t`, plus the read at each commit `reqs` name that no served
    /// release is at.
    fn candidates(&mut self, t: &PkgId, reqs: &[Req]) -> Res<Vec<Release>> {
        let mut candidates: Vec<Release> = self.cat.releases(t).to_vec();
        for q in reqs {
            if let Req::Commit(sha) = q
                && !t.is_repo()
                && self.cat.known(t)
                && !candidates.iter().any(|r| r.commit.starts_with(sha.as_str()))
            {
                candidates.push(self.cat.snapshot(t, sha)?);
            }
        }
        Ok(candidates)
    }

    /// The newest release of `t` meeting every constraint on it (and `extra`), with every thorn
    /// they name (and, for a present package moving unnamed, every thorn it has enabled):
    /// approved releases first, one awaiting review only when none fits; a yanked one only for an
    /// exact constraint; never an invalid one. A release picked at a restart wins while it meets
    /// them, even one awaiting review over an approved one: the constraint that forced it may not
    /// have come back yet in this pass, and [`Self::stale_pick`] drops it at the end of the pass
    /// once that constraint is gone for good.
    fn select(&mut self, t: &PkgId, extra: Option<&Req>) -> Res<Option<Release>> {
        if extra.is_none() {
            self.note_used(t);
        }
        let constraints: Vec<Constraint> =
            self.s.constraints.get(t).into_iter().flatten().filter(|c| self.counts(c)).cloned().collect();
        let mut reqs: Vec<Req> = constraints.iter().map(|c| c.req.clone()).collect();
        reqs.extend(extra.cloned());
        let mut required: Vec<String> = constraints.iter().flat_map(|c| c.thorns.iter().cloned()).collect();
        let present = self.s.chosen.get(t).and_then(|e| e.present.clone());
        if let Some(p) = &present
            && !self.s.named.contains(t)
        {
            required.extend(p.enabled.iter().cloned());
        }
        let mut candidates = self.candidates(t, &reqs)?;
        if let Some(p) = &present
            && !candidates.iter().any(|r| r.same(&p.release))
        {
            candidates.push(p.release.clone());
        }
        let ok = eligible(&candidates, &reqs, present.as_ref().map(|p| &p.release));
        let has_required = |r: &&Release| required.iter().all(|th| r.thorn(th).is_some());
        let ok: Vec<&Release> = ok.into_iter().filter(has_required).collect();
        if let Some(p) = self.picks.get(t)
            && ok.iter().any(|r| r.same(&p.release))
        {
            return Ok(Some(p.release.clone()));
        }
        Ok(newest(&approved_first(ok)).cloned())
    }

    /// Who takes a need for `t`: `t` itself when chosen, a package an override says satisfies it,
    /// a chosen fork that stands in for the versions asked, or (to be brought in) `t` itself.
    fn identity(&self, t: &PkgId, req: &Req) -> Identity {
        if self.s.chosen.contains_key(t) {
            return Identity::Itself;
        }
        for x in self.request.rules.satisfiers(t) {
            if self.s.chosen.contains_key(x) {
                let note = format!("{x} stands in for {t} (a claims override)");
                return Identity::Other { taker: x.clone(), note };
            }
        }
        for (id, e) in &self.s.chosen {
            let Some(fork) = fork_claim(&e.release, t) else { continue };
            let published = self.cat.published(t);
            if t.slug(req).is_some_and(|dep| fork.stands_in_for(&dep, &published)) {
                let note = format!("{} stands in for {t}", e.release.label());
                return Identity::Other { taker: id.clone(), note };
            }
            return Identity::NotStandIn { fork: id.clone() };
        }
        Identity::Itself
    }

    fn touch(&mut self, t: &PkgId, why: &Why) {
        let next = self.s.next_order;
        if let Some(e) = self.s.chosen.get_mut(t)
            && e.order == usize::MAX
        {
            e.order = next;
            e.why = Some(why.clone());
            self.s.next_order += 1;
        }
    }

    fn source_label(source: &Source) -> Option<String> {
        match source {
            Source::Release { label, .. } => Some(label.clone()),
            _ => None,
        }
    }

    /// One need. `Some(t)`: the pass restarts, `t`'s decision to be made again.
    fn process(&mut self, mut need: Need) -> Res<Option<PkgId>> {
        let t = need.target.clone();
        if let Req::Tip(tip) = &need.req {
            if t.is_repo() || !self.cat.known(&t) {
                self.fail(Refusal::NotRegistered { target: t, by: None });
                return Ok(None);
            }
            let snap = self.cat.snapshot(&t, tip)?;
            need.req = Req::Commit(snap.commit);
        }
        match self.identity(&t, &need.req) {
            Identity::Itself => {}
            Identity::Other { taker, note } => {
                self.take_by(&taker, &need, note);
                return Ok(None);
            }
            Identity::NotStandIn { fork } => {
                // An unmet flesh dependency plans the flesh's replacement instead.
                if !self.cat.is_flesh(&t) {
                    let fork_label = self.s.chosen[&fork].release.label();
                    let live = self.need_live(&need);
                    self.fail(Refusal::ForkDoesNotStandIn { fork, fork_label, live });
                    return Ok(None);
                }
            }
        }
        if !self.s.chosen.contains_key(&t) {
            if !need.held
                && let Some(f) = self.hints.get(&t)
                && !self.s.chosen.contains_key(f)
            {
                need.held = true;
                self.s.held.push(need);
                return Ok(None);
            }
            if !self.cat.known(&t) {
                let by = (need.source != Source::Named).then(|| self.need_live(&need));
                self.fail(Refusal::NotRegistered { target: t, by });
                return Ok(None);
            }
        }
        let thorns = match &need.thorns {
            Thorns::Some(v) => v.clone(),
            Thorns::All => Vec::new(),
        };
        let (source, req) = (need.source.clone(), need.req.clone());
        let c = Constraint { source, req, thorns, passive: false };
        self.s.constraints.entry(t.clone()).or_default().push(c);
        if matches!(need.source, Source::Release { .. }) {
            self.s.touched.insert(t.clone());
        }
        self.touch(&t, &need.why);
        if let Some(e) = self.s.chosen.get(&t) {
            let current = e.release.clone();
            let origin = e.present.as_ref().map(|p| p.origin.clone());
            let movable = self.movable(&t);
            let (version_ok, missing) = self.fits(&t, &current, !movable);
            let fits = version_ok && missing.is_empty();
            let options = self.request.options;
            let named = self.s.named.contains(&t);
            // A named added package re-resolves to its newest release, as re-adding does; a
            // named base package only under --upgrade; --upgrade alone moves only what no longer
            // fits; --upgrade-all moves everything.
            let wants_newest = movable
                && !current.flesh
                && !self.s.settled.contains(&t)
                && match origin {
                    None => false,
                    Some(Origin::Added) => named || options.upgrade_all,
                    Some(Origin::Base { .. }) => (named && options.upgrade) || options.upgrade_all,
                };
            if wants_newest || (movable && !fits) {
                self.s.settled.insert(t.clone());
                match self.select(&t, None)? {
                    Some(new) if !new.same(&current) => {
                        if let Some(restart) = self.change(&t, new) {
                            return Ok(Some(restart));
                        }
                    }
                    Some(_) => {}
                    None if fits => {}
                    None => {
                        let r = self.no_release(&t);
                        self.fail_on(&t, r);
                        return Ok(None);
                    }
                }
            } else if !fits {
                let installed = current.label();
                let live = self.need_live(&need);
                if version_ok {
                    self.fail_on(&t, Refusal::PresentLacksThorns { installed, thorns: missing, live });
                } else {
                    self.fail_on(&t, Refusal::PresentOutsideRange { installed, live });
                }
                return Ok(None);
            }
        } else {
            let Some(release) = self.select(&t, None)? else {
                let r = self.no_release(&t);
                self.fail_on(&t, r);
                return Ok(None);
            };
            if release.library {
                if let Source::Release { label, id } = &need.source {
                    let (by, ignore) = (label.clone(), format!("{id}:dependency:{t}"));
                    let detail =
                        format!("{t} is a library, and the dependency does not say where it goes (into)");
                    self.fail(Refusal::BadClaim { by, claim: t.to_string(), detail, ignore });
                } else {
                    self.fail(Refusal::NamedLibrary { target: t });
                }
                return Ok(None);
            }
            let flesh = release.flesh;
            let entry = Entry {
                release,
                enabled: BTreeSet::new(),
                present: None,
                order: self.s.next_order,
                why: Some(need.why.clone()),
                expanded: false,
                whole: false,
                keep: Vec::new(),
            };
            self.s.next_order += 1;
            self.s.chosen.insert(t.clone(), entry);
            if flesh && let Some(restart) = self.replace_flesh(&t) {
                return Ok(Some(restart));
            }
        }
        self.enable(&t, &need, None);
        if self.request.options.replace
            && self.s.chosen.get(&t).is_some_and(Entry::is_new)
            && let Some(restart) = self.absorb(&t)
        {
            return Ok(Some(restart));
        }
        Ok(self.hint_for(&t))
    }

    /// Enables a need's thorns in `taker` (the target itself, or whoever stands in for it), and
    /// pushes the dependencies the newly enabled thorns bring.
    fn enable(&mut self, taker: &PkgId, need: &Need, note: Option<String>) {
        let e = &self.s.chosen[taker];
        let names: Vec<String> = match &need.thorns {
            Thorns::All => e.release.thorns.iter().map(|t| t.thorn.clone()).collect(),
            Thorns::Some(list) => {
                let mut names = Vec::new();
                let mut missing = Vec::new();
                for want in list {
                    match e.release.thorn(want) {
                        Some(t) => names.push(t.thorn.clone()),
                        None => missing.push(want.clone()),
                    }
                }
                if !missing.is_empty() {
                    let (taker, live) = (e.release.label(), self.need_live(need));
                    self.fail(Refusal::MissingThorns { taker, thorns: missing, live });
                    return;
                }
                names
            }
        };
        if matches!(need.thorns, Thorns::All) && taker == &need.target {
            self.s.chosen.get_mut(taker).expect("the taker is chosen").whole = true;
        }
        self.enable_names(taker, names, Self::source_label(&need.source), note);
    }

    fn enable_names(&mut self, taker: &PkgId, names: Vec<String>, by: Option<String>, note: Option<String>) {
        let e = self.s.chosen.get_mut(taker).expect("the taker is chosen");
        let newly: BTreeSet<String> = names.into_iter().filter(|n| !contains(&e.enabled, n)).collect();
        e.enabled.extend(newly.iter().cloned());
        let first = !e.expanded && e.is_new();
        if let Some(note) = note {
            if e.present.is_some() && !e.is_new() {
                if !e.keep.contains(&note) {
                    e.keep.push(note);
                }
            } else {
                self.s.notes.insert(note);
            }
        }
        if let Some(label) = by {
            self.s.brought_by.entry(taker.clone()).or_default().insert(label);
        }
        if !newly.is_empty() || first {
            self.expand(taker, &newly, first);
        }
    }

    /// A need taken by a fork or an override's stand-in.
    fn take_by(&mut self, taker: &PkgId, need: &Need, note: String) {
        self.touch(taker, &need.why);
        self.enable(taker, need, Some(note));
    }

    /// Moves `t` to `new`. `Some`: restart (its dependencies were pushed already, or a constraint
    /// the old release made was read by a decision).
    fn change(&mut self, t: &PkgId, new: Release) -> Option<PkgId> {
        let sources = self.live_sources(t);
        self.picks.insert(t.clone(), Pick { release: new.clone(), sources });
        let e = self.s.chosen.get_mut(t).expect("a chosen package changes");
        if e.expanded {
            return Some(t.clone());
        }
        let old = std::mem::replace(&mut e.release, new);
        // A whole package is all of its new release; otherwise the enabled thorns carry over
        // where the new release has them (an unnamed move made sure it has them all).
        e.enabled = if e.whole {
            e.release.thorn_names()
        } else {
            e.enabled.iter().filter_map(|n| e.release.thorn(n).map(|t| t.thorn.clone())).collect()
        };
        let flesh = e.release.flesh;
        if let Some(restart) = self.drop_source(&old.label()) {
            return Some(restart);
        }
        if flesh {
            return self.replace_flesh(t);
        }
        None
    }

    /// Drops the constraints a release made. `Some(t)` when a decision on `t` read one of them.
    fn drop_source(&mut self, label: &str) -> Option<PkgId> {
        let mut hit: Option<PkgId> = None;
        let used = &self.s.used;
        for (t, cs) in self.s.constraints.iter_mut() {
            cs.retain(|c| {
                let gone = matches!(&c.source, Source::Release { label: l, .. } if l == label);
                if gone && hit.is_none() && used.contains(&(t.clone(), c.source.clone())) {
                    hit = Some(t.clone());
                }
                !gone
            });
        }
        hit
    }

    /// `t`, a flesh, comes in: the flesh the installation has (a base repo, or an added flesh
    /// package) is replaced; a second planned flesh is refused.
    fn replace_flesh(&mut self, t: &PkgId) -> Option<PkgId> {
        let others: Vec<PkgId> = (self.s.chosen.iter())
            .filter(|(id, e)| *id != t && e.release.flesh)
            .map(|(id, _)| id.clone())
            .collect();
        for id in others {
            let e = self.s.chosen.remove(&id).expect("listed above");
            let replaced = match e.present.as_ref().map(|p| &p.origin) {
                Some(Origin::Base { repo }) => Replaced::Base { repo: repo.clone() },
                Some(Origin::Added) => Replaced::Added { id: id.clone() },
                None => {
                    let (first, second) = (e.release.label(), self.s.chosen[t].release.label());
                    self.fail(Refusal::TwoFleshes { first, second });
                    continue;
                }
            };
            self.s.replaced.push((replaced, t.clone(), Vec::new(), true));
            if let Some(restart) = self.drop_source(&e.release.label()) {
                return Some(restart);
            }
        }
        None
    }

    /// Under `--replace`, `n` takes the place of every unchanged base package it forks: it takes
    /// the needs that package served (if it stands in for them) and every thorn the installation
    /// has enabled from it.
    fn absorb(&mut self, n: &PkgId) -> Option<PkgId> {
        let release = self.s.chosen[n].release.clone();
        let related: Vec<PkgId> = (self.s.chosen.iter())
            .filter(|(id, e)| *id != n && e.base_repo().is_some() && forks(&release, id))
            .map(|(id, _)| id.clone())
            .collect();
        for u in related {
            let fork = fork_claim(&release, &u).expect("a fork-of claim on it");
            let published = self.cat.published(&u);
            let active: Vec<Constraint> =
                self.s.constraints.get(&u).into_iter().flatten().filter(|c| !c.passive).cloned().collect();
            for c in &active {
                if !u.slug(&c.req).is_some_and(|dep| fork.stands_in_for(&dep, &published)) {
                    let live = self.live(&u, c);
                    let (fork, fork_label) = (n.clone(), release.label());
                    self.fail(Refusal::ForkDoesNotStandIn { fork, fork_label, live });
                    return None;
                }
            }
            let ue = self.s.chosen.remove(&u).expect("listed above");
            let repo = ue.base_repo().unwrap_or_default().to_owned();
            let missing: Vec<String> =
                ue.enabled.iter().filter(|t| release.thorn(t).is_none()).cloned().collect();
            if !missing.is_empty() {
                let (by, by_label) = (n.clone(), release.label());
                self.fail(Refusal::ReplacementLacksThorns { by, by_label, base_repo: repo, thorns: missing });
                return None;
            }
            self.s.replaced.push((Replaced::Base { repo }, n.clone(), Vec::new(), false));
            if let Some(restart) = self.drop_source(&ue.release.label()) {
                return Some(restart);
            }
            let names = ue.enabled.iter().filter_map(|t| release.thorn(t).map(|x| x.thorn.clone())).collect();
            self.enable_names(n, names, None, None);
        }
        None
    }

    /// When `f`, just brought in, forks an upstream the plan brought in unnamed and stands in for
    /// every need on it, records that (`Some(upstream)`: restart), so the next pass holds the
    /// upstream's needs until the fork is chosen, whichever order they come in.
    fn hint_for(&mut self, f: &PkgId) -> Option<PkgId> {
        let e = self.s.chosen.get(f)?;
        if !e.is_new() {
            return None;
        }
        let release = e.release.clone();
        for c in release.claims.iter().filter(|c| c.kind == "fork-of") {
            let Ok(u) = PkgId::parse(&c.target) else { continue };
            let Some(ue) = self.s.chosen.get(&u) else { continue };
            if ue.present.is_some() || self.s.named.contains(&u) || self.hints.get(&u) == Some(f) {
                continue;
            }
            let Some(fork) = fork_claim(&release, &u) else { continue };
            let published = self.cat.published(&u);
            let stands_in = (self.s.constraints.get(&u).into_iter().flatten())
                .all(|c| u.slug(&c.req).is_some_and(|dep| fork.stands_in_for(&dep, &published)));
            if stands_in && ue.enabled.iter().all(|t| release.thorn(t).is_some()) {
                self.hints.insert(u.clone(), f.clone());
                return Some(u);
            }
        }
        None
    }

    /// Pushes `t`'s dependencies that the thorns enabled now bring (all of its enabled thorns
    /// when its release first enters). Libraries are placement, never needs.
    fn expand(&mut self, t: &PkgId, newly: &BTreeSet<String>, first: bool) {
        if self.request.options.no_deps {
            return;
        }
        let e = self.s.chosen.get_mut(t).expect("a chosen package expands");
        e.expanded = true;
        let release = e.release.clone();
        let scope = if first { e.enabled.clone() } else { newly.clone() };
        let label = release.label();
        for (i, c) in release.claims.iter().enumerate() {
            if c.kind != "dependency" || c.into.is_some() || !applies(c, &scope, first) {
                continue;
            }
            if !self.s.pushed.insert((t.clone(), i)) {
                continue;
            }
            if !c.alternatives.is_empty() {
                let a = AnyOf { claim: c.clone(), index: i, dependent: t.clone(), label: label.clone() };
                self.s.any_of.insert((t.clone(), i), a);
                continue;
            }
            let (target, req) = match claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) {
                Ok(x) => x,
                Err(detail) => {
                    let (by, claim, ignore) = (label.clone(), claim_label(c), format!("{t}:dependency:{}", c.target));
                    self.fail(Refusal::BadClaim { by, claim, detail, ignore });
                    continue;
                }
            };
            let why = Why::Needed {
                by: label.clone(),
                by_id: t.clone(),
                claim: claim_label(c),
                req: req.to_string(),
                needed_by: c.needed_by.clone(),
                rule: None,
            };
            let source = Source::Release { label: label.clone(), id: t.clone() };
            let thorns = thorns_of(&c.needs);
            self.s.singles.push_back(Need { target, req, thorns, why, source, held: false });
        }
    }

    /// Decides one any-of dependency: an alternative present at a release that fits it; else one
    /// already in the plan at a release that fits; else the first listed that conflicts with
    /// nothing present or planned; else the first listed.
    fn decide(&mut self, a: AnyOf) -> Res<()> {
        let label = claim_label(&a.claim);
        let mut alts: Vec<(PkgId, Req, Thorns)> = Vec::new();
        for alt in &a.claim.alternatives {
            match claim_target(&alt.target, alt.range.as_deref(), alt.sha.as_deref()) {
                Ok((id, req)) => alts.push((id, req, thorns_of(&alt.needs))),
                Err(detail) => {
                    let (by, ignore) = (a.label.clone(), format!("{}:dependency:{}", a.dependent, alt.target));
                    self.fail(Refusal::BadClaim { by, claim: label, detail, ignore });
                    return Ok(());
                }
            }
        }
        self.cat.ensure(alts.iter().map(|(id, _, _)| id.clone()))?;
        let held = |me: &Self, (id, req, _): &(PkgId, Req, Thorns), present: bool| {
            let is_present = |e: &Entry| e.present.is_some() == present;
            match me.identity(id, req) {
                Identity::Itself => me.holds_itself(id, req, present),
                Identity::Other { taker, .. } => me.s.chosen.get(&taker).is_some_and(is_present),
                Identity::NotStandIn { .. } => false,
            }
        };
        let present = alts.iter().position(|alt| held(self, alt, true));
        let mut pick = present.map(|i| (i, ChoiceRule::Present));
        if pick.is_none() {
            let planned = alts.iter().position(|alt| held(self, alt, false));
            pick = planned.map(|i| (i, ChoiceRule::Planned));
        }
        if pick.is_none() {
            for (i, alt) in alts.iter().enumerate() {
                if !self.conflicts(alt)? {
                    let rule =
                        if i == 0 { ChoiceRule::FirstListed } else { ChoiceRule::FirstWithoutConflict };
                    pick = Some((i, rule));
                    break;
                }
            }
        }
        let (i, rule) = pick.unwrap_or((0, ChoiceRule::FirstListed));
        let alternatives: Vec<PkgId> = alts.iter().map(|(id, _, _)| id.clone()).collect();
        let (target, req, thorns) = alts.swap_remove(i);
        self.s.choices.push(Choice {
            dependent: a.dependent.clone(),
            dependent_label: a.label.clone(),
            claim: a.index,
            label: label.clone(),
            alternatives,
            chosen: target.clone(),
            rule,
        });
        let why = Why::Needed {
            by: a.label.clone(),
            by_id: a.dependent.clone(),
            claim: label,
            req: req.to_string(),
            needed_by: a.claim.needed_by.clone(),
            rule: Some(rule),
        };
        let source = Source::Release { label: a.label, id: a.dependent };
        self.s.singles.push_back(Need { target, req, thorns, why, source, held: false });
        Ok(())
    }

    /// Whether an any-of alternative would conflict with what is present or planned: no release
    /// of it fits, a thorn it needs is enabled in another package, an `incompatible-with` either
    /// way, or a fork relation either way.
    fn conflicts(&mut self, (id, req, thorns): &(PkgId, Req, Thorns)) -> Res<bool> {
        if !self.cat.known(id) {
            return Ok(true);
        }
        let Some(candidate) = self.select(id, Some(req))? else { return Ok(true) };
        let names: Vec<String> = match thorns {
            Thorns::All => candidate.thorns.iter().map(|t| t.thorn.clone()).collect(),
            Thorns::Some(v) => v.clone(),
        };
        for (other, e) in &self.s.chosen {
            if other == id {
                continue;
            }
            let incompatible = |a, b| conflict::incompatible(a, b);
            if names.iter().any(|n| contains(&e.enabled, n))
                || incompatible(&candidate, &e.release)
                || incompatible(&e.release, &candidate)
                || forks(&candidate, other)
                || forks(&e.release, id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A pick to drop at the end of a pass: one in use whose constraints are not all live any
    /// more (it may no longer be what fits best), or one that kept a present package's passive
    /// constraints out of a pass in which that package did not move after all.
    fn stale_pick(&mut self) -> Option<PkgId> {
        let mut stale: Option<PkgId> = None;
        for (t, pick) in &self.picks {
            let in_use = self.s.chosen.get(t).is_some_and(|e| e.release.same(&pick.release));
            let gone = if in_use {
                !pick.sources.is_subset(&self.live_sources(t))
            } else {
                self.s.unseeded.contains(t)
            };
            if gone {
                stale = Some(t.clone());
                break;
            }
        }
        if let Some(t) = &stale {
            self.picks.remove(t);
        }
        stale
    }

    /// After the last pass: replacements, conflicts, libraries, the CCL check, then the plan.
    fn finish(mut self) -> Res<Plan> {
        if self.request.options.replace {
            self.replace_colliding();
        }
        if let Some(r) = conflict::conflicts(&self.s.chosen) {
            return Err(r.into());
        }
        let libraries = self.place_libraries()?;
        self.check_requirements()?;
        let mut entries: Vec<(&PkgId, &Entry)> =
            self.s.chosen.iter().filter(|(_, e)| e.order != usize::MAX).collect();
        entries.sort_by_key(|(_, e)| e.order);
        let mut notes: IndexSet<String> = IndexSet::new();
        // A release at no version is taken to meet a range, with a note.
        for (id, e) in &entries {
            if e.release.at.version().is_some() {
                continue;
            }
            for c in self.s.constraints.get(*id).into_iter().flatten() {
                if c.passive || !matches!(&c.req, Req::Range(q) if *q != TrackedReq::any()) {
                    continue;
                }
                let l = self.live(id, c);
                let label = e.release.label();
                notes.insert(format!("{label} is at a commit no version names; taken to meet {} (from {})", l.req, l.source));
            }
        }
        notes.extend(self.no_deps_notes());
        notes.extend(self.s.notes.iter().cloned());
        let options = self.request.options;
        let mut plan = Plan { notes: notes.into_iter().collect(), ..Plan::default() };
        for (id, e) in &entries {
            let id: &PkgId = id;
            let named = self.s.named.contains(id);
            let why = e.why.clone().unwrap_or(Why::Named);
            let enabled: Vec<String> = e.enabled.iter().cloned().collect();
            let mut recorded = true;
            match &e.present {
                None => plan.steps.push(Step::Add { release: e.release.clone(), thorns: enabled, why }),
                Some(p) if e.is_new() => {
                    // A named package, a flesh, or one --upgrade-all moved says nothing more.
                    let unflagged = named || e.release.flesh || why == Why::UpgradeAll;
                    let flag = match () {
                        _ if unflagged => None,
                        _ if options.upgrade_all => Some("--upgrade-all"),
                        _ if options.upgrade => Some("--upgrade"),
                        _ => None,
                    };
                    let (from, to) = (p.release.clone(), e.release.clone());
                    plan.steps.push(Step::Upgrade { from, to, thorns: enabled, why, flag });
                    if let Origin::Base { repo } = &p.origin {
                        let replaced = Replaced::Base { repo: repo.clone() };
                        let (by, flesh) = (id.clone(), e.release.flesh);
                        let thorns = Vec::new();
                        plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                    }
                }
                Some(p) => {
                    let newly: Vec<String> =
                        e.enabled.iter().filter(|t| !contains(&p.enabled, t)).cloned().collect();
                    match &p.origin {
                        Origin::Added if named => {
                            plan.steps.push(Step::Readd { release: e.release.clone(), thorns: enabled })
                        }
                        _ if !newly.is_empty() => {
                            let (id, label) = (id.clone(), e.release.label());
                            plan.steps.push(Step::AddThorns { id, label, thorns: newly, why });
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
            for (replaced, by, thorns, flesh) in &self.s.replaced {
                if by == id {
                    let (replaced, by) = (replaced.clone(), by.clone());
                    let (thorns, flesh) = (thorns.clone(), *flesh);
                    plan.steps.push(Step::Replace { replaced, by, thorns, flesh, explicit: named });
                }
            }
            for note in &e.keep {
                plan.steps.push(Step::Keep { id: id.clone(), note: note.clone() });
            }
            if recorded {
                let choices = self.s.choices.iter().filter(|c| &c.chosen == id).map(Choice::key).collect();
                let by = self.s.brought_by.get(id).map(|b| b.iter().cloned().collect()).unwrap_or_default();
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
        plan.libraries = libraries;
        plan.rebuild = plan.steps.iter().any(|s| match s {
            Step::Add { release, .. } | Step::Upgrade { to: release, .. } => release.flesh,
            Step::Replace { flesh, .. } => *flesh,
            _ => false,
        });
        plan.choices = self.s.choices.clone();
        Ok(plan)
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

    /// Under `--no-deps`, each present package's declared range or thorns that the plan breaks.
    fn no_deps_notes(&self) -> Vec<String> {
        if !self.request.options.no_deps {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (t, cs) in &self.s.constraints {
            let Some(e) = self.s.chosen.get(t).filter(|e| e.is_new()) else { continue };
            for c in cs.iter().filter(|c| c.passive) {
                let lacks = c.thorns.iter().any(|th| e.release.thorn(th).is_none());
                if c.req.fit(&e.release) == Fit::No || lacks {
                    let by = Self::source_label(&c.source).unwrap_or_default();
                    out.push(format!("{by} needs {t} {}; --no-deps leaves it unmet", c.req));
                }
            }
        }
        out
    }

    /// Under `--replace`, a base repo sharing a thorn name with a package the plan brings in or
    /// moves gives up those checkouts (the whole repo, once none is left), as `compose` drops only
    /// the checkouts an added package claims. (A base package the newcomer forks went whole,
    /// during the pass.)
    fn replace_colliding(&mut self) {
        let mut newcomers: Vec<(usize, PkgId)> = (self.s.chosen.iter())
            .filter(|(_, e)| e.changed())
            .map(|(id, e)| (e.order, id.clone()))
            .collect();
        newcomers.sort();
        for (_, by) in newcomers {
            let Some(e) = self.s.chosen.get(&by) else { continue };
            let new: BTreeSet<String> = e.enabled.iter().filter(|t| e.thorn_is_new(t)).cloned().collect();
            let bases: Vec<PkgId> = (self.s.chosen.iter())
                .filter(|(id, b)| *id != &by && b.base_repo().is_some())
                .filter(|(_, b)| b.enabled.iter().any(|t| contains(&new, t)))
                .map(|(id, _)| id.clone())
                .collect();
            for id in bases {
                let b = self.s.chosen.get_mut(&id).expect("listed above");
                let repo = b.base_repo().unwrap_or_default().to_owned();
                let taken: Vec<String> = b.enabled.iter().filter(|t| contains(&new, t)).cloned().collect();
                b.enabled.retain(|t| !contains(&new, t));
                let thorns = if b.enabled.is_empty() {
                    self.s.chosen.remove(&id);
                    Vec::new()
                } else {
                    taken
                };
                self.s.replaced.push((Replaced::Base { repo }, by.clone(), thorns, false));
            }
        }
    }

    /// Every library the plan places: the libraries of packages it brings in or moves (and the
    /// libraries those go inside), at one release every placement accepts. A library installed
    /// by an unchanged package stays at its commit, and a new placement must accept it. Placement
    /// goes on under `--no-deps`.
    fn place_libraries(&mut self) -> Res<Vec<Library>> {
        let mut installed: BTreeMap<PkgId, (String, Vec<(Placement, Live)>)> = BTreeMap::new();
        for (id, e) in &self.s.chosen {
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
                let slot = installed.entry(l.id.clone()).or_insert_with(|| (l.commit.clone(), Vec::new()));
                slot.1.push((placement, live));
            }
        }
        let mut roots: Vec<&Entry> = self.s.chosen.values().filter(|e| e.is_new()).collect();
        roots.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.release.id.cmp(&b.release.id)));
        let roots: Vec<(PkgId, Release)> =
            roots.iter().map(|e| (e.release.id.clone(), e.release.clone())).collect();
        let mut selected: BTreeMap<PkgId, Release> = BTreeMap::new();
        // Each round reads every placing release (the packages, then the libraries selected so
        // far, inside each package that placed them) and selects each library again, until
        // nothing changes.
        for _ in 0..64 {
            let mut placements: BTreeMap<PkgId, Vec<(Placement, Live)>> = BTreeMap::new();
            let mut frontier: VecDeque<(PkgId, Option<PkgId>, Release)> =
                roots.iter().map(|(id, r)| (id.clone(), None, r.clone())).collect();
            let mut seen: BTreeSet<(PkgId, PkgId)> = BTreeSet::new();
            while let Some((by, inside, r)) = frontier.pop_front() {
                for c in r.claims.iter().filter(|c| c.kind == "dependency") {
                    let Some(into) = &c.into else { continue };
                    let (label, ignore) = (r.label(), format!("{}:dependency:{}", r.id, c.target));
                    let (lib, req) = match claim_target(&c.target, c.range.as_deref(), c.sha.as_deref()) {
                        Ok(x) => x,
                        Err(detail) => {
                            let claim = claim_label(c);
                            return Err(Refusal::BadClaim { by: label, claim, detail, ignore }.into());
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
                        frontier.push_back((by.clone(), Some(lib.clone()), sel.clone()));
                    }
                }
            }
            self.cat.ensure(placements.keys().cloned())?;
            let mut next: BTreeMap<PkgId, Release> = BTreeMap::new();
            for (lib, ps) in &placements {
                let reqs: Vec<Req> = ps.iter().filter_map(|(p, _)| p.req.clone()).collect();
                let mut lives: Vec<Live> = ps.iter().map(|(_, l)| l.clone()).collect();
                let target = lib.clone();
                if let Some((commit, held)) = installed.get(lib) {
                    lives.extend(held.iter().map(|(_, l)| l.clone()));
                    let release = self.cat.releases(lib).iter().find(|r| &r.commit == commit).cloned();
                    let release = release.unwrap_or_else(|| Release {
                        id: lib.clone(),
                        at: At::Unknown(commit.clone()),
                        commit: commit.clone(),
                        tag: None,
                        url: String::new(),
                        contact: None,
                        flesh: false,
                        library: true,
                        yanked: false,
                        pending: false,
                        usable: true,
                        thorns: Vec::new(),
                        claims: Vec::new(),
                    });
                    if reqs.iter().any(|q| q.fit(&release) == Fit::No) {
                        return Err(Refusal::LibraryClash { library: target, placements: lives }.into());
                    }
                    next.insert(lib.clone(), release);
                    continue;
                }
                let candidates = self.candidates(lib, &reqs)?;
                let ok: Vec<&Release> =
                    eligible(&candidates, &reqs, None).into_iter().filter(|r| r.library).collect();
                let Some(best) = newest(&approved_first(ok)).cloned() else {
                    if !self.cat.known(lib) {
                        let by = lives.into_iter().next();
                        return Err(Refusal::NotRegistered { target, by }.into());
                    }
                    let sources: BTreeSet<&PkgId> = ps.iter().map(|(p, _)| &p.by).collect();
                    if sources.len() > 1 {
                        return Err(Refusal::LibraryClash { library: target, placements: lives }.into());
                    }
                    return Err(Refusal::NoReleaseFits { target, constraints: lives }.into());
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
                    if let Some((_, held)) = installed.remove(&lib) {
                        ps.extend(held.into_iter().map(|(p, _)| p));
                    }
                    // New placements first, then by package: not in the order specs were named.
                    let key = |p: &Placement| (!p.new, p.by.clone(), p.inside.clone(), p.into.clone());
                    ps.sort_by_key(key);
                    out.push(Library { release, placements: ps });
                }
                return Ok(out);
            }
            selected = next;
        }
        bail!("Libraries placed inside each other without end")
    }

    /// Every hard CCL requirement of every thorn the plan enables is met by the final set;
    /// otherwise a refusal, whatever the number of providers (cactup never fills in a
    /// dependency nobody declared).
    fn check_requirements(&mut self) -> Res<()> {
        let unmet = conflict::unmet_requirements(&self.s.chosen, self.ccl);
        if unmet.is_empty() {
            return Ok(());
        }
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
            out.push(Unmet { thorn, package, requirement, providers });
        }
        Err(Refusal::Unclaimed(out).into())
    }
}
