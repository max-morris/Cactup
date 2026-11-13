//! Claim overrides (`cactup claims override …`, kept in the installation) and `--ignore-claim`
//! (one command), applied to every release's claims as the resolver loads it.
// Plan §2.2 step 1, §4.4; misfits 14 (any-of targets) and 15 (`repo:<name>`).

use std::fmt;
use std::str::FromStr;

use cactup_ecosystem_common::api::ClaimInfo;

use super::{PkgId, Release};
use crate::Res;
use anyhow::{anyhow, bail};

/// The kinds of claim an override or `--ignore-claim` can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClaimKind {
    Dependency,
    IncompatibleWith,
    ForkOf,
}

impl ClaimKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimKind::Dependency => "dependency",
            ClaimKind::IncompatibleWith => "incompatible-with",
            ClaimKind::ForkOf => "fork-of",
        }
    }

    fn parse(s: &str) -> Option<ClaimKind> {
        match s {
            "dependency" => Some(ClaimKind::Dependency),
            "incompatible-with" => Some(ClaimKind::IncompatibleWith),
            "fork-of" => Some(ClaimKind::ForkOf),
            _ => None,
        }
    }
}

/// `OWNER/PKG[:KIND:TARGET]`: every claim of a package, or its claims of one kind on one target,
/// in every version of it. The target of an any-of dependency is any of its alternatives, and
/// matching one drops the whole dependency.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClaimPattern {
    pub package: PkgId,
    pub kind: Option<(ClaimKind, PkgId)>,
}

impl ClaimPattern {
    pub fn new(package: PkgId, kind: ClaimKind, target: PkgId) -> ClaimPattern {
        ClaimPattern { package, kind: Some((kind, target)) }
    }

    fn matches(&self, id: &PkgId, claim: &ClaimInfo) -> bool {
        if &self.package != id {
            return false;
        }
        let Some((kind, target)) = &self.kind else { return true };
        let names = |t: &str| t == target.as_str();
        let any_alternative = claim.alternatives.iter().any(|a| names(&a.target));
        claim.kind == kind.as_str() && (names(&claim.target) || any_alternative)
    }
}

impl FromStr for ClaimPattern {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Res<ClaimPattern> {
        let mut parts = s.splitn(3, ':');
        let package = PkgId::parse(parts.next().unwrap_or(""))?;
        match (parts.next(), parts.next()) {
            (None, _) => Ok(ClaimPattern { package, kind: None }),
            (Some(kind), Some(target)) => {
                let kind = ClaimKind::parse(kind).ok_or_else(|| {
                    anyhow!(
                        "\"{s}\": the kind of claim is dependency, incompatible-with or fork-of, not \"{kind}\""
                    )
                })?;
                Ok(ClaimPattern::new(package, kind, PkgId::parse(target)?))
            }
            (Some(_), None) => bail!("\"{s}\" is not OWNER/PKG or OWNER/PKG:KIND:TARGET"),
        }
    }
}

impl fmt::Display for ClaimPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.package)?;
        if let Some((kind, target)) = &self.kind {
            write!(f, ":{}:{target}", kind.as_str())?;
        }
        Ok(())
    }
}

/// One durable override of the claims the registry serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Override {
    /// Drop a claim (`OWNER/PKG:KIND:TARGET`).
    Drop(ClaimPattern),
    /// Add a claim the package's manifest lacks (a dependency, usually).
    Add { package: PkgId, claim: ClaimInfo },
    /// Add `needs` thorns to the package's dependency on `target`.
    Needs { package: PkgId, target: PkgId, thorns: Vec<String> },
    /// `by` stands in for `target`: it takes every need for `target`, with no version check.
    /// `by` may be a pseudo-package (`repo:<name>`).
    Satisfies { by: PkgId, target: PkgId },
}

/// The overrides and ignores a resolution runs under.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// `--ignore-claim`, this command only.
    pub ignore: Vec<ClaimPattern>,
    pub overrides: Vec<Override>,
}

impl Rules {
    /// Rewrites a release's claims: added claims first, then every dropped or ignored one goes,
    /// then extra `needs` are merged in.
    pub(super) fn apply(&self, r: &mut Release) {
        for o in &self.overrides {
            if let Override::Add { package, claim } = o
                && package == &r.id
            {
                r.claims.push(claim.clone());
            }
        }
        let dropped = |c: &ClaimInfo| {
            let drops =
                |o: &Override| matches!(o, Override::Drop(p) if p.kind.is_some() && p.matches(&r.id, c));
            self.ignore.iter().any(|p| p.matches(&r.id, c)) || self.overrides.iter().any(drops)
        };
        let kept: Vec<ClaimInfo> = r.claims.iter().filter(|c| !dropped(c)).cloned().collect();
        r.claims = kept;
        for o in &self.overrides {
            let Override::Needs { package, target, thorns } = o else { continue };
            if package != &r.id {
                continue;
            }
            let merge = |needs: &mut Vec<String>| {
                for t in thorns {
                    if !needs.iter().any(|n| n.eq_ignore_ascii_case(t)) {
                        needs.push(t.clone());
                    }
                }
            };
            for c in r.claims.iter_mut().filter(|c| c.kind == "dependency") {
                if c.alternatives.is_empty() {
                    if c.target == target.as_str() {
                        merge(&mut c.needs);
                    }
                    continue;
                }
                for a in c.alternatives.iter_mut().filter(|a| a.target == target.as_str()) {
                    merge(&mut a.needs);
                }
                // `target`/`needs` repeat the first alternative.
                if c.target == target.as_str() {
                    merge(&mut c.needs);
                }
            }
        }
    }

    /// The packages an override says stand in for `target`.
    pub(super) fn satisfiers<'a>(&'a self, target: &'a PkgId) -> impl Iterator<Item = &'a PkgId> + 'a {
        self.overrides.iter().filter_map(move |o| match o {
            Override::Satisfies { by, target: t } if t == target => Some(by),
            _ => None,
        })
    }
}
