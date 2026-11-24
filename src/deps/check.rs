//! The check of an installation as it is: the conflicts and unmet CCL requirements among the
//! packages in it, read-only, each problem one line ending in the way past.
// Plan §2.2 steps 6 and 7 over the whole installation, §4.4 (`claims check`).

use std::collections::BTreeMap;

use cactup_ecosystem_common::ccl::{self, ThornFacts};

use super::conflict::{incompatible, needs_of};
use super::{
    claim_target, fork_claims, forks, Facts, Fit, Origin, Override, PkgId, Present, Provider, Release, Req, Requirement,
    Rules,
};
use crate::Res;

/// What a line says when nothing provides a requirement.
pub const NO_PROVIDER: &str = "No registered package provides it.";

/// The problems among `present`, with `rules` applied to every release as the resolver loads it
/// (an override that drops a claim drops its problem), one line each, in a fixed order: forks
/// beside their upstream, incompatibilities, thorn names enabled twice, requirements that
/// nothing meets, dependencies not met by what is installed, overrides that no longer apply.
/// `facts` lists the providers of an unmet requirement and the versions of an upstream a fork
/// may stand in for. Nothing is written.
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
                    "{} is a fork of {}, also installed. Remove one of them, or run `cactup claims override drop {fork}:fork-of:{upstream}`.",
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
    dependencies(&ps, rules, facts, &mut out)?;
    for o in &rules.overrides {
        if let Override::Satisfies { by, .. } = o
            && by.is_repo()
            && !ps.iter().any(|p| &p.release.id == by)
        {
            let key = o.key();
            out.push(format!(
                "The override '{key}' names a repo that is not installed. Remove it with `cactup claims override --remove '{key}'`."
            ));
        }
    }
    Ok(out)
}

/// One way to meet a dependency: target, range, commit, thorns needed.
type Alternative<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [String]);

/// What stops an installed package's dependency from being met.
enum Unmet {
    NotInstalled,
    /// The installed release is outside the range.
    Outside(String),
    /// Whoever takes it (its label, its id) lacks these thorns enabled.
    Lacks(Box<(String, PkgId, Vec<String>)>),
}

/// The dependency claims of every installed package, each met by an installed release in range
/// (a package an override says stands in, or a fork standing in, takes it without a version check)
/// with the thorns it needs enabled; an any-of dependency by one of its alternatives.
fn dependencies(ps: &[Present], rules: &Rules, facts: &mut dyn Facts, out: &mut Vec<String>) -> Res<()> {
    let find = |id: &PkgId| ps.iter().find(|p| &p.release.id == id);
    let lacks = |p: &Present, needs: &[String]| -> Vec<String> {
        needs.iter().filter(|n| !p.enabled.iter().any(|e| e.eq_ignore_ascii_case(n))).cloned().collect()
    };
    for p in ps.iter().filter(|p| !p.stand_in) {
        let active = |needed_by: &[String]| {
            needed_by.is_empty() || needed_by.iter().any(|t| p.enabled.iter().any(|e| e.eq_ignore_ascii_case(t)))
        };
        for c in p.release.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_none() && active(&c.needed_by)) {
            let alternatives: Vec<Alternative> = match c.alternatives.is_empty() {
                true => vec![(&c.target, c.range.as_deref(), c.sha.as_deref(), &c.needs)],
                false => c.alternatives.iter().map(|a| (a.target.as_str(), a.range.as_deref(), a.sha.as_deref(), a.needs.as_slice())).collect(),
            };
            let mut why: Vec<(PkgId, Req, Unmet)> = Vec::new();
            let mut met = false;
            for (target, range, sha, needs) in alternatives {
                let Ok((target, req)) = claim_target(target, range, sha) else { met = true; break };
                let unmet: Option<Unmet>;
                if let Some(t) = find(&target) {
                    let missing = lacks(t, needs);
                    unmet = match (req.fit(&t.release), missing.is_empty()) {
                        (Fit::No, _) => Some(Unmet::Outside(t.release.label())),
                        (_, false) => Some(Unmet::Lacks(Box::new((t.release.label(), target.clone(), missing)))),
                        _ => None,
                    };
                } else if let Some(x) = rules.satisfiers(&target).find_map(find) {
                    let missing = lacks(x, needs);
                    unmet = (!missing.is_empty()).then(|| Unmet::Lacks(Box::new((x.release.label(), x.release.id.clone(), missing))));
                } else {
                    let forks_of: Vec<&Present> =
                        ps.iter().filter(|f| f.release.id != target && forks(&f.release, &target)).collect();
                    let mut found = Some(Unmet::NotInstalled);
                    if !forks_of.is_empty() {
                        facts.load(std::slice::from_ref(&target))?;
                        let published: Vec<_> = (facts.releases(&target).unwrap_or_default().iter())
                            .filter_map(|r| r.at.version().map(|v| (v.clone(), Some(r.commit.clone()))))
                            .collect();
                        let dep = target.slug(&req);
                        let standing = forks_of.into_iter().find(|f| {
                            fork_claims(&f.release, &target)
                                .iter()
                                .any(|k| dep.as_ref().is_some_and(|d| k.stands_in_for(d, &published)))
                        });
                        if let Some(f) = standing {
                            let missing = lacks(f, needs);
                            found = (!missing.is_empty()).then(|| Unmet::Lacks(Box::new((f.release.label(), f.release.id.clone(), missing))));
                        }
                    }
                    unmet = found;
                }
                match unmet {
                    None => {
                        met = true;
                        break;
                    }
                    Some(u) => why.push((target, req, u)),
                }
            }
            // A fork of the first target that is installed blocks adding the target itself.
            let fork = why.first().and_then(|(t, _, _)| ps.iter().find(|f| f.release.id != *t && forks(&f.release, t)));
            if !met && let Some(line) = dependency_line(p, c.alternatives.is_empty(), &why, fork) {
                out.push(line);
            }
        }
    }
    Ok(())
}

/// One line for a dependency that nothing met: what is wrong and the ways past.
fn dependency_line(p: &Present, single: bool, why: &[(PkgId, Req, Unmet)], fork: Option<&Present>) -> Option<String> {
    let (id, label) = (&p.release.id, p.release.label());
    let (first, req, unmet) = why.first()?;
    let drop = format!("`cactup claims override drop {id}:dependency:{first}`");
    if !single {
        let names: Vec<String> = why.iter().map(|(t, _, _)| t.to_string()).collect();
        return Some(format!(
            "{label} needs one of {}, and none is met. Run `cactup add {}`, or {drop}.",
            names.join(", "),
            names[0]
        ));
    }
    Some(match unmet {
        // The fork stands in for none of what is asked, and adding the target beside it is refused.
        Unmet::NotInstalled if fork.is_some() => {
            let fork = fork?;
            let id = &fork.release.id;
            let remove = match fork.origin {
                Origin::Added => format!(", `cactup remove {id}`"),
                Origin::Base { .. } => String::new(),
            };
            format!(
                "{label} needs {first} {req}, which is not installed; the fork {} does not stand in. \
                 Run `cactup claims override satisfies {id} {first}`{remove}, or {drop}.",
                fork.release.label()
            )
        }
        Unmet::NotInstalled => {
            format!("{label} needs {first} {req}, which is not installed. Run `cactup add {first}`, or {drop}.")
        }
        Unmet::Outside(installed) => format!(
            "{label} needs {first} {req}, and {installed} is installed. Run `cactup add --upgrade {first}`, or {drop}."
        ),
        Unmet::Lacks(lacking) => {
            let (taker, taker_id, thorns) = &**lacking;
            let list = thorns.join(", ");
            match taker_id.is_repo() {
                true => format!("{label} needs {list} of {taker}, not enabled. Run {drop}."),
                false => {
                    let adds: Vec<String> = thorns.iter().map(|t| format!("{taker_id}/{t}")).collect();
                    format!(
                        "{label} needs {list} of {taker}, not enabled. Run `cactup add {}`, or {drop}.",
                        adds.join(" ")
                    )
                }
            }
        }
    })
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
        true => NO_PROVIDER.to_owned(),
        false => format!("Run {}.", found.join(" or ")),
    };
    format!("{thorn} ({}) {need}; no installed thorn provides it. {way}", release.label())
}
