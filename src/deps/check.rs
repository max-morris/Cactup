//! The check of an installation as it is: the conflicts and unmet CCL requirements among the
//! packages in it, read-only, each problem one line ending in the way past.
// Plan §2.2 steps 6 and 7 over the whole installation, §4.4 (`claims check`).

use std::collections::BTreeMap;

use cactup_ecosystem_common::ccl::{self, ThornFacts};

use super::conflict::{incompatible, needs_of};
use super::{forks, Facts, Origin, PkgId, Present, Provider, Release, Requirement, Rules};
use crate::Res;

/// The problems among `present`, with `rules` applied to every release as the resolver loads it
/// (an override that drops a claim drops its problem), one line each, in a fixed order: forks
/// beside their upstream, incompatibilities, thorn names enabled twice, requirements that
/// nothing meets. `facts` lists the providers of an unmet requirement. Nothing is read or written.
pub fn check(present: &[Present], rules: &Rules, facts: &mut dyn Facts) -> Res<Vec<String>> {
    let mut ps: Vec<Present> = present.to_vec();
    for p in &mut ps {
        rules.apply(&mut p.release);
    }
    ps.sort_by(|a, b| a.release.id.cmp(&b.release.id));
    let mut out = Vec::new();

    let mut fork_pairs: Vec<(&PkgId, &PkgId)> = Vec::new();
    for a in &ps {
        for b in ps.iter().filter(|b| b.release.id != a.release.id) {
            if forks(&a.release, &b.release.id) {
                let (fork, upstream) = (&a.release.id, &b.release.id);
                fork_pairs.push((fork, upstream));
                out.push(format!(
                    "{} is a fork of {}, also installed. Run `cactup claims override drop {fork}:fork-of:{upstream}`.",
                    a.release.label(),
                    b.release.label()
                ));
            }
        }
    }
    for a in &ps {
        for b in ps.iter().filter(|b| b.release.id != a.release.id) {
            if incompatible(&a.release, &b.release) {
                let (by, target) = (&a.release.id, &b.release.id);
                out.push(format!(
                    "{} is incompatible with {}. Run `cactup claims override drop {by}:incompatible-with:{target}`.",
                    a.release.label(),
                    b.release.label()
                ));
            }
        }
    }

    // A thorn name enabled by two packages (a fork beside its upstream is said above).
    let mut by_name: BTreeMap<String, Vec<(&Present, &str)>> = BTreeMap::new();
    for p in &ps {
        for t in &p.enabled {
            by_name.entry(t.to_ascii_lowercase()).or_default().push((p, t));
        }
    }
    for holders in by_name.values().filter(|h| h.len() > 1) {
        let ids: Vec<&PkgId> = holders.iter().map(|(p, _)| &p.release.id).collect();
        let forked = |a: &PkgId, b: &PkgId| fork_pairs.iter().any(|(f, u)| (f, u) == (&a, &b) || (f, u) == (&b, &a));
        if ids.iter().enumerate().all(|(i, a)| ids[i + 1..].iter().all(|b| forked(a, b))) {
            continue;
        }
        let labels: Vec<String> = holders.iter().map(|(p, _)| p.release.label()).collect();
        let added: Vec<&PkgId> = holders.iter().filter(|(p, _)| p.origin == Origin::Added).map(|(p, _)| &p.release.id).collect();
        let way = match added.first() {
            Some(id) => format!("Run `cactup remove {id}`."),
            None => "Remove one from the thornlist.".to_owned(),
        };
        out.push(format!("The thorn {} is enabled by {}. {way}", holders[0].1, labels.join(" and ")));
    }

    // A requirement of an enabled thorn that no enabled thorn meets.
    let enabled: Vec<(&str, &ThornFacts)> = ps
        .iter()
        .flat_map(|p| p.enabled.iter().filter_map(|t| p.release.thorn(t)))
        .map(|t| (t.thorn.as_str(), &t.facts))
        .collect();
    let mut asked: BTreeMap<(u8, String), Vec<Provider>> = BTreeMap::new();
    for p in &ps {
        for name in &p.enabled {
            let Some(thorn) = p.release.thorn(name) else { continue };
            for need in needs_of(&thorn.facts) {
                if enabled.iter().any(|(n, f)| ccl::meets(n, f, &need)) {
                    continue;
                }
                let providers = match asked.get(&need.key()) {
                    Some(found) => found.clone(),
                    None => {
                        let found = facts.providers(&need)?;
                        asked.insert(need.key(), found.clone());
                        found
                    }
                };
                out.push(unmet_line(&thorn.thorn, &p.release, &need, &providers));
            }
        }
    }
    Ok(out)
}

/// `CarpetX (et/carpetx@1.0.0) inherits or befriends grid; no installed thorn provides it. …`
fn unmet_line(thorn: &str, release: &Release, need: &Requirement, providers: &[Provider]) -> String {
    let add = |p: &Provider| match p.version.contains(':') {
        true => format!("`cactup add {}/{}@{}`", p.package, p.thorn, p.version),
        false => format!("`cactup add {}/{}`", p.package, p.thorn),
    };
    let mut found: Vec<String> = providers.iter().map(add).collect();
    found.dedup();
    let way = match found.is_empty() {
        true => "No registered package provides it.".to_owned(),
        false => format!("Run {}.", found.join(" or ")),
    };
    format!("{thorn} ({}) {need}; no installed thorn provides it. {way}", release.label())
}
