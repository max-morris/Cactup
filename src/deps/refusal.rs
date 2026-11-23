//! Every way resolution refuses, each with its way past.
// Plan §4.5. The text is user-visible: no spec refs in any string here, and no command that does
// not exist yet (`cactup claims override …` comes with P4; each place it belongs says so).

use std::fmt;

use super::{PkgId, Provider, Requirement, Unanswered, UnreadKind};

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

/// The flags a refusal may suggest: each one only when a search with it (on the facts already
/// read) found a plan, so a way past is never a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ways {
    pub upgrade: bool,
    pub replace: bool,
    pub no_deps: bool,
}

/// What a branch of the search needed of a present package the Cactarium did not read at its
/// commit (its thorns are known from the tree; what they depend on, and what it forks, are not).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnreadNeed {
    pub id: PkgId,
    /// As installed: `et/admbase#abababababab`.
    pub package: String,
    /// What needed it: `max/zza@2.0.0`, `the command line`.
    pub by: String,
    pub what: UnreadFor,
    pub why: Unanswered,
}

/// What an [`UnreadNeed`] was for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnreadFor {
    /// Thorns of it not enabled now: what they depend on.
    Thorns(Vec<String>),
    /// Every thorn of it.
    Every,
    /// A package coming in has a thorn of the same name as one it has enabled: whether it is a
    /// fork of that package (it would stand in for it) or not (the two clash).
    ForkCheck { other: PkgId, thorn: String },
}

impl UnreadNeed {
    /// What it needed: `StaticConformal of et/admbase#abababababab`.
    fn wanted(&self) -> String {
        let UnreadNeed { id, package, what, .. } = self;
        match what {
            UnreadFor::Thorns(thorns) => format!("{} of {package}", thorns.join(", ")),
            UnreadFor::Every => format!("every thorn of {package}"),
            UnreadFor::ForkCheck { other, thorn } => format!(
                "{other}, which shares the thorn {thorn} with {package} (only a read of {id} tells whether it \
                 is a fork of {other})"
            ),
        }
    }

    /// The note a plan carries for a release it was not considered: `skipped` (or keeping the
    /// package where it is, `keeping`), with everything it needs that the Cactarium did not read.
    pub(super) fn note(skipped: &str, keeping: bool, needs: &[UnreadNeed]) -> String {
        // In the order of what is needed (thorn names), so that it does not follow package ids.
        let mut needs: Vec<&UnreadNeed> = needs.iter().collect();
        needs.sort_by(|a, b| (&a.what, &a.by, &a.id).cmp(&(&b.what, &b.by, &b.id)));
        let parts: Vec<String> = (needs.iter())
            .map(|n| {
                let (wanted, id, why) = (n.wanted(), &n.id, &n.why.why);
                if keeping {
                    format!("{} needs {wanted}, and the Cactarium did not read {id} there: {why}", n.by)
                } else {
                    format!("it needs {wanted}, and the Cactarium did not read {id} there: {why}")
                }
            })
            .collect();
        let what = if keeping { format!("keeping {skipped}") } else { skipped.to_owned() };
        format!("{what} was not considered: {}", parts.join("; "))
    }
}

impl fmt::Display for UnreadNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} needs {}; the Cactarium did not read {} there: {}", self.by, self.wanted(), self.id, self.why.why)
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
    /// The `--ignore-claim` patterns that dropped dependencies of `package` (one may have
    /// declared it).
    pub ignored: Vec<String>,
}

/// Why resolution stopped. Each one's text ends with what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A present package that may not move is outside a new constraint. `upgrade`: `--upgrade`
    /// finds a plan.
    PresentOutsideRange { installed: String, live: Live, upgrade: bool },
    /// A present package that may not move lacks thorns a dependency needs.
    PresentLacksThorns { installed: String, thorns: Vec<String>, live: Live, upgrade: bool },
    /// Two packages enable a thorn of one name.
    SameThorn {
        thorn: String,
        first: String,
        second: String,
        newcomer: PkgId,
        base_repo: Option<String>,
        /// The claim that brings the newcomer in, when one does.
        ignore: Option<String>,
        /// `--replace` finds a plan.
        replace: bool,
    },
    /// `by` declares itself incompatible with `target`.
    Incompatible { by: PkgId, by_label: String, target: PkgId, target_label: String },
    /// A fork and its upstream are both in the plan.
    /// `replace`: `--replace` finds a plan.
    ForkAndUpstream {
        fork: PkgId,
        fork_label: String,
        upstream: PkgId,
        upstream_base_repo: Option<String>,
        replace: bool,
    },
    /// A chosen fork of a dependency's target does not stand in for the versions it asks.
    ForkDoesNotStandIn { fork: PkgId, fork_label: String, live: Live },
    /// No release meets every constraint on a package.
    NoReleaseFits { target: PkgId, constraints: Vec<Live> },
    /// No flesh release meets every flesh constraint.
    FleshUnmet { target: PkgId, constraints: Vec<Live> },
    /// The plan would need two fleshes. `ignore`: the claim that brings the second, when one does.
    TwoFleshes { first: String, second: String, ignore: Option<String> },
    /// No combination of releases meets these together (or the search gave up before finding one).
    NoConsistentSet { constraints: Vec<Live> },
    /// The search ran past its wall-time safety net; `best` is the closest it came.
    GaveUp { seconds: u64, best: Box<Refusal> },
    /// A commit or tip the Cactarium cannot read.
    NotAvailable { target: PkgId, at: String, by: Option<Live>, detail: String },
    /// Whoever takes a need lacks thorns it names.
    MissingThorns { taker: String, thorns: Vec<String>, live: Live },
    /// Under `--replace`, a package taking over a base repo whose package it upgrades or forks
    /// lacks thorns the installation has enabled from that repo.
    ReplacementLacksThorns {
        by: PkgId,
        by_label: String,
        upstream: PkgId,
        base_repo: String,
        thorns: Vec<String>,
    },
    /// Two packages place one library at versions no single release meets, or a new placement
    /// does not accept the release an installed one holds.
    LibraryClash { library: PkgId, placements: Vec<Live> },
    /// Hard CCL requirements that nothing in the final set meets.
    Unclaimed(Vec<Unmet>),
    /// A library was named; it comes with the package that builds against it.
    NamedLibrary { target: PkgId },
    /// A dependency places `target` at `into` as a library, but it is a
    /// package with thorns.
    NotALibrary { target: PkgId, by: String, into: String, ignore: Option<String> },
    /// The Cactarium has no such package.
    NotRegistered { target: PkgId, by: Option<Live> },
    /// A claim cactup cannot read.
    BadClaim { by: String, claim: String, detail: String, ignore: String },
    /// No plan, and some branch failed for want of the claims of present packages the Cactarium
    /// did not read at their commits: what needed which, and why it was not read; with the ways
    /// past that find a plan.
    Unreadable { needs: Vec<UnreadNeed>, ways: Ways },
    /// Removing these would leave dependents without what they need.
    HasDependents { removed: Vec<PkgId>, dependents: Vec<(String, String)> },
}

impl Refusal {
    /// What gets past it.
    pub fn way_past(&self) -> String {
        match self {
            Refusal::PresentOutsideRange { live, upgrade, .. }
            | Refusal::PresentLacksThorns { live, upgrade, .. } => match (upgrade, &live.ignore) {
                (true, Some(ignore)) => format!("Pass --upgrade to move it, or --ignore-claim {ignore}"),
                (true, None) => "Pass --upgrade to move it".to_owned(),
                (false, Some(ignore)) => format!("Pass --ignore-claim {ignore}"),
                (false, None) => "Change what asks for it; --upgrade finds no plan either".to_owned(),
            },
            // P4: or `cactup claims override satisfies local:<repo> <newcomer>`.
            Refusal::SameThorn { newcomer, base_repo: Some(repo), replace: true, .. } => {
                format!("Pass --replace to use {newcomer} in place of base repo {repo}")
            }
            Refusal::SameThorn { ignore: Some(ignore), .. } => format!(
                "Name single thorns (owner/pkg/Thorn) to leave one of them out, or pass --ignore-claim {ignore}"
            ),
            Refusal::SameThorn { .. } => {
                "Name single thorns (owner/pkg/Thorn) to leave one of them out".to_owned()
            }
            // P4: or `cactup claims override drop <by>:incompatible-with:<target>` (for good).
            Refusal::Incompatible { by, target, .. } => {
                format!("Pass --ignore-claim {by}:incompatible-with:{target}")
            }
            Refusal::ForkAndUpstream { fork, upstream, upstream_base_repo: Some(repo), replace: true, .. } => format!(
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
            Refusal::TwoFleshes { ignore: Some(ignore), .. } => format!("Pass --ignore-claim {ignore}"),
            Refusal::TwoFleshes { .. } => "Name one flesh".to_owned(),
            // P4: or `cactup claims override needs …` to correct the claim for good.
            Refusal::MissingThorns { live, .. } => match &live.ignore {
                Some(ignore) => format!("Pass --ignore-claim {ignore}"),
                None => "Name thorns the package has".to_owned(),
            },
            Refusal::ReplacementLacksThorns { by, upstream, .. } => {
                format!("Pass --ignore-claim {by}:fork-of:{upstream} to install it as a package of its own")
            }
            Refusal::Unclaimed(unmet) => {
                let mut s =
                    String::from("Add a provider first");
                // A provider on another track than the default is named at
                // its version there, or `cactup add` would look on the
                // default track.
                let lines: Vec<String> = unmet
                    .iter()
                    .flat_map(|u| u.providers.iter())
                    .map(|p| match p.version.contains(':') {
                        true => format!("  cactup add {}/{}@{}", p.package, p.thorn, p.version),
                        false => format!("  cactup add {}/{}", p.package, p.thorn),
                    })
                    .collect();
                if lines.is_empty() {
                    s.push_str(", or ask the package authors to declare the dependency");
                } else {
                    s.push_str(":\n");
                    s.push_str(&dedup(lines).join("\n"));
                }
                s
            }
            Refusal::NamedLibrary { .. } => {
                "Add a package that builds against it".to_owned()
            }
            Refusal::NotALibrary { ignore: Some(ignore), .. } => {
                format!("Pass --ignore-claim {ignore} (and tell the package's authors)")
            }
            Refusal::NotALibrary { ignore: None, .. } => "Tell the package's authors".to_owned(),
            Refusal::NotRegistered { by: Some(live), .. } => match &live.ignore {
                Some(ignore) => format!("Pass --ignore-claim {ignore}"),
                None => "Check the name".to_owned(),
            },
            Refusal::NotRegistered { by: None, .. } => "Check the name".to_owned(),
            Refusal::NotAvailable { by: Some(Live { ignore: Some(ignore), .. }), .. } => {
                format!("Pass --ignore-claim {ignore}")
            }
            Refusal::NotAvailable { .. } => "Name a commit or version the Cactarium has".to_owned(),
            Refusal::GaveUp { best, .. } => best.way_past(),
            Refusal::BadClaim { ignore, .. } => {
                format!("Pass --ignore-claim {ignore} (and tell the package's authors)")
            }
            Refusal::Unreadable { needs, ways } => {
                let mut out: Vec<String> = Vec::new();
                if ways.upgrade {
                    out.push("pass --upgrade to move it to a release".to_owned());
                }
                if ways.replace {
                    out.push("pass --replace to replace its base repo with the new package".to_owned());
                }
                if ways.no_deps {
                    out.push("pass --no-deps to add only the packages named".to_owned());
                }
                if out.is_empty() {
                    // Nothing cactup can do here finds a plan: what would.
                    let mut causes: Vec<(UnreadKind, &PkgId)> = needs.iter().map(|n| (n.why.kind, &n.id)).collect();
                    causes.sort();
                    causes.dedup();
                    out = (causes.into_iter())
                        .map(|(kind, id)| match kind {
                            UnreadKind::Retry => "try again later".to_owned(),
                            UnreadKind::Missing => format!("push the current commit of {id}, then try again"),
                            UnreadKind::Untrusted => format!("put {id} at a released version"),
                            UnreadKind::Invalid => {
                                format!("put {id} at a commit readable by the Cactarium, such as a released version")
                            }
                        })
                        .collect();
                    out.dedup();
                    let mut s = out.join("; ");
                    s[..1].make_ascii_uppercase();
                    return s;
                }
                let mut s = out.join(", or ");
                s[..1].make_ascii_uppercase();
                s
            }
            Refusal::HasDependents { .. } => {
                "Pass --no-deps to remove them anyway, leaving the dependents without them".to_owned()
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
        many => format!("Pass --ignore-claim with one of: {}", many.join(", ")),
    }
}

fn list(constraints: &[Live]) -> String {
    constraints.iter().map(|c| format!("\n  {c}")).collect()
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Refusal::GaveUp { seconds, best } = self {
            // The closest refusal carries its own way past.
            return write!(f, "No plan found in {seconds} s. The closest one:\n{best}");
        }
        match self {
            Refusal::GaveUp { .. } => unreachable!("written above"),
            Refusal::PresentOutsideRange { installed, live, .. } => write!(
                f,
                "The installation has {installed}, outside {} (asked by {})",
                live.req, live.source
            )?,
            Refusal::PresentLacksThorns { installed, thorns, live, .. } => write!(
                f,
                "The installation has {installed}, without {} (needed by {})",
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
                "{fork_label} is a fork of {upstream}, and cannot be installed beside it"
            )?,
            Refusal::ForkDoesNotStandIn { fork_label, live, .. } => write!(
                f,
                "{} needs {} {}, and the fork {fork_label} does not stand in for those versions",
                live.source, live.target, live.req
            )?,
            Refusal::NoReleaseFits { target, constraints } => {
                write!(f, "No release of {target} meets every constraint on it:{}", list(constraints))?
            }
            Refusal::FleshUnmet { target, constraints } => write!(
                f,
                "Neither the installation's flesh nor any release of {target} meets these:{}",
                list(constraints)
            )?,
            Refusal::TwoFleshes { first, second, .. } => {
                write!(f, "The plan needs two fleshes, {first} and {second}; an installation has one")?
            }
            Refusal::NoConsistentSet { constraints } => write!(
                f,
                "No combination of releases meets these constraints together:{}",
                list(constraints)
            )?,
            Refusal::NotAvailable { target, at, by, detail } => {
                match by {
                    Some(live) => write!(f, "{} needs {target} at {at}", live.source)?,
                    None => write!(f, "{target} at {at} was asked for")?,
                }
                write!(f, ", which the Cactarium cannot read: {detail}")?
            }
            Refusal::MissingThorns { taker, thorns, live } => {
                write!(f, "{taker} has no {}, which {} needs", thorns.join(", "), live.source)?
            }
            Refusal::ReplacementLacksThorns { by_label, base_repo, thorns, .. } => write!(
                f,
                "{by_label} would replace base repo {base_repo}, but lacks its enabled thorns {}",
                thorns.join(", ")
            )?,
            Refusal::LibraryClash { library, placements } => write!(
                f,
                "No release of library {library} meets every package placing it:{}",
                list(placements)
            )?,
            Refusal::Unclaimed(unmet) => {
                if unmet.iter().any(|u| !u.ignored.is_empty()) {
                    write!(f, "Requirements not met by the installation:")?;
                } else {
                    write!(f, "Undeclared requirements not met by the installation:")?;
                }
                for u in unmet {
                    write!(f, "\n  {} ({}) {}", u.thorn, u.package, u.requirement)?;
                    if !u.ignored.is_empty() {
                        write!(f, "; --ignore-claim {} dropped a dependency that may declare it", u.ignored.join(", "))?;
                    }
                    if u.providers.is_empty() {
                        write!(f, "; no registered package provides it")?;
                    }
                }
            }
            Refusal::NamedLibrary { target } => write!(
                f,
                "{target} is a library, which has no thorns"
            )?,
            Refusal::NotALibrary { target, by, into, .. } => write!(
                f,
                "{by} places {target} (into = \"{into}\"), but {target} is not a library"
            )?,
            Refusal::NotRegistered { target, by: Some(live) } => {
                write!(f, "{} needs {target}, which the Cactarium does not know", live.source)?
            }
            Refusal::NotRegistered { target, by: None } => write!(f, "The Cactarium does not know {target}")?,
            Refusal::BadClaim { by, claim, detail, .. } => {
                write!(f, "{by} has an unreadable claim ({claim}): {detail}")?
            }
            Refusal::Unreadable { needs, .. } => {
                write!(f, "No plan works without packages unread by the Cactarium:")?;
                for n in needs {
                    write!(f, "\n  {n}")?;
                }
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
