//! `--trim` for `sim export`/`sim pack` (spec §8.9): cut a thornlist down to
//! what a parfile needs — its `ActiveThorns`, closed under what those
//! thorns' `.ccl` files require — so a `.sim` does not drag in the other
//! three hundred thorns of the list it was built from.
//!
//! Conservative throughout: a requirement two thorns could meet keeps both,
//! the flesh and every component that is not an arrangement thorn stay, and
//! a trimmed thorn is not deleted from the list but written as a
//! `#DISABLED Arrangement/Thorn` line, so it is neither fetched nor built
//! but stays visible and is one edit away from coming back.

use crate::thornlist::{Component, Thornlist};
use crate::Res;
use anyhow::{bail, Context};
use cactup_ecosystem_common::ccl::ThornFacts;
use colored::Colorize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The thorns a parfile activates: every `ActiveThorns = "…"` statement (the
/// key case-insensitive, as Cactus reads it), comments dropped, names
/// lowercased because Cactus compares thorn names case-insensitively.
pub fn active_thorns(par: &str) -> BTreeSet<String> {
    let re = regex::Regex::new(r#"(?is)\bActiveThorns\s*=\s*"([^"]*)""#).expect("valid regex");
    // Drop whole-line comments first, so a commented-out statement counts
    // for nothing.
    let code: String = par
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    re.captures_iter(&code)
        .flat_map(|c| {
            c[1].lines()
                .map(|l| l.split('#').next().unwrap_or("").to_owned())
                .flat_map(|l| l.split_whitespace().map(str::to_ascii_lowercase).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The list's arrangement thorns: checkout → (lowercased thorn name, facts
/// read from the installed tree under `cactus_root`). Read on the parallel
/// pool with progress; ~400 thorns' `.ccl` files on a network filesystem add
/// up. A thorn whose `interface.ccl` cannot be read is kept by the caller,
/// never guessed at.
fn thorn_facts(list: &Thornlist, cactus_root: &Path) -> Res<BTreeMap<String, (String, Option<ThornFacts>)>> {
    let thorns: Vec<&Component> = list.components().iter().filter(|c| is_arrangement_thorn(list, c)).collect();
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("read thorns' .ccl files");
    item.init(Some(thorns.len()), Some(prodash::unit::label("thorns")));
    let item = std::sync::Mutex::new(item);
    let read = crate::par::parallel_map(&thorns, |c| {
        let current = item.lock().expect("trim progress poisoned").add_child(c.checkout.clone());
        let dir = cactus_root.join("arrangements").join(&c.checkout);
        let facts = std::fs::read_to_string(dir.join("interface.ccl")).ok().map(|interface| {
            let configuration = std::fs::read_to_string(dir.join("configuration.ccl")).ok();
            ThornFacts::scan(&interface, configuration.as_deref())
        });
        drop(current);
        item.lock().expect("trim progress poisoned").inc();
        facts
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    Ok(thorns.iter().zip(read?).map(|(c, facts)| (c.checkout.clone(), (thorn_name(&c.checkout), facts))).collect())
}

fn is_arrangement_thorn(list: &Thornlist, c: &Component) -> bool {
    c.target.trim_end_matches('/') == format!("{}/arrangements", list.root().trim_end_matches('/'))
        && c.checkout.matches('/').count() == 1
}

fn thorn_name(checkout: &str) -> String {
    checkout.rsplit('/').next().unwrap_or(checkout).to_ascii_lowercase()
}

/// The checkouts a parfile needs: its active thorns, closed under what they
/// require. Returns the arrangement-thorn checkouts to keep; every other
/// component is kept regardless.
pub fn needed(list: &Thornlist, cactus_root: &Path, par: &str) -> Res<BTreeSet<String>> {
    let facts = thorn_facts(list, cactus_root)?;
    let active = active_thorns(par);
    if active.is_empty() {
        bail!("The parfile activates no thorns (no ActiveThorns statement), so there is nothing to trim to");
    }
    let by_name: BTreeMap<&str, &str> = facts.iter().map(|(checkout, (name, _))| (name.as_str(), checkout.as_str())).collect();
    let missing: Vec<&String> = active.iter().filter(|t| !by_name.contains_key(t.as_str())).collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        bail!("The parfile activates thorns the thornlist does not name: {}", names.join(", "));
    }
    let lower = |v: &[String]| v.iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>();
    // Who offers what, across the whole list.
    let mut providers: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut implementers: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut function_providers: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (checkout, (name, facts)) in &facts {
        let Some(facts) = facts else { continue };
        for capability in lower(&facts.provides) {
            providers.entry(capability).or_default().push(checkout);
        }
        let implements = facts.implements.as_deref().unwrap_or(name).to_ascii_lowercase();
        implementers.entry(implements).or_default().push(checkout);
        for function in lower(&facts.provides_functions) {
            function_providers.entry(function).or_default().push(checkout);
        }
    }

    let mut keep: BTreeSet<String> = active.iter().map(|t| by_name[t.as_str()].to_owned()).collect();
    // A thorn whose facts could not be read is kept: nothing says it is unneeded.
    keep.extend(facts.iter().filter(|(_, (_, f))| f.is_none()).map(|(c, _)| c.clone()));
    let mut unmet = BTreeSet::new();
    loop {
        let before = keep.len();
        let kept: Vec<String> = keep.iter().cloned().collect();
        let offered = |table: &BTreeMap<String, Vec<&str>>, what: &str, keep: &BTreeSet<String>| {
            table.get(what).is_some_and(|ps| ps.iter().any(|p| keep.contains(*p)))
        };
        for checkout in &kept {
            let Some((_, Some(facts))) = facts.get(checkout) else { continue };
            let mut add: Vec<&str> = Vec::new();
            // Hard requirements: every provider, since which one a build
            // would settle on is not ours to guess.
            for capability in lower(&facts.requires) {
                match providers.get(&capability) {
                    Some(ps) => add.extend(ps.iter().copied()),
                    None => {
                        unmet.insert(format!("capability {capability} (required by {checkout})"));
                    }
                }
            }
            for thorn in lower(&facts.requires_thorns) {
                if let Some(c) = by_name.get(thorn.as_str()) {
                    add.push(c);
                }
            }
            // Implementations and functions: providers only when no kept
            // thorn already offers one — the parfile chose that one.
            for implementation in lower(&facts.inherits).into_iter().chain(lower(&facts.friends)) {
                if !offered(&implementers, &implementation, &keep) {
                    add.extend(implementers.get(&implementation).into_iter().flatten().copied());
                }
            }
            for function in lower(&facts.requires_functions) {
                if !offered(&function_providers, &function, &keep) {
                    add.extend(function_providers.get(&function).into_iter().flatten().copied());
                }
            }
            keep.extend(add.into_iter().map(str::to_owned));
        }
        if keep.len() == before {
            break;
        }
    }
    for what in unmet {
        println!("{}", format!("Nothing in the thornlist provides {what}; the build will need it").yellow());
    }
    Ok(keep)
}

/// `text` with every arrangement-thorn checkout not in `keep` moved out of
/// its `!CHECKOUT` into a `#DISABLED` line (§7.5), so it is neither fetched
/// nor built. Sections left with no checkout at all are dropped whole.
/// Everything else stays byte for byte; the result is parsed back and must
/// name exactly the kept checkouts.
pub fn trim_text(text: &str, list: &Thornlist, keep: &BTreeSet<String>) -> Res<String> {
    let drop: BTreeSet<&str> = list
        .components()
        .iter()
        .filter(|c| is_arrangement_thorn(list, c) && !keep.contains(&c.checkout))
        .map(|c| c.checkout.as_str())
        .collect();
    drop_checkouts(text, list, &drop, true)
}

/// `text` (parsed as `list`) with the checkouts in `drop` taken out of their
/// `!CHECKOUT`s — written as `#DISABLED` lines when `disable`, else gone —
/// and everything else byte for byte. Parsed back, it must name exactly the
/// other checkouts.
pub fn drop_checkouts(text: &str, list: &Thornlist, drop: &BTreeSet<&str>, disable: bool) -> Res<String> {
    if drop.is_empty() {
        return Ok(text.to_owned());
    }
    let key_re = regex::Regex::new(r"(?m)^[ \t]*!([^\s=]+)[ \t]*=").expect("valid regex");
    let keys: Vec<(String, usize, usize)> = key_re
        .captures_iter(text)
        .map(|c| {
            let m = c.get(0).expect("match");
            (c[1].to_owned(), m.start(), m.end())
        })
        .collect();
    let mut out = String::new();
    let mut done = 0;
    for (i, (key, start, value_start)) in keys.iter().enumerate() {
        if key != "CHECKOUT" {
            continue;
        }
        let value_end = keys.get(i + 1).map_or(text.len(), |k| k.1);
        let value = &text[*value_start..value_end];
        // Tokens before the first comment-only line belong to this
        // checkout; `#` ends a token run on its line, as the parser reads it.
        let tokens: Vec<&str> = value
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .flat_map(|l| l.split('#').next().unwrap_or("").split_whitespace())
            .collect();
        if !tokens.iter().any(|t| drop.contains(t)) {
            continue;
        }
        let kept: Vec<&str> = tokens.iter().copied().filter(|t| !drop.contains(t)).collect();
        let dropped: Vec<&str> = tokens.iter().copied().filter(|t| drop.contains(t)).collect();
        // The comment and blank lines already in the value keep their place
        // after the rewritten checkouts.
        let trailing: String = value
            .split_inclusive('\n')
            .skip(1)
            .filter(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
            .collect();
        out.push_str(&text[done..*start]);
        // One checkout per line, never on the `!CHECKOUT` line itself: Cactus
        // reads the same file as its ThornList, skipping every `!` line and
        // squeezing the spaces out of the rest (lib/sbin/MakeUtils.pl,
        // ReadThornlist), so a thorn sharing a line is never built. (No
        // checkout left: the section fetches nothing, which is harmless.)
        out.push_str("!CHECKOUT =\n");
        for k in &kept {
            out.push_str(&format!("{k}\n"));
        }
        for d in dropped.into_iter().filter(|_| disable) {
            out.push_str(&format!("#DISABLED {d}\n"));
        }
        out.push_str(&trailing);
        done = value_end;
    }
    out.push_str(&text[done..]);

    let trimmed = crate::thornlist::parse(&out).context("The trimmed thornlist does not parse")?;
    let names = |l: &Thornlist| -> BTreeSet<String> { l.components().iter().map(|c| c.checkout.clone()).collect() };
    let want: BTreeSet<String> = names(list).into_iter().filter(|c| !drop.contains(c.as_str())).collect();
    if names(&trimmed) != want {
        bail!("Dropping checkouts changed more than those checkouts (a bug in rewriting this list)");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_thorns_reads_every_statement_case_insensitively() {
        let par = "# ActiveThorns = \"Commented\"\n\
                   ActiveThorns = \"CarpetX Loop # the driver\n  ODESolvers\"\n\
                   activethorns = \"IOUtil\"\n";
        let active: Vec<String> = active_thorns(par).into_iter().collect();
        assert_eq!(active, ["carpetx", "ioutil", "loop", "odesolvers"]);
    }

    /// A tree whose thorns exercise each rule: a capability two thorns
    /// provide (both kept), an implementation an active thorn already
    /// provides (the alternative not added), REQUIRES THORNS, a required
    /// function, and unrelated thorns that are trimmed — written as
    /// `#DISABLED` lines — while the flesh stays.
    #[test]
    fn trim_keeps_the_closure_and_disables_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("Cactus");
        let thorn = |checkout: &str, interface: &str, configuration: Option<&str>| {
            let dir = cactus.join("arrangements").join(checkout);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("interface.ccl"), interface).unwrap();
            if let Some(c) = configuration {
                std::fs::write(dir.join("configuration.ccl"), c).unwrap();
            }
        };
        thorn("A/Main", "IMPLEMENTS: main\nINHERITS: driver\nREQUIRES FUNCTION Solve\n", Some("REQUIRES HDF5\nREQUIRES THORNS: Helper\n"));
        thorn("A/Driver", "IMPLEMENTS: driver\n", None);
        thorn("A/OtherDriver", "IMPLEMENTS: driver\n", None);
        thorn("A/Helper", "IMPLEMENTS: helper\n", None);
        thorn("A/Solver", "IMPLEMENTS: solver\nPROVIDES FUNCTION Solve WITH SolveImpl LANGUAGE C\n", None);
        thorn("Lib/HDF5", "IMPLEMENTS: hdf5\n", Some("PROVIDES HDF5\n{\n  SCRIPT configure.sh\n  LANG bash\n}\n"));
        thorn("Lib/HDF5Alt", "IMPLEMENTS: hdf5alt\n", Some("PROVIDES HDF5\n{\n}\n"));
        thorn("B/Unused", "IMPLEMENTS: unused\n", None);
        thorn("B/AlsoUnused", "IMPLEMENTS: alsounused\n", None);

        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT\n!TYPE = git\n!URL = https://x/flesh.git\n!CHECKOUT = Makefile\n\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/a.git\n!REPO_PATH = $2\n\
                    !CHECKOUT = A/Main A/Driver A/OtherDriver\n  A/Helper A/Solver\n\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/lib.git\n!REPO_PATH = $2\n\
                    !CHECKOUT = Lib/HDF5 Lib/HDF5Alt\n\n\
                    # unrelated\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/b.git\n!REPO_PATH = $2\n\
                    !CHECKOUT = B/Unused B/AlsoUnused\n";
        let list = crate::thornlist::parse(text).unwrap();
        let par = "ActiveThorns = \"Main Driver\"\n";
        let keep = needed(&list, &cactus, par).unwrap();
        let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
        assert_eq!(keep, ["A/Driver", "A/Helper", "A/Main", "A/Solver", "Lib/HDF5", "Lib/HDF5Alt"]);

        let trimmed = trim_text(text, &list, &needed(&list, &cactus, par).unwrap()).unwrap();
        let parsed = crate::thornlist::parse(&trimmed).unwrap();
        let checkouts: Vec<&str> = parsed.components().iter().map(|c| c.checkout.as_str()).collect();
        assert_eq!(checkouts, ["Makefile", "A/Main", "A/Driver", "A/Helper", "A/Solver", "Lib/HDF5", "Lib/HDF5Alt"]);
        assert_eq!(parsed.disabled_thorns(), ["A/OtherDriver", "B/Unused", "B/AlsoUnused"]);
        assert!(trimmed.contains("# unrelated\n!TARGET"), "comments stay: {trimmed}");
        // Cactus reads this file as its ThornList and skips `!` lines, so no
        // kept thorn may sit on a `!CHECKOUT` line, and each needs its own.
        assert!(trimmed.contains("!CHECKOUT =\nA/Main\nA/Driver\nA/Helper\nA/Solver\n"), "{trimmed}");

        // A thorn the parfile activates that the list lacks is an error.
        let err = format!("{:#}", needed(&list, &cactus, "ActiveThorns = \"Main Nope\"\n").unwrap_err());
        assert!(err.contains("nope"), "{err}");
    }
}
