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

    pub fn render(&self, command: &str) -> String {
        self.lines(command).into_iter().map(|(_, l)| l + "\n").collect()
    }
}

/// Plans removing `targets`: refuses while a remaining package depends on one of them (unless
/// `no_deps`), and lists the added packages, never named, that only the removed ones needed
/// (to a fixpoint, cycles included). A package nothing reached before the removal (one an
/// earlier `--no-deps` left behind) counts as named: it is never swept, nor what it needs.
pub fn plan_remove(
    present: &[Present],
    added_for: &BTreeMap<PkgId, AddedFor>,
    targets: &[PkgId],
    rules: &Rules,
    no_deps: bool,
) -> Result<RemovePlan, Refusal> {
    let removed: Vec<PkgId> = targets.to_vec();
    let mut all: BTreeMap<PkgId, Present> = BTreeMap::new();
    for p in present {
        let mut p = p.clone();
        rules.apply(&mut p.release);
        all.insert(p.release.id.clone(), p);
    }
    let removed_labels =
        removed.iter().map(|id| all.get(id).map_or_else(|| id.to_string(), |p| p.release.label())).collect();
    if no_deps {
        return Ok(RemovePlan { removed, removed_labels, orphans: Vec::new() });
    }
    let gone: BTreeSet<&PkgId> = removed.iter().collect();
    let remaining: BTreeSet<&PkgId> = all.keys().filter(|id| !gone.contains(id)).collect();
    let everything: BTreeSet<&PkgId> = all.keys().collect();
    let g = Graph { all: &all, rules, added_for };

    let mut dependents: Vec<(String, String)> = Vec::new();
    for d in &remaining {
        let p = &all[*d];
        for (label, before) in g.holders(p, &everything) {
            let was_held_by_gone = before.iter().any(|h| gone.contains(h));
            if was_held_by_gone && g.holders_of_label(p, &label, &remaining).is_empty() {
                dependents.push((p.release.label(), label));
            }
        }
    }
    if !dependents.is_empty() {
        return Err(Refusal::HasDependents { removed, dependents });
    }

    let reached_before = g.reach(&everything, &BTreeSet::new());
    let stray: BTreeSet<PkgId> =
        remaining.iter().filter(|id| !reached_before.contains(**id)).map(|id| (*id).clone()).collect();
    let reached_after = g.reach(&remaining, &stray);
    let orphans = remaining
        .iter()
        .filter(|id| !reached_after.contains(**id))
        .map(|id| Orphan {
            id: (*id).clone(),
            label: all[*id].release.label(),
            by: added_for.get(*id).map(|a| a.by.clone()).unwrap_or_default(),
        })
        .collect();
    Ok(RemovePlan { removed, removed_labels, orphans })
}

struct Graph<'a> {
    all: &'a BTreeMap<PkgId, Present>,
    rules: &'a Rules,
    added_for: &'a BTreeMap<PkgId, AddedFor>,
}

impl Graph<'_> {
    /// Who in `set` takes a dependency on `target`: the target itself, a package an override
    /// says satisfies it, or a fork of it (identity only: the versions were checked when it was
    /// added).
    fn taker<'s>(&self, target: &str, set: &BTreeSet<&'s PkgId>) -> Option<&'s PkgId> {
        if let Some(id) = set.iter().find(|id| id.as_str() == target) {
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

    /// For each dependency of `p` that applies to its enabled thorns (libraries aside), the
    /// packages in `set` holding it: the single target's taker, or for an any-of the alternative
    /// recorded as its choice, else the first one present.
    fn holders<'s>(&self, p: &Present, set: &BTreeSet<&'s PkgId>) -> Vec<(String, Vec<&'s PkgId>)> {
        let mut out = Vec::new();
        for c in p.release.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_none()) {
            let enabled = |n: &String| p.enabled.iter().any(|t| t.eq_ignore_ascii_case(n));
            let applies = c.needed_by.is_empty() || c.needed_by.iter().any(enabled);
            if !applies {
                continue;
            }
            let label = claim_label(c);
            let held: Vec<&PkgId> = if c.alternatives.is_empty() {
                self.taker(&c.target, set).into_iter().collect()
            } else {
                let alternatives: Option<Vec<PkgId>> =
                    c.alternatives.iter().map(|a| PkgId::parse(&a.target).ok()).collect();
                let dependent = p.release.id.clone();
                let key = alternatives.map(|alternatives| ChoiceKey { dependent, alternatives });
                let takers: Vec<&PkgId> =
                    c.alternatives.iter().filter_map(|a| self.taker(&a.target, set)).collect();
                let recorded = takers.iter().find(|id| {
                    let choices = self.added_for.get(**id).map(|a| a.choices.as_slice()).unwrap_or_default();
                    key.as_ref().is_some_and(|k| choices.contains(k))
                });
                recorded.or(takers.first()).copied().into_iter().collect()
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
                || match p.origin {
                    Origin::Base { .. } => true,
                    Origin::Added => is_requested(self.added_for.get(*id), &p.release.commit),
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
