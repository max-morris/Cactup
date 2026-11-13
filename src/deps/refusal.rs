//! Every way resolution refuses, each with its way past.
// Plan §4.5. The text is user-visible: no spec refs in any string here, and no command that does
// not exist yet (`cactup claims override …` comes with P4; each place it belongs says so).

use std::fmt;

use super::{PkgId, Provider, Requirement};

/// A live constraint, as a refusal lists it: what it asks and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    /// The package it constrains.
    pub target: PkgId,
    /// `^2026.5`, `#0123456789ab`.
    pub req: String,
    /// `max/z4c@1.2.0`, `the command line`, `installed max/a@1.0.0`, `--upgrade-all`.
    pub source: String,
    /// The `--ignore-claim` pattern that drops it, when a claim makes it.
    pub ignore: Option<String>,
}

impl fmt::Display for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} (from {})", self.target, self.req, self.source)
    }
}

/// An unmet CCL requirement of a thorn the plan enables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmet {
    pub thorn: String,
    /// The package whose manifest leaves the requirement undeclared.
    pub package: PkgId,
    pub requirement: Requirement,
    pub providers: Vec<Provider>,
}

/// Why resolution stopped. Each one's text ends with what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A present package that may not move is outside a new constraint.
    PresentOutsideRange { installed: String, live: Live },
    /// A present package that may not move lacks thorns a dependency needs.
    PresentLacksThorns { installed: String, thorns: Vec<String>, live: Live },
    /// Two packages enable a thorn of one name.
    SameThorn {
        thorn: String,
        first: String,
        second: String,
        newcomer: PkgId,
        base_repo: Option<String>,
    },
    /// `by` declares itself incompatible with `target`.
    Incompatible { by: PkgId, by_label: String, target: PkgId, target_label: String },
    /// A fork and its upstream are both in the plan.
    ForkAndUpstream { fork: PkgId, fork_label: String, upstream: PkgId, upstream_base_repo: Option<String> },
    /// A chosen fork of a dependency's target does not stand in for the versions it asks.
    ForkDoesNotStandIn { fork: PkgId, fork_label: String, live: Live },
    /// No release meets every constraint on a package.
    NoReleaseFits { target: PkgId, constraints: Vec<Live> },
    /// No flesh release meets every flesh constraint.
    FleshUnmet { target: PkgId, constraints: Vec<Live> },
    /// The plan would need two fleshes.
    TwoFleshes { first: String, second: String },
    /// The constraints kept displacing each other past the step budget.
    NoConsistentSet { constraints: Vec<Live> },
    /// Whoever takes a need lacks thorns it names.
    MissingThorns { taker: String, thorns: Vec<String>, live: Live },
    /// Under `--replace`, a package taking over a base repo whose package it upgrades or forks
    /// lacks thorns the installation has enabled from that repo.
    ReplacementLacksThorns { by: PkgId, by_label: String, base_repo: String, thorns: Vec<String> },
    /// Two packages place one library at versions no single release meets, or a new placement
    /// does not accept the release an installed one holds.
    LibraryClash { library: PkgId, placements: Vec<Live> },
    /// Hard CCL requirements that nothing in the final set meets.
    Unclaimed(Vec<Unmet>),
    /// A library was named; it comes with the package that builds against it.
    NamedLibrary { target: PkgId },
    /// The Cactarium has no such package.
    NotRegistered { target: PkgId, by: Option<Live> },
    /// A claim cactup cannot read.
    BadClaim { by: String, claim: String, detail: String, ignore: String },
    /// Removing these would leave dependents without what they need.
    HasDependents { removed: Vec<PkgId>, dependents: Vec<(String, String)> },
}

impl Refusal {
    /// What gets past it.
    pub fn way_past(&self) -> String {
        match self {
            Refusal::PresentOutsideRange { live, .. } | Refusal::PresentLacksThorns { live, .. } => {
                match &live.ignore {
                    Some(ignore) => format!("Pass --upgrade to move it, or --ignore-claim {ignore}"),
                    None => "Pass --upgrade to move it".to_owned(),
                }
            }
            // P4: or `cactup claims override satisfies repo:<repo> <newcomer>`.
            Refusal::SameThorn { newcomer, base_repo: Some(repo), .. } => {
                format!("Pass --replace to use {newcomer} in place of base repo {repo}")
            }
            Refusal::SameThorn { .. } => {
                "Install only one of them: name single thorns (owner/pkg/Thorn) to leave the other's out, \
                 or drop the claim that brings it in with --ignore-claim"
                    .to_owned()
            }
            // P4: or `cactup claims override drop <by>:incompatible-with:<target>` (for good).
            Refusal::Incompatible { by, target, .. } => {
                format!("Pass --ignore-claim {by}:incompatible-with:{target}")
            }
            Refusal::ForkAndUpstream { fork, upstream, upstream_base_repo: Some(repo), .. } => format!(
                "Pass --replace to use {fork} in place of base repo {repo}, or --ignore-claim {fork}:fork-of:{upstream}"
            ),
            Refusal::ForkAndUpstream { fork, upstream, .. } => {
                format!("Remove one of them, or pass --ignore-claim {fork}:fork-of:{upstream}")
            }
            // P4: or `cactup claims override satisfies <fork> <target>`.
            Refusal::ForkDoesNotStandIn { live, .. } => match &live.ignore {
                Some(ignore) => format!("Pass --ignore-claim {ignore}"),
                None => "Name a version the fork stands in for".to_owned(),
            },
            Refusal::NoReleaseFits { constraints, .. }
            | Refusal::FleshUnmet { constraints, .. }
            | Refusal::NoConsistentSet { constraints }
            | Refusal::LibraryClash { placements: constraints, .. } => ignore_one(constraints),
            Refusal::TwoFleshes { .. } => {
                "Drop one of the dependencies on a flesh with --ignore-claim, or name one flesh".to_owned()
            }
            // P4: or `cactup claims override needs …` to correct the claim for good.
            Refusal::MissingThorns { live, .. } => match &live.ignore {
                Some(ignore) => format!("Pass --ignore-claim {ignore}"),
                None => "Name thorns the package has".to_owned(),
            },
            Refusal::ReplacementLacksThorns { .. } => {
                "Leave out --replace, or install a release that has those thorns".to_owned()
            }
            Refusal::Unclaimed(unmet) => {
                let mut s =
                    String::from("Cactup adds only what a manifest declares. Add a provider yourself first");
                let lines: Vec<String> = unmet
                    .iter()
                    .flat_map(|u| u.providers.iter())
                    .map(|p| format!("  cactup add {}/{}", p.package, p.thorn))
                    .collect();
                if lines.is_empty() {
                    s.push_str(", or ask the package's authors to declare the dependency");
                } else {
                    s.push_str(":\n");
                    s.push_str(&dedup(lines).join("\n"));
                }
                s
            }
            Refusal::NamedLibrary { .. } => {
                "Add the package that builds against it, and the library comes with it".to_owned()
            }
            Refusal::NotRegistered { by: Some(live), .. } => match &live.ignore {
                Some(ignore) => format!("Pass --ignore-claim {ignore}"),
                None => "Check the name".to_owned(),
            },
            Refusal::NotRegistered { by: None, .. } => "Check the name".to_owned(),
            Refusal::BadClaim { ignore, .. } => {
                format!("Pass --ignore-claim {ignore} (and tell the package's authors)")
            }
            Refusal::HasDependents { .. } => {
                "Pass --no-deps to remove only what was named, leaving its dependents without it".to_owned()
            }
        }
    }
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    v.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

/// "Pass --ignore-claim <one of the claims>", for refusals that list several constraints.
fn ignore_one(constraints: &[Live]) -> String {
    let ignores = dedup(constraints.iter().filter_map(|c| c.ignore.clone()).collect());
    match ignores.as_slice() {
        [] => "Name another version".to_owned(),
        [one] => format!("Pass --ignore-claim {one}"),
        many => format!("Pass --ignore-claim with the claim to drop: {}", many.join(", ")),
    }
}

fn list(constraints: &[Live]) -> String {
    constraints.iter().map(|c| format!("\n  {c}")).collect()
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::PresentOutsideRange { installed, live } => write!(
                f,
                "The installation has {installed}, outside {} which {} asks",
                live.req, live.source
            )?,
            Refusal::PresentLacksThorns { installed, thorns, live } => write!(
                f,
                "The installation has {installed}, which has no {}, which {} needs",
                thorns.join(", "),
                live.source
            )?,
            Refusal::SameThorn { thorn, first, second, .. } => {
                write!(f, "{first} and {second} both have a thorn {thorn}")?
            }
            Refusal::Incompatible { by_label, target_label, .. } => {
                write!(f, "{by_label} says it is incompatible with {target_label}")?
            }
            Refusal::ForkAndUpstream { fork_label, upstream, .. } => write!(
                f,
                "{fork_label} is a fork of {upstream}, and a fork is never installed beside its upstream"
            )?,
            Refusal::ForkDoesNotStandIn { fork_label, live, .. } => write!(
                f,
                "{} needs {} {}, and {fork_label}, a fork of it, does not stand in for those versions",
                live.source, live.target, live.req
            )?,
            Refusal::NoReleaseFits { target, constraints } => {
                write!(f, "No release of {target} meets every constraint on it:{}", list(constraints))?
            }
            Refusal::FleshUnmet { target, constraints } => write!(
                f,
                "The installation's flesh does not meet a flesh dependency, and no release of {target} does:{}",
                list(constraints)
            )?,
            Refusal::TwoFleshes { first, second } => {
                write!(f, "The plan needs two fleshes, {first} and {second}, and an installation has one")?
            }
            Refusal::NoConsistentSet { constraints } => write!(
                f,
                "No consistent set of releases: these constraints keep displacing each other:{}",
                list(constraints)
            )?,
            Refusal::MissingThorns { taker, thorns, live } => {
                write!(f, "{taker} has no {}, which {} needs", thorns.join(", "), live.source)?
            }
            Refusal::ReplacementLacksThorns { by_label, base_repo, thorns, .. } => write!(
                f,
                "{by_label} would take the place of base repo {base_repo}, but has no {}, which the \
                 installation has enabled from it",
                thorns.join(", ")
            )?,
            Refusal::LibraryClash { library, placements } => write!(
                f,
                "No release of the library {library} meets every package that places it:{}",
                list(placements)
            )?,
            Refusal::Unclaimed(unmet) => {
                write!(f, "Requirements nothing in the installation meets, which no manifest declares:")?;
                for u in unmet {
                    write!(f, "\n  {} ({}) {}", u.thorn, u.package, u.requirement)?;
                    if u.providers.is_empty() {
                        write!(f, "; no registered package provides it")?;
                    }
                }
            }
            Refusal::NamedLibrary { target } => write!(
                f,
                "{target} is a library, which has no thorns: it is checked out where a package that builds against \
                 it says (the `into` of that package's dependency on it)"
            )?,
            Refusal::NotRegistered { target, by: Some(live) } => {
                write!(f, "{} needs {target}, which the Cactarium does not know", live.source)?
            }
            Refusal::NotRegistered { target, by: None } => write!(f, "The Cactarium does not know {target}")?,
            Refusal::BadClaim { by, claim, detail, .. } => {
                write!(f, "{by} has a claim cactup cannot read ({claim}): {detail}")?
            }
            Refusal::HasDependents { removed, dependents } => {
                let names: Vec<String> = removed.iter().map(ToString::to_string).collect();
                write!(f, "Other packages depend on {}:", names.join(", "))?;
                for (dependent, claim) in dependents {
                    write!(f, "\n  {dependent} ({claim})")?;
                }
            }
        }
        write!(f, ".\n{}", self.way_past())
    }
}

impl std::error::Error for Refusal {}
