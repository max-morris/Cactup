//! Claim overrides (`cactup claims override …`, kept in the installation) and `--ignore-claim`
//! (one command), applied to every release's claims as the resolver loads it.
// Plan §2.2 step 1, §4.4; misfits 14 (any-of targets) and 15 (`local:<name>`).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use cactup_ecosystem_common::api::ClaimInfo;

use super::{PkgId, Release};
use crate::installation::ClaimOverride;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::version::TrackedReq;

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

    pub fn matches(&self, id: &PkgId, claim: &ClaimInfo) -> bool {
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
                        "\"{s}\": the kind of claim is dependency, incompatible-with or fork-of, not \"{kind}\"."
                    )
                })?;
                Ok(ClaimPattern::new(package, kind, PkgId::parse(target)?))
            }
            (Some(_), None) => bail!("\"{s}\" is not OWNER/PKG or OWNER/PKG:KIND:TARGET."),
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
    /// Drop a claim (`OWNER/PKG:KIND:TARGET`), or every claim of a package (`OWNER/PKG`), as
    /// `--ignore-claim` does.
    Drop(ClaimPattern),
    /// Add a claim the package's manifest lacks (a dependency, usually).
    Add { package: PkgId, claim: ClaimInfo },
    /// Add `needs` thorns to the package's dependency on `target`.
    Needs { package: PkgId, target: PkgId, thorns: Vec<String> },
    /// `by` stands in for `target`: it takes every need for `target`, with no version check.
    /// `by` may be a pseudo-package (`local:<name>`).
    Satisfies { by: PkgId, target: PkgId },
}

impl Override {
    /// `drop OWNER/PKG:KIND:TARGET`: the claim named, in every version of the package.
    pub fn drop_claim(claim: &str) -> Res<Override> {
        let pattern: ClaimPattern = claim.parse()?;
        if pattern.kind.is_none() {
            bail!("\"{claim}\" is not OWNER/PKG:KIND:TARGET.");
        }
        Ok(Override::Drop(pattern))
    }

    /// `add OWNER/PKG dependency TARGET[@RANGE] needs A,B [needed-by T]`: a dependency the
    /// package's manifest lacks.
    pub fn add_dependency(package: &str, dependency: &str, needs: &[String], needed_by: &[String]) -> Res<Override> {
        let package = PkgId::parse(package)?;
        if package.is_repo() {
            bail!("{package} is not a registered package.");
        }
        let (target, range) = match dependency.split_once('@') {
            Some((t, r)) => (t, Some(canonical_range(r).with_context(|| format!("\"{dependency}\""))?)),
            None => (dependency, None),
        };
        let target = PkgId::parse(target)?;
        if target.is_repo() {
            bail!("{target} is not a registered package.");
        }
        if target == package {
            bail!("{package} cannot depend on itself.");
        }
        let needs = thorn_list(needs).context("--needs")?;
        if needs.is_empty() {
            bail!("--needs names no thorn.");
        }
        let needed_by = thorn_list(needed_by).context("--needed-by")?;
        let claim = ClaimInfo {
            kind: "dependency".to_owned(),
            target: target.as_str().to_owned(),
            range,
            sha: None,
            needs,
            into: None,
            needed_by,
            alternatives: Vec::new(),
            note: Some("claims override".to_owned()),
        };
        Ok(Override::Add { package, claim })
    }

    /// `needs OWNER/PKG:dependency:TARGET A,B`: more thorns of the target the dependency needs.
    pub fn add_needs(claim: &str, thorns: &[String]) -> Res<Override> {
        let pattern: ClaimPattern = claim.parse()?;
        let Some((ClaimKind::Dependency, target)) = pattern.kind else {
            bail!("\"{claim}\" is not OWNER/PKG:dependency:TARGET.");
        };
        let thorns = thorn_list(thorns)?;
        if thorns.is_empty() {
            bail!("No thorn named.");
        }
        Ok(Override::Needs { package: pattern.package, target, thorns })
    }

    /// `satisfies X Y`: `X` stands in for `Y`.
    pub fn satisfies(by: &str, target: &str) -> Res<Override> {
        let (by, target) = (PkgId::parse(by)?, PkgId::parse(target)?);
        if target.is_repo() {
            bail!("{target} is not a registered package.");
        }
        if by == target {
            bail!("{by} cannot stand in for itself.");
        }
        Ok(Override::Satisfies { by, target })
    }

    /// The override's canonical text, which is its key in the installation's `claim-overrides`:
    /// the same override always has the same text, whatever order its thorns were named in.
    pub fn key(&self) -> String {
        match self {
            Override::Drop(pattern) => format!("drop {pattern}"),
            Override::Add { package, claim } => {
                let target = match &claim.range {
                    Some(range) => format!("{}@{range}", claim.target),
                    None => claim.target.clone(),
                };
                let by = match claim.needed_by.is_empty() {
                    true => String::new(),
                    false => format!(" needed-by {}", claim.needed_by.join(",")),
                };
                format!("add {package} dependency {target} needs {}{by}", claim.needs.join(","))
            }
            Override::Needs { package, target, thorns } => {
                format!("needs {package}:dependency:{target} {}", thorns.join(","))
            }
            Override::Satisfies { by, target } => format!("satisfies {by} {target}"),
        }
    }

    /// Whether the key reads back as this override: one that does not would make the installation
    /// unusable once recorded.
    pub fn reads_back(&self) -> bool {
        Override::from_key(&self.key()).is_ok_and(|o| &o == self)
    }

    /// The override a key names (the inverse of [`Override::key`]).
    pub fn from_key(key: &str) -> Res<Override> {
        let words: Vec<&str> = key.split_whitespace().collect();
        let list = |s: &str| s.split(',').map(str::to_owned).collect::<Vec<String>>();
        match words.as_slice() {
            ["drop", claim] => Override::drop_claim(claim),
            ["needs", claim, thorns] => Override::add_needs(claim, &list(thorns)),
            ["satisfies", by, target] => Override::satisfies(by, target),
            ["add", package, "dependency", target, "needs", needs] => {
                Override::add_dependency(package, target, &list(needs), &[])
            }
            ["add", package, "dependency", target, "needs", needs, "needed-by", by] => {
                Override::add_dependency(package, target, &list(needs), &list(by))
            }
            _ => bail!("\"{key}\" is not the text of a claim override."),
        }
    }

    /// The `kind` its record carries.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Override::Drop(_) => "drop",
            Override::Add { .. } => "add",
            Override::Needs { .. } => "needs",
            Override::Satisfies { .. } => "satisfies",
        }
    }
}

/// A version range as a key keeps it: as the range reads, with no whitespace (`>=1.0,<2.0`,
/// `^1||:^2`), so the key is one word that reads back as the same range.
fn canonical_range(text: &str) -> Res<String> {
    let range: TrackedReq = text.parse()?;
    let canonical: String = range.to_string().split_whitespace().collect();
    if canonical.parse::<TrackedReq>().map(|r| r != range).unwrap_or(true) {
        bail!("\"{text}\" cannot be written as one word.");
    }
    Ok(canonical)
}

/// Thorn names as an override keeps them: commas split, no empty or repeated name (case
/// aside), in name order, each spelled as first given.
fn thorn_list(given: &[String]) -> Res<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in given.iter().flat_map(|g| g.split(',')).map(str::trim).filter(|n| !n.is_empty()) {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("\"{name}\" is not a thorn name.");
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(name)) {
            out.push(name.to_owned());
        }
    }
    out.sort_by_key(|n| n.to_ascii_lowercase());
    Ok(out)
}

/// The overrides an installation records, in key order. A record that does not read is an
/// error: ignoring an override would change what resolution does.
pub fn overrides_of(records: &BTreeMap<String, ClaimOverride>) -> Res<Vec<Override>> {
    records
        .iter()
        .map(|(key, record)| {
            read_record(key, record).map_err(|_| unreadable(key))
        })
        .collect()
}

/// The override a record is, or an error when its key does not read or its kind disagrees.
pub fn read_record(key: &str, record: &ClaimOverride) -> Res<Override> {
    let o = Override::from_key(key)?;
    if o.kind_name() != record.kind {
        bail!("\"{key}\" says kind \"{}\".", record.kind);
    }
    Ok(o)
}

/// What an override that does not read says: where it is, and how to be rid of it.
pub fn unreadable(key: &str) -> anyhow::Error {
    anyhow!("installation.toml has a claim override that does not read: '{key}'.\nRemove it with `cactup claims override --remove '{key}'`.")
}

/// `--ignore-claim` patterns, read as `claims override drop` reads its claim.
pub fn parse_ignores(patterns: &[String]) -> Res<Vec<ClaimPattern>> {
    patterns.iter().map(|s| s.parse::<ClaimPattern>().with_context(|| format!("--ignore-claim {s}"))).collect()
}

/// The overrides and ignores a resolution runs under.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    /// `--ignore-claim`, this command only.
    pub ignore: Vec<ClaimPattern>,
    pub overrides: Vec<Override>,
}

impl Rules {
    /// The rules of an installation's recorded overrides, and `ignore` for this command.
    pub fn of(records: &BTreeMap<String, ClaimOverride>, ignore: Vec<ClaimPattern>) -> Res<Rules> {
        Ok(Rules { ignore, overrides: overrides_of(records)? })
    }

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
            let drops = |o: &Override| matches!(o, Override::Drop(p) if p.matches(&r.id, c));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Every override as the command line builds it.
    fn samples() -> Vec<Override> {
        vec![
            Override::drop_claim("max/foo:dependency:et/bar").unwrap(),
            Override::drop_claim("max/foo:incompatible-with:et/bar").unwrap(),
            Override::drop_claim("max/carpet-mp:fork-of:et/carpet").unwrap(),
            Override::add_dependency("max/foo", "et/bar", &strings(&["Loop", "CarpetX"]), &[]).unwrap(),
            Override::add_dependency("max/foo", "et/bar@^1.2", &strings(&["Loop"]), &strings(&["Zeta", "Alpha"])).unwrap(),
            Override::add_dependency("max/foo", "et/bar@>=1.0, <2.0", &strings(&["Loop"]), &[]).unwrap(),
            Override::add_dependency("max/foo", "et/bar@1.x || 2.x", &strings(&["Loop"]), &[]).unwrap(),
            Override::add_dependency("max/foo", "et/bar@mp:>=1 <2 || :^3", &strings(&["Loop"]), &[]).unwrap(),
            Override::add_dependency("max/foo", "et/bar@mixed:^1", &strings(&["Loop"]), &[]).unwrap(),
            Override::add_needs("max/foo:dependency:et/bar", &strings(&["Loop", "arith"])).unwrap(),
            Override::satisfies("max/carpet-mp", "et/carpet").unwrap(),
            Override::satisfies("local:carpetx", "et/carpetx").unwrap(),
        ]
    }

    /// The key is the override's text: it reads back as the same override, and the same text
    /// again, so a key written today is the key found tomorrow.
    #[test]
    fn a_key_reads_back_as_itself() {
        let keys: Vec<String> = samples().iter().map(Override::key).collect();
        assert_eq!(
            keys,
            [
                "drop max/foo:dependency:et/bar",
                "drop max/foo:incompatible-with:et/bar",
                "drop max/carpet-mp:fork-of:et/carpet",
                "add max/foo dependency et/bar needs CarpetX,Loop",
                "add max/foo dependency et/bar@^1.2 needs Loop needed-by Alpha,Zeta",
                "add max/foo dependency et/bar@>=1.0,<2.0 needs Loop",
                "add max/foo dependency et/bar@1.*||2.* needs Loop",
                "add max/foo dependency et/bar@mp:>=1,<2||:^3 needs Loop",
                "add max/foo dependency et/bar@mixed:^1 needs Loop",
                "needs max/foo:dependency:et/bar arith,Loop",
                "satisfies max/carpet-mp et/carpet",
                "satisfies local:carpetx et/carpetx",
            ]
        );
        for (o, key) in samples().iter().zip(&keys) {
            let back = Override::from_key(key).unwrap();
            assert_eq!(&back, o, "{key}");
            assert_eq!(&back.key(), key);
        }
    }

    /// The order thorns are named in, their case, repeats and commas change nothing.
    #[test]
    fn thorns_named_any_way_make_one_key() {
        let a = Override::add_needs("max/foo:dependency:et/bar", &strings(&["Loop", "CarpetX"])).unwrap();
        let b = Override::add_needs("max/foo:dependency:et/bar", &strings(&["CarpetX,Loop,loop", " CarpetX"])).unwrap();
        assert_eq!(a.key(), b.key());
        assert_eq!(a.key(), "needs max/foo:dependency:et/bar CarpetX,Loop");
        let a = Override::add_dependency("max/foo", "et/bar", &strings(&["B", "a"]), &strings(&["Y", "x"])).unwrap();
        let b = Override::add_dependency("max/foo", "et/bar", &strings(&["a,B"]), &strings(&["x,Y"])).unwrap();
        assert_eq!(a.key(), b.key());
        // A range is written as the range reads, not as it was typed.
        let a = Override::add_dependency("max/foo", "et/bar@^1.2", &strings(&["A"]), &[]).unwrap();
        let b = Override::add_dependency("max/foo", "et/bar@^1.2.0", &strings(&["A"]), &[]).unwrap();
        assert_eq!(Override::from_key(&a.key()).unwrap(), a);
        assert_eq!(Override::from_key(&b.key()).unwrap(), b);
    }

    /// An override whose key would not read back is told apart (the command refuses to record it).
    #[test]
    fn an_override_that_does_not_read_back_is_told_apart() {
        assert!(samples().iter().all(Override::reads_back));
        let Override::Add { package, mut claim } = Override::add_dependency("max/foo", "et/bar", &strings(&["A"]), &[]).unwrap()
        else {
            unreachable!()
        };
        claim.range = Some(">=1.0, <2.0".to_owned());
        assert!(!Override::Add { package, claim }.reads_back());
    }

    /// What the commands refuse to make.
    #[test]
    fn nonsense_overrides_are_refused() {
        let err = |r: Res<Override>| format!("{:#}", r.unwrap_err());
        assert!(err(Override::drop_claim("max/foo")).contains("OWNER/PKG:KIND:TARGET"));
        assert!(err(Override::drop_claim("max/foo:depends:et/bar")).contains("fork-of"));
        assert!(err(Override::drop_claim("foo:dependency:et/bar")).contains("owner/name"));
        assert!(err(Override::add_needs("max/foo:fork-of:et/bar", &strings(&["A"]))).contains("OWNER/PKG:dependency:TARGET"));
        assert!(err(Override::add_needs("max/foo:dependency:et/bar", &[])).contains("No thorn"));
        assert!(err(Override::add_needs("max/foo:dependency:et/bar", &strings(&["a b"]))).contains("not a thorn name"));
        assert!(err(Override::satisfies("et/a", "et/a")).contains("itself"));
        assert!(err(Override::satisfies("et/a", "local:b")).contains("not a registered package"));
        assert!(err(Override::satisfies("local:", "et/a")).contains("local:<name>"));
        assert!(err(Override::add_dependency("max/foo", "et/bar", &[], &[])).contains("--needs"));
        assert!(err(Override::add_dependency("local:foo", "et/bar", &strings(&["A"]), &[])).contains("not a registered package."));
        assert!(err(Override::add_dependency("max/foo", "local:bar", &strings(&["A"]), &[])).contains("not a registered"));
        assert!(err(Override::add_dependency("max/foo", "et/bar@not a range", &strings(&["A"]), &[])).contains("et/bar@not a range"));
        assert!(err(Override::add_dependency("max/foo", "max/foo", &strings(&["A"]), &[])).contains("cannot depend on itself."));
        for key in ["", "drop", "drop a b", "needs max/foo:dependency:et/bar", "remove max/foo", "add max/foo et/bar needs A"] {
            assert!(Override::from_key(key).is_err(), "{key}");
        }
    }

    /// A recorded override that does not read, or whose kind disagrees with its key, is an error:
    /// ignoring it would change what resolution does.
    #[test]
    fn recorded_overrides_are_read_whole_or_refused() {
        let record = |kind: &str| ClaimOverride { kind: kind.to_owned(), note: None };
        let ok: BTreeMap<String, ClaimOverride> =
            [("drop max/foo:dependency:et/bar".to_owned(), record("drop")), ("satisfies a/b c/d".to_owned(), record("satisfies"))]
                .into();
        let rules = Rules::of(&ok, Vec::new()).unwrap();
        assert_eq!(rules.overrides.iter().map(Override::kind_name).collect::<Vec<_>>(), ["drop", "satisfies"]);
        let wrong: BTreeMap<String, ClaimOverride> = [("drop max/foo:dependency:et/bar".to_owned(), record("needs"))].into();
        let one_line = |m: &BTreeMap<String, ClaimOverride>| format!("{:#}", overrides_of(m).unwrap_err());
        assert_eq!(
            one_line(&wrong),
            "installation.toml has a claim override that does not read: 'drop max/foo:dependency:et/bar'.\n\
             Remove it with `cactup claims override --remove 'drop max/foo:dependency:et/bar'`."
        );
        let junk: BTreeMap<String, ClaimOverride> = [("prefer a/b".to_owned(), record("prefer"))].into();
        assert!(one_line(&junk).contains("does not read: 'prefer a/b'.\nRemove it with"), "{}", one_line(&junk));
    }

    /// `--ignore-claim` reads a pattern as `claims override drop` does, and says which flag was wrong.
    #[test]
    fn ignores_and_drops_share_one_pattern_parser() {
        let ignores = parse_ignores(&strings(&["max/foo", "max/foo:dependency:et/bar"])).unwrap();
        assert_eq!(ignores[1], ClaimPattern::new(PkgId::parse("max/foo").unwrap(), ClaimKind::Dependency, PkgId::parse("et/bar").unwrap()));
        assert_eq!(Override::drop_claim("max/foo:dependency:et/bar").unwrap(), Override::Drop(ignores[1].clone()));
        let err = format!("{:#}", parse_ignores(&strings(&["max/foo:depends:et/bar"])).unwrap_err());
        assert!(err.starts_with("--ignore-claim max/foo:depends:et/bar: "), "{err}");
    }
}
