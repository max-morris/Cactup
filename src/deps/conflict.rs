//! The checks over a set of packages in play: conflicts between them, and CCL requirements nothing
//! meets.
// Plan §2.2 steps 6 and 7; decisions 5 and 7.

use std::collections::BTreeMap;

use cactup_ecosystem_common::ccl::{self, ThornFacts};

use super::refusal::Refusal;
use super::resolve::Entry;
use super::{forks, Fit, PkgId, Release, Req, Requirement};

/// Whether `a` declares itself incompatible with `b`'s release. A release at no version is taken
/// to match a range (it may be any version).
pub(super) fn incompatible(a: &Release, b: &Release) -> bool {
    a.claims
        .iter()
        .filter(|c| c.kind == "incompatible-with" && c.target == b.id.as_str())
        .map(|c| Req::from_claim(c.range.as_deref(), c.sha.as_deref()))
        .any(|q| q.map_or(true, |q| q.fit(b) != Fit::No))
}

/// Which packages' thorns a thorn-name check counts: (package, thorn).
pub(super) type CountsThorn<'a> = &'a dyn Fn(&PkgId, &str) -> bool;

/// The first conflict among the packages `among` accepts (and the upstreams `upstream` accepts) that involves something this command
/// changes (brings in, moves, or gives thorns; two unchanged present packages are the
/// installation's business, not this plan's), with the two packages it is between. The most
/// specific kind comes first (a fork usually shares its upstream's thorn names):
/// - a fork and its upstream;
/// - an `incompatible-with`, whichever of the two declares it;
/// - two enabled thorns of one name (case-insensitive) in different packages, pseudo-packages
///   included (only when `same_thorn` is given, and among the thorns it accepts: a package's
///   thorn may count where the package's release does not).
pub(super) fn conflicts(
    chosen: &BTreeMap<PkgId, Entry>,
    among: &dyn Fn(&PkgId) -> bool,
    upstream: &dyn Fn(&PkgId) -> bool,
    same_thorn: Option<CountsThorn>,
) -> Option<(Refusal, Vec<PkgId>)> {
    let all: Vec<(&PkgId, &Entry)> = chosen.iter().collect();
    let entries: Vec<(&PkgId, &Entry)> = all.iter().copied().filter(|(id, _)| among(id)).collect();
    // Only a pair with something changing can conflict anew: each changing package against all.
    let changing: Vec<(&PkgId, &Entry)> = entries.iter().copied().filter(|(_, e)| e.changed()).collect();
    for (a, ea) in &changing {
        for (b, eb) in &all {
            for ((fork, ef), (up, eu)) in [((a, ea), (b, eb)), ((b, eb), (a, ea))] {
                let counts = among(fork) && (among(up) || upstream(up));
                if fork != up && counts && forks(&ef.release, up) {
                    let r = Refusal::ForkAndUpstream {
                        fork: (*fork).clone(),
                        fork_label: ef.release.label(),
                        upstream: (*up).clone(),
                        upstream_base_repo: eu.base_repo().map(str::to_owned),
                        // Checked by a search with it, when it is suggested.
                        replace: eu.base_repo().is_some(),
                    };
                    return Some((r, vec![(*fork).clone(), (*up).clone()]));
                }
            }
        }
    }
    for (a, ea) in &changing {
        for (b, eb) in &entries {
            for ((x, ex), (y, ey)) in [((a, ea), (b, eb)), ((b, eb), (a, ea))] {
                if x != y && incompatible(&ex.release, &ey.release) {
                    let r = Refusal::Incompatible {
                        by: (*x).clone(),
                        by_label: ex.release.label(),
                        target: (*y).clone(),
                        target_label: ey.release.label(),
                    };
                    return Some((r, vec![(*x).clone(), (*y).clone()]));
                }
            }
        }
    }
    let holds = same_thorn?;
    for (a, ea) in all.iter().filter(|(_, e)| e.changed()) {
        for ta in ea.enabled.iter().filter(|t| ea.thorn_is_new(t) && holds(a, t)) {
            for (b, eb) in &all {
                if a == b {
                    continue;
                }
                let Some(tb) = eb.enabled.iter().find(|t| t.eq_ignore_ascii_case(ta)) else { continue };
                if !holds(b, tb) {
                    continue;
                }
                // Reported with the two in id order; the newcomer is the one changing (the later
                // one when both are), and the other may be a base repo to replace.
                let ((first, ef, tf), (_, es, _)) =
                    if a < b { ((a, ea, ta), (b, eb, tb)) } else { ((b, eb, tb), (a, ea, ta)) };
                let b_new = eb.thorn_is_new(tb);
                let (newcomer, other) = if b_new && b > a { (*b, *ea) } else { (*a, *eb) };
                let r = Refusal::SameThorn {
                    thorn: tf.clone(),
                    first: ef.release.label(),
                    second: es.release.label(),
                    newcomer: newcomer.clone(),
                    base_repo: other.base_repo().map(str::to_owned),
                    ignore: None,
                    // Checked by a search with it, when it is suggested.
                    replace: other.base_repo().is_some(),
                };
                let _ = first;
                return Some((r, vec![(*a).clone(), (*b).clone()]));
            }
        }
    }
    None
}

/// A thorn's hard requirements, header includes left out: those are
/// feature/include-closure's, and this branch checks as before the crate produced them.
fn needs_of(facts: &ThornFacts) -> Vec<Requirement> {
    ccl::needs_of(facts).into_iter().filter(|r| !matches!(r, Requirement::Include(_))).collect()
}

/// The enabled thorns, with their facts.
fn enabled_facts(chosen: &BTreeMap<PkgId, Entry>) -> Vec<(&str, &ThornFacts)> {
    chosen
        .values()
        .flat_map(|e| e.enabled.iter().filter_map(|t| e.release.thorn(t)))
        .map(|t| (t.thorn.as_str(), &t.facts))
        .collect()
}

/// Every hard requirement of every thorn this command enables (a package brought in or moved:
/// all its enabled thorns; a present one: the thorns new to it) that no enabled thorn of the
/// set meets: (thorn, its package, the requirement), in plan order.
pub(super) fn unmet_requirements(
    chosen: &BTreeMap<PkgId, Entry>,
) -> Vec<(String, PkgId, Requirement)> {
    let enabled = enabled_facts(chosen);
    let mut entries: Vec<(&PkgId, &Entry)> = chosen.iter().collect();
    entries.sort_by(|(a, ea), (b, eb)| ea.order.cmp(&eb.order).then_with(|| a.cmp(b)));
    let mut out: Vec<(String, PkgId, Requirement)> = Vec::new();
    for (id, e) in entries {
        for name in e.enabled.iter().filter(|t| e.thorn_is_new(t)) {
            let Some(thorn) = e.release.thorn(name) else { continue };
            for r in needs_of(&thorn.facts) {
                if !enabled.iter().any(|(n, f)| ccl::meets(n, f, &r)) {
                    out.push((thorn.thorn.clone(), id.clone(), r));
                }
            }
        }
    }
    out
}

/// The requirements of thorns the installation keeps that `before` (the thorns it had) met and
/// the set no longer does: what replacing a base repo's checkouts, or the flesh, takes away.
pub(super) fn broken_requirements(
    chosen: &BTreeMap<PkgId, Entry>,
    before: &[(String, ThornFacts)],
) -> Vec<(String, PkgId, Requirement)> {
    let enabled = enabled_facts(chosen);
    let mut out = Vec::new();
    for (id, e) in chosen.iter().filter(|(_, e)| !e.is_new()) {
        for name in e.enabled.iter().filter(|t| !e.thorn_is_new(t)) {
            let Some(thorn) = e.release.thorn(name) else { continue };
            for r in needs_of(&thorn.facts) {
                let now = enabled.iter().any(|(n, f)| ccl::meets(n, f, &r));
                if !now && before.iter().any(|(n, f)| ccl::meets(n, f, &r)) {
                    out.push((thorn.thorn.clone(), id.clone(), r));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// A header a thorn includes is no requirement on this branch (feature/include-closure's);
    /// every other kind is as the crate gives it.
    #[test]
    fn includes_are_left_out() {
        let mut facts = cactup_ecosystem_common::ccl::ThornFacts::scan("IMPLEMENTS: z\nINHERITS: grid", None);
        facts.needs_includes = vec!["loop.hxx".into()];
        assert_eq!(cactup_ecosystem_common::ccl::needs_of(&facts).len(), 2);
        assert_eq!(super::needs_of(&facts), [super::Requirement::Implementation("grid".into())]);
    }
}
