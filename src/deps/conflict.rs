//! The checks over the final set: conflicts between packages, and CCL requirements nothing meets.
// Plan §2.2 steps 6 and 7; decisions 5 and 7.

use std::collections::BTreeMap;

use cactup_ecosystem_common::ccl::ThornFacts;

use super::refusal::Refusal;
use super::resolve::Entry;
use super::{forks, Fit, PkgId, Release, Req, Requirement, Requirements};

/// Whether `a` declares itself incompatible with `b`'s release. A release at no version is taken
/// to match a range (it may be any version).
pub(super) fn incompatible(a: &Release, b: &Release) -> bool {
    a.claims
        .iter()
        .filter(|c| c.kind == "incompatible-with" && c.target == b.id.as_str())
        .map(|c| Req::from_claim(c.range.as_deref(), c.sha.as_deref()))
        .any(|q| q.map_or(true, |q| q.fit(b) != Fit::No))
}

/// The first conflict in the final set that involves something this command changes (brings
/// in, moves, or gives thorns; two unchanged present packages are the installation's business,
/// not this plan's), the most
/// specific kind first (a fork usually shares its upstream's thorn names):
/// - a fork and its upstream;
/// - an `incompatible-with`, whichever of the two declares it;
/// - two enabled thorns of one name (case-insensitive) in different packages, pseudo-packages
///   included.
pub(super) fn conflicts(chosen: &BTreeMap<PkgId, Entry>) -> Option<Refusal> {
    for (id, e) in chosen {
        for (up, eu) in chosen {
            if id != up && forks(&e.release, up) && (e.changed() || eu.changed()) {
                return Some(Refusal::ForkAndUpstream {
                    fork: id.clone(),
                    fork_label: e.release.label(),
                    upstream: up.clone(),
                    upstream_base_repo: eu.base_repo().map(str::to_owned),
                });
            }
        }
    }
    for (a, ea) in chosen {
        for (b, eb) in chosen {
            if a != b && (ea.changed() || eb.changed()) && incompatible(&ea.release, &eb.release) {
                return Some(Refusal::Incompatible {
                    by: a.clone(),
                    by_label: ea.release.label(),
                    target: b.clone(),
                    target_label: eb.release.label(),
                });
            }
        }
    }
    let mut by_name: BTreeMap<String, Vec<(&PkgId, &Entry, &str)>> = BTreeMap::new();
    for (id, e) in chosen {
        for t in &e.enabled {
            by_name.entry(t.to_ascii_lowercase()).or_default().push((id, e, t));
        }
    }
    for group in by_name.values() {
        for (i, (a, ea, ta)) in group.iter().enumerate() {
            for (b, eb, tb) in &group[i + 1..] {
                let (a_new, b_new) = (ea.thorn_is_new(ta), eb.thorn_is_new(tb));
                if !a_new && !b_new {
                    continue;
                }
                // The newcomer is the one changing; the other may be a base repo to replace.
                let (newcomer, other) = if b_new { (*b, *ea) } else { (*a, *eb) };
                let base_repo = other.base_repo().map(str::to_owned);
                return Some(Refusal::SameThorn {
                    thorn: ta.to_string(),
                    first: ea.release.label(),
                    second: eb.release.label(),
                    newcomer: newcomer.clone(),
                    base_repo,
                });
            }
        }
    }
    None
}

/// Every hard requirement of every thorn this command enables (a package brought in or moved:
/// all its enabled thorns; a present one: the thorns new to it) that no enabled thorn of the
/// final set meets: (thorn, its package, the requirement), in plan order.
pub(super) fn unmet_requirements(
    chosen: &BTreeMap<PkgId, Entry>,
    ccl: &dyn Requirements,
) -> Vec<(String, PkgId, Requirement)> {
    let enabled: Vec<(&str, &ThornFacts)> = chosen
        .values()
        .flat_map(|e| e.enabled.iter().filter_map(|t| e.release.thorn(t)))
        .map(|t| (t.thorn.as_str(), &t.facts))
        .collect();
    let mut entries: Vec<(&PkgId, &Entry)> = chosen.iter().collect();
    entries.sort_by(|(a, ea), (b, eb)| ea.order.cmp(&eb.order).then_with(|| a.cmp(b)));
    let mut out: Vec<(String, PkgId, Requirement)> = Vec::new();
    for (id, e) in entries {
        for name in e.enabled.iter().filter(|t| e.thorn_is_new(t)) {
            let Some(thorn) = e.release.thorn(name) else { continue };
            for r in ccl.of(&thorn.facts) {
                if !enabled.iter().any(|(n, f)| ccl.meets(n, f, &r)) {
                    out.push((thorn.thorn.clone(), id.clone(), r));
                }
            }
        }
    }
    out
}
