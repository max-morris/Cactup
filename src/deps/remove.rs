//! What a `remove` does beyond the packages named: refuse while other packages depend on them,
//! and take along the packages it leaves unneeded. Computed from the present packages' current
//! claims (with the overrides applied), never from the `by` lists recorded at add time.
// Plan §2.4 (added-for defaults), §4.2; decision 9.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::plan::{table, ChoiceKey, LineKind};
use super::refusal::Refusal;
use super::resolve::claim_label;
use super::{forks, Origin, PkgId, Present, Rules};

/// An `added-for` entry of `installation.toml`, as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddedFor {
    pub requested: bool,
    pub by: Vec<String>,
    /// The any-of dependencies that chose it.
    pub choices: Vec<ChoiceKey>,
    /// The `[[added]]` commit the entry was written for.
    pub for_commit: Option<String>,
}

/// Whether an added package counts as named by the user: when its `added-for` entry says so,
/// and also when there is none (installations from before dependencies were recorded) or it was
/// written for another commit (an older binary re-added or upgraded the package), so that no
/// orphan pass ever removes a package the user may have named.
pub fn is_requested(entry: Option<&AddedFor>, commit: &str) -> bool {
    match entry {
        None => true,
        Some(a) => a.requested || a.for_commit.as_deref() != Some(commit),
    }
}

/// A package the removal leaves unneeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    pub id: PkgId,
    pub label: String,
    /// Whose dependencies brought it in, as recorded (informational).
    pub by: Vec<String>,
}

impl Orphan {
    /// `brought in by max/z4c@1.2.0, nothing else needs it`.
    pub fn why(&self) -> String {
        if self.by.is_empty() {
            "brought in as a dependency, nothing else needs it".to_owned()
        } else {
            format!("brought in by {}, nothing else needs it", self.by.join(", "))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemovePlan {
    pub removed: Vec<PkgId>,
    /// `owner/name@version` of each removed package, as the installation has it.
    pub removed_labels: Vec<String>,
    /// Removed too, once confirmed (none under `--no-deps`).
    pub orphans: Vec<Orphan>,
}

impl RemovePlan {
    /// Whether the plan goes beyond what was named, so it is shown and confirmed.
    pub fn beyond_named(&self) -> bool {
        !self.orphans.is_empty()
    }

    /// The plan as people read it, each line with its kind.
    pub fn lines(&self, command: &str) -> Vec<(LineKind, String)> {
        let mut rows: Vec<[String; 4]> = Vec::new();
        for label in &self.removed_labels {
            rows.push(["remove".into(), label.clone(), String::new(), "named".into()]);
        }
        for o in &self.orphans {
            rows.push(["remove".into(), o.label.clone(), String::new(), o.why()]);
        }
        let mut out = vec![(LineKind::Header, format!("Plan for {command}:"))];
        out.extend(table(&rows).into_iter().map(|l| (LineKind::Step, l)));
        out
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn render(&self, command: &str) -> String {
        self.lines(command).into_iter().map(|(_, l)| l + "\n").collect()
    }
}

/// An `[[added]]` entry that only gives a base repo's package (`id`, present as the base) more
/// thorns. Removing it takes those thorns away and leaves the base: it is its own node of the
/// removal, held only by dependencies that need one of its thorns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    pub id: PkgId,
    pub thorns: Vec<String>,
    /// As installed, for the plan's lines.
    pub label: String,
    /// The `[[added]]` commit, against which its `added-for` entry is read.
    pub commit: String,
}

/// Plans removing `targets`: refuses while a remaining package depends on one of them (unless
/// `no_deps`), and lists the added packages, never named, that only the removed ones needed
/// (to a fixpoint, cycles included). A package nothing reached before the removal (one an
/// earlier `--no-deps` left behind) counts as named: it is never swept, nor what it needs.
/// A target, or an orphan, that is one of `extensions` is that extension: its thorns go, the
/// base package stays. `returning` are the base packages that come back when an added package
/// that replaced their base repos goes (each with that package: a target, or an orphan swept
/// with them), as read where the base has them: they hold what they meet after the removal (a
/// release that fits the dependency, with the thorns it needs), never before.
pub fn plan_remove(
    present: &[Present],
    added_for: &BTreeMap<PkgId, AddedFor>,
    targets: &[PkgId],
    rules: &Rules,
    no_deps: bool,
    extensions: &[Extension],
    returning: &[(PkgId, Present)],
) -> Result<RemovePlan, Refusal> {
    let mut all: BTreeMap<PkgId, Present> = BTreeMap::new();
    for p in present {
        let mut p = p.clone();
        rules.apply(&mut p.release);
        all.insert(p.release.id.clone(), p);
    }
    // Each returning base package a node of its own (its package may be the very target going),
    // with the added package whose going brings it back.
    let mut back: BTreeMap<PkgId, PkgId> = BTreeMap::new();
    let mut back_with: BTreeMap<PkgId, PkgId> = BTreeMap::new();
    for (replacer, p) in returning {
        let mut p = p.clone();
        rules.apply(&mut p.release);
        // Keyed apart, its release its own (a label, claims, forks read as the package's).
        let node = PkgId(format!("{}#base", p.release.id));
        back.insert(node.clone(), p.release.id.clone());
        back_with.insert(node.clone(), replacer.clone());
        all.insert(node, p);
    }
    // Each extension its own node, beside the base node of its package, which keeps the rest.
    let mut ext: BTreeMap<PkgId, (PkgId, &Extension)> = BTreeMap::new();
    for e in extensions {
        let Some(base) = all.get_mut(&e.id) else { continue };
        let has = |t: &String| e.thorns.iter().any(|x| x.eq_ignore_ascii_case(t));
        base.enabled.retain(|t| !has(t));
        let mut node = base.clone();
        let node_id = PkgId(format!("{}#added", e.id));
        node.release.id = node_id.clone();
        node.enabled = e.thorns.iter().cloned().collect();
        node.origin = Origin::Added;
        node.libraries.clear();
        all.insert(node_id.clone(), node);
        ext.insert(node_id, (e.id.clone(), e));
    }
    let node_of = |id: &PkgId| ext.iter().find(|(_, (b, _))| b == id).map_or_else(|| id.clone(), |(n, _)| n.clone());
    let real = |id: &PkgId| ext.get(id).map_or_else(|| id.clone(), |(b, _)| b.clone());
    let label = |id: &PkgId, all: &BTreeMap<PkgId, Present>| match ext.get(id) {
        Some((_, e)) => e.label.clone(),
        None if back.contains_key(id) => {
            all.get(id).map_or_else(|| id.to_string(), |p| format!("{} (the base's, after the removal)", p.release.label()))
        }
        None => all.get(id).map_or_else(|| id.to_string(), |p| p.release.label()),
    };
    let removed: Vec<PkgId> = targets.iter().map(node_of).collect();
    let removed_labels = removed.iter().map(|id| label(id, &all)).collect();
    if no_deps {
        return Ok(RemovePlan { removed: targets.to_vec(), removed_labels, orphans: Vec::new() });
    }
    let gone: BTreeSet<&PkgId> = removed.iter().collect();
    // After the removal: what stays, and the base packages that come back with what goes (the
    // targets, then the orphans swept with them).
    let back_for = |going: &BTreeSet<PkgId>| -> BTreeSet<&PkgId> {
        back_with.iter().filter(|(_, r)| going.contains(*r)).map(|(n, _)| n).collect()
    };
    let after = |going: &BTreeSet<PkgId>| -> BTreeSet<&PkgId> {
        let comes_back = back_for(going);
        all.keys().filter(|id| !gone.contains(id) && (!back.contains_key(*id) || comes_back.contains(id))).collect()
    };
    let targets_set: BTreeSet<PkgId> = targets.iter().cloned().collect();
    let remaining: BTreeSet<&PkgId> = after(&targets_set);
    // Before the removal, the returning base packages are not there.
    let everything: BTreeSet<&PkgId> = all.keys().filter(|id| !back.contains_key(*id)).collect();
    let g = Graph { all: &all, rules, added_for, ext: &ext, back: &back };

    // Each dependent once per dependency: a claim of a base package that applies to a base thorn
    // and an added one is the base node's and the extension node's alike.
    let mut dependents: Vec<(String, String)> = Vec::new();
    let mut listed: BTreeSet<(PkgId, String)> = BTreeSet::new();
    for d in &remaining {
        let p = &all[*d];
        for (claim, before) in g.holders(p, &everything) {
            let was_held_by_gone = before.iter().any(|h| gone.contains(h));
            if was_held_by_gone
                && g.holders_of_label(p, &claim, &remaining).is_empty()
                && listed.insert((real(d), claim.clone()))
            {
                dependents.push((label(d, &all), claim));
            }
        }
    }
    if !dependents.is_empty() {
        return Err(Refusal::HasDependents { removed: targets.to_vec(), dependents });
    }

    let reached_before = g.reach(&everything, &BTreeSet::new());
    let stray: BTreeSet<PkgId> =
        remaining.iter().filter(|id| !reached_before.contains(**id)).map(|id| (*id).clone()).collect();
    // An orphan that replaced base repos brings them back too: they count, to a fixpoint.
    let mut going = targets_set.clone();
    let mut remaining = remaining;
    let mut reached_after = g.reach(&remaining, &stray);
    for _ in 0..8 {
        let orphaned: BTreeSet<PkgId> =
            remaining.iter().filter(|id| !reached_after.contains(**id)).map(|id| real(id)).collect();
        let next: BTreeSet<PkgId> = targets_set.union(&orphaned).cloned().collect();
        if next == going {
            break;
        }
        going = next;
        remaining = after(&going);
        reached_after = g.reach(&remaining, &stray);
    }
    let orphans = remaining
        .iter()
        .filter(|id| !back.contains_key(**id) && !reached_after.contains(**id))
        .map(|id| {
            let real = real(id);
            Orphan { id: real.clone(), label: label(id, &all), by: added_for.get(&real).map(|a| a.by.clone()).unwrap_or_default() }
        })
        .collect();
    Ok(RemovePlan { removed: targets.to_vec(), removed_labels, orphans })
}

struct Graph<'a> {
    all: &'a BTreeMap<PkgId, Present>,
    rules: &'a Rules,
    added_for: &'a BTreeMap<PkgId, AddedFor>,
    /// Extension node → (its base package, the extension).
    ext: &'a BTreeMap<PkgId, (PkgId, &'a Extension)>,
    /// Returning base package node → its package.
    back: &'a BTreeMap<PkgId, PkgId>,
}

impl Graph<'_> {
    /// Who in `set` takes a dependency on `target`: the target itself, a package an override
    /// says satisfies it, or a fork of it (identity only: the versions were checked when it was
    /// added). An extension node shares its base node's claims, but the base node, which sorts
    /// before it and never goes, is always found first.
    fn taker<'s>(&self, target: &str, set: &BTreeSet<&'s PkgId>) -> Option<&'s PkgId> {
        if let Some(id) = set.iter().find(|id| id.as_str() == target) {
            return Some(id);
        }
        if let Some(id) = set.iter().find(|id| self.back.get(**id).is_some_and(|b| b.as_str() == target)) {
            return Some(id);
        }
        let t = PkgId::parse(target).ok()?;
        for x in self.rules.satisfiers(&t) {
            if let Some(id) = set.iter().find(|id| **id == x) {
                return Some(id);
            }
        }
        set.iter().find(|id| forks(&self.all[**id].release, &t)).copied()
    }

    /// Whether `taker`, when it is a base package coming back, meets a dependency asking `range`
    /// or `sha` of it and `needs` of its thorns: its release fits, and the base enables them. Any
    /// other taker is taken as is (its versions were checked when it was added).
    fn back_fits(&self, taker: &PkgId, range: Option<&str>, sha: Option<&str>, needs: &[String]) -> bool {
        if !self.back.contains_key(taker) {
            return true;
        }
        let p = &self.all[taker];
        let fits = super::Req::from_claim(range, sha).is_ok_and(|q| q.fit(&p.release) != super::Fit::No);
        fits && needs.iter().all(|n| p.enabled.iter().any(|t| t.eq_ignore_ascii_case(n)))
    }

    /// The holder of a dependency of `dependent` that `taker` takes, needing `needs` of it: the
    /// taker, or its extension node when the dependency needs one of the extension's thorns;
    /// `None` when that node is not in `set`. A dependency naming no thorns (every thorn) holds
    /// the extension only for a dependent cactup added: a base package's claims never asked for
    /// more than the base it came with, which removing the extension gives back.
    fn holder<'s>(
        &self,
        dependent: &Present,
        taker: &'s PkgId,
        needs: &[String],
        set: &BTreeSet<&'s PkgId>,
    ) -> Option<&'s PkgId> {
        let Some((node, (_, e))) = self.ext.iter().find(|(_, (b, _))| b == taker) else { return Some(taker) };
        let every = needs.is_empty() && !matches!(dependent.origin, Origin::Base { .. });
        let touches = every || needs.iter().any(|n| e.thorns.iter().any(|t| t.eq_ignore_ascii_case(n)));
        if !touches {
            return Some(taker);
        }
        set.iter().find(|id| **id == node).copied()
    }

    /// For each dependency of `p` that applies to its enabled thorns (libraries aside), the
    /// packages in `set` holding it: the single target's taker, or for an any-of the alternative
    /// recorded as its choice, else the first one present.
    fn holders<'s>(&self, p: &Present, set: &BTreeSet<&'s PkgId>) -> Vec<(String, Vec<&'s PkgId>)> {
        let mut out = Vec::new();
        // An extension node carries only the dependencies of its own thorns:
        // one for every thorn is its package's, held from the base node.
        let extension = self.ext.contains_key(&p.release.id);
        for c in p.release.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_none()) {
            let enabled = |n: &String| p.enabled.iter().any(|t| t.eq_ignore_ascii_case(n));
            let applies = (c.needed_by.is_empty() && !extension) || c.needed_by.iter().any(enabled);
            if !applies {
                continue;
            }
            let label = claim_label(c);
            let held: Vec<&PkgId> = if c.alternatives.is_empty() {
                let taker = self.taker(&c.target, set).filter(|t| self.back_fits(t, c.range.as_deref(), c.sha.as_deref(), &c.needs));
                taker.and_then(|t| self.holder(p, t, &c.needs, set)).into_iter().collect()
            } else {
                let alternatives: Option<Vec<PkgId>> =
                    c.alternatives.iter().map(|a| PkgId::parse(&a.target).ok()).collect();
                // The dependent as recorded: an extension node's choices are its package's.
                let dependent = self.ext.get(&p.release.id).map_or_else(|| p.release.id.clone(), |(b, _)| b.clone());
                let key = alternatives.map(|alternatives| ChoiceKey { dependent, alternatives });
                let takers: Vec<(&PkgId, &Vec<String>)> = (c.alternatives.iter())
                    .filter_map(|a| {
                        let t = self.taker(&a.target, set)?;
                        self.back_fits(t, a.range.as_deref(), a.sha.as_deref(), &a.needs).then_some((t, &a.needs))
                    })
                    .collect();
                let recorded = takers.iter().find(|(id, _)| {
                    let real = self.ext.get(*id).map_or(*id, |(b, _)| b);
                    let choices = self.added_for.get(real).map(|a| a.choices.as_slice()).unwrap_or_default();
                    key.as_ref().is_some_and(|k| choices.contains(k))
                });
                let chosen = recorded.or(takers.first());
                chosen.and_then(|(t, needs)| self.holder(p, t, needs, set)).into_iter().collect()
            };
            out.push((label, held));
        }
        out
    }

    fn holders_of_label<'s>(
        &self,
        p: &Present,
        label: &str,
        set: &BTreeSet<&'s PkgId>,
    ) -> Vec<&'s PkgId> {
        self.holders(p, set).into_iter().filter(|(l, _)| l == label).flat_map(|(_, h)| h).collect()
    }

    /// The packages of `set` reachable from its roots (base repos, added packages the user named,
    /// and `extra`) through the dependencies they hold.
    fn reach(&self, set: &BTreeSet<&PkgId>, extra: &BTreeSet<PkgId>) -> BTreeSet<PkgId> {
        let mut reached: BTreeSet<PkgId> = BTreeSet::new();
        let mut queue: VecDeque<&PkgId> = VecDeque::new();
        for id in set {
            let p = &self.all[*id];
            let root = extra.contains(*id)
                || match (&p.origin, self.ext.get(*id)) {
                    (_, Some((base, e))) => is_requested(self.added_for.get(base), &e.commit),
                    (Origin::Base { .. }, None) => true,
                    (Origin::Added, None) => is_requested(self.added_for.get(*id), &p.release.commit),
                };
            if root && reached.insert((*id).clone()) {
                queue.push_back(id);
            }
        }
        while let Some(id) = queue.pop_front() {
            for (_, held) in self.holders(&self.all[id], set) {
                for h in held {
                    if reached.insert(h.clone()) {
                        queue.push_back(h);
                    }
                }
            }
        }
        reached
    }
}
