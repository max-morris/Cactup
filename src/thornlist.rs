//! Thornlists. The CRL 1.0 parser itself (spec §3.2) — a port of
//! GetComponents' `parse_list` plus cactup's extensions (`#DISABLED`,
//! `!REPO_COMMIT`) — lives in the Cactup ecosystem's shared crate, where the
//! Cactarium registry parses lists with exactly the same rules; it is
//! re-exported here. What stays is what only an installed tree can answer:
//! [`parse_config_list`], which resolves a plain Cactus `ThornList` through
//! the tree's arrangement links.

pub use cactup_ecosystem_common::crl::{parse, parse_with_base, Component, ComponentType, Thornlist};

use cactup_ecosystem_common::crl::lexical_canonicalize;
use std::path::Path;

/// A config's processed thornlist, read for the §7.4 change tracking
/// (sources, providers, shapes): a CRL list parses as one, and a plain Cactus
/// `ThornList` (one `Arrangement/Thorn` per line, no `!CRL_VERSION` header) —
/// which Cactus builds from just as happily, and which hand-written benchmark
/// lists usually are — is resolved through the tree instead. Without this, a
/// config built from such a list had no tracking at all: every build of it
/// read as "up to date" whatever had changed underneath.
///
/// Resolving reads the tree, not the installation's own CRL list: a fetched
/// tree links every `arrangements/<A>/<T>` (and the flesh's `Makefile`) into
/// `repos/<repo>/…`, and that link is exactly what the build compiles from,
/// whatever the live list says now. Each linked thorn becomes a git component
/// of its repo, the flesh one checking out `Makefile` into the root (so
/// `fetch::is_flesh` finds it); a thorn that is not such a link — a real
/// directory, or not there at all — becomes an `Ignore` component, still in
/// [`Thornlist::thorn_providers`] (its shape is tracked) but taking no part
/// in source state, and is named in [`Thornlist::untracked`] (as is the flesh,
/// when its `Makefile` is not a link). The links are read on the
/// [`crate::par`] pool: ~400 `readlink`s on NFS add up.
///
/// The inner `Err` is why the text is neither kind of list, a clause about
/// "its thornlist" that reads on its own after "cannot track …'s sources: "
/// (`fetch::NoSources::Unparseable` carries it). The outer one is only an interrupt (§2.4),
/// which callers must propagate rather than fold into "unreadable".
pub fn parse_config_list(
    text: &str,
    install_root: &Path,
    cactus_root: &Path,
) -> crate::Res<Result<Thornlist, String>> {
    let crl_error = match parse(text) {
        Ok(list) => return Ok(Ok(list)),
        Err(e) => e,
    };
    // A list with a `!CRL_VERSION` header is a broken CRL list, not a plain
    // one. Matched after leading whitespace (which `extract_header` rejects),
    // so an indented header is reported as the broken CRL list it is; a
    // comment that mentions it still does not count.
    if text.lines().any(|line| line.trim_start().starts_with("!CRL_VERSION")) {
        return Ok(Err(format!("{:#}", crl_error.context("its thornlist is not a valid GetComponents (CRL) list"))));
    }
    let thorns = match plain_thorns(text) {
        Ok(thorns) => thorns,
        Err(why) => return Ok(Err(why)),
    };
    let Some(root) = cactus_root.strip_prefix(install_root).ok().and_then(|p| p.to_str()).map(str::to_owned)
    else {
        return Ok(Err(format!(
            "its Cactus root '{}' is not inside the installation '{}'",
            cactus_root.display(),
            install_root.display()
        )));
    };
    let repos_dir = cactus_root.join("repos");
    let repos_real = std::fs::canonicalize(&repos_dir).ok();
    let component = |ty, target: String, checkout: &str, repo: String| Component {
        ty,
        target,
        checkout: checkout.to_owned(),
        name: None,
        url: None,
        auth_url: None,
        anon_user: None,
        anon_pass: None,
        repo_path: None,
        branch: None,
        commit: None,
        repo,
    };
    let mut components = Vec::new();
    let mut untracked = Vec::new();
    match repo_behind(&cactus_root.join("Makefile"), &repos_dir, repos_real.as_deref()) {
        Some(repo) => components.push(component(ComponentType::Git, root.clone(), "Makefile", repo)),
        None => untracked.push(UNTRACKED_FLESH.to_owned()),
    }
    let arrangements = format!("{root}/arrangements");
    let behind = crate::par::parallel_map(&thorns, |thorn| {
        repo_behind(&cactus_root.join("arrangements").join(thorn), &repos_dir, repos_real.as_deref())
    })?;
    for (thorn, repo) in thorns.iter().zip(behind) {
        match repo {
            Some(repo) => components.push(component(ComponentType::Git, arrangements.clone(), thorn, repo)),
            None => {
                components.push(component(ComponentType::Ignore, arrangements.clone(), thorn, String::new()));
                untracked.push(thorn.clone());
            }
        }
    }
    Ok(Ok(Thornlist::from_plain(text, root, components, untracked)))
}

/// The [`Thornlist::untracked`] entry naming the flesh, when the root
/// `Makefile` is not a link into `repos/`.
pub const UNTRACKED_FLESH: &str = "the Cactus flesh";

/// The `Arrangement/Thorn` lines of a plain Cactus `ThornList`, comments
/// (`#` to end of line, so a `#DISABLED` toggle too) and anything after the
/// first word stripped. The `Err` names the first line that is not one, or
/// says there were none.
fn plain_thorns(text: &str) -> Result<Vec<String>, String> {
    let mut thorns = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("");
        let Some(word) = line.split_whitespace().next() else { continue };
        match word.split_once('/') {
            Some((a, t)) if !a.is_empty() && !t.is_empty() && !t.contains('/') && !a.starts_with('!') => {
                thorns.push(word.to_owned())
            }
            _ => {
                return Err(format!(
                    "its thornlist has no !CRL_VERSION header, and line {} ({:?}) is not of the form \
                     Arrangement/Thorn",
                    n + 1,
                    raw.trim()
                ));
            }
        }
    }
    if thorns.is_empty() {
        return Err("its thornlist names no thorns".to_owned());
    }
    Ok(thorns)
}

/// The repo under `repos_dir` that `path` is a link into: `<repo>` for a link
/// to `repos_dir/<repo>/…`. Read lexically off the link first (cheap, and
/// right for the relative links a fetch makes); failing that — an absolute
/// link through a symlinked directory, or a whole arrangement linked rather
/// than each thorn — through `path` canonicalized against `repos_real`
/// (`repos_dir` canonicalized once by the caller). `None` for anything that
/// does not land in `repos_dir`, including a plain directory.
fn repo_behind(path: &Path, repos_dir: &Path, repos_real: Option<&Path>) -> Option<String> {
    let first = |rest: &Path| rest.components().next()?.as_os_str().to_str().map(str::to_owned);
    if let Ok(target) = std::fs::read_link(path) {
        let joined = path.parent()?.join(target);
        let lexical = lexical_canonicalize(joined.to_str()?);
        let repos = lexical_canonicalize(repos_dir.to_str()?);
        if let Ok(rest) = Path::new(&lexical).strip_prefix(&repos)
            && let Some(repo) = first(rest)
        {
            return Some(repo);
        }
    }
    let real = std::fs::canonicalize(path).ok()?;
    first(real.strip_prefix(repos_real?).ok()?)
}

/// `text` with each repo in `pins` (repo name → full commit id) pinned by a
/// `!REPO_COMMIT` (§3.2), and nothing else changed: the header, comments and
/// every section without a pinned repo stay byte for byte. A section whose
/// components all come from one repo gains the directive just before its
/// `!CHECKOUT` (or has an existing one rewritten). A section that spans
/// several repos — `$1`/`$2` in its `!URL` — is split into one copy per repo,
/// in order of first checkout, each with that repo's checkouts and pin: every
/// other directive is substituted per checkout, so the copies mean exactly
/// what the one section did. The result is parsed back and must name the
/// same checkouts with exactly these pins, or this fails rather than return
/// a list that means something else. A pin for a repo the list does not
/// name is an error too.
pub fn pin_text(text: &str, pins: &std::collections::BTreeMap<String, String>) -> crate::Res<String> {
    use anyhow::{bail, Context};
    let original = parse(text).context("The thornlist does not parse")?;
    for repo in pins.keys() {
        if !original.components().iter().any(|c| &c.repo == repo) {
            bail!("A pin names repo {repo}, which the thornlist does not");
        }
    }
    let target_re = regex::Regex::new(r"(?m)^!TARGET\s*=").expect("valid regex");
    let starts: Vec<usize> = target_re.find_iter(text).map(|m| m.start()).collect();
    let Some(&first) = starts.first() else { return Ok(text.to_owned()) };
    // Every section is parsed alone under the list's header plus all of its
    // !DEFINEs (a define may sit anywhere in the body), to learn its repos.
    let header = &text[..first];
    let defines: String = text
        .lines()
        .filter(|l| l.trim_start().starts_with("!DEFINE"))
        .filter(|l| !header.contains(*l))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut out = String::from(header);
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        let section = &text[start..end];
        let alone = parse(&format!("{header}{defines}\n{section}"))
            .with_context(|| format!("Failed to read the section at byte {start} on its own"))?;
        // (repo, its checkouts) in order of first checkout.
        let mut repos: Vec<(String, Vec<String>)> = Vec::new();
        for c in alone.components() {
            match repos.iter_mut().find(|(r, _)| r == &c.repo) {
                Some((_, checkouts)) => checkouts.push(c.checkout.clone()),
                None => repos.push((c.repo.clone(), vec![c.checkout.clone()])),
            }
        }
        if !repos.iter().any(|(r, _)| pins.contains_key(r)) {
            out.push_str(section);
            continue;
        }
        if let [(repo, _)] = repos.as_slice() {
            out.push_str(&pin_section(section, None, &pins[repo]));
            continue;
        }
        for (n, (repo, checkouts)) in repos.iter().enumerate() {
            let pin = pins.get(repo).map_or("", String::as_str);
            out.push_str(&pin_section(section, Some((checkouts, n == 0, n + 1 == repos.len())), pin));
        }
    }
    // Same checkouts, and exactly the pins asked for.
    let pinned = parse(&out).context("The pinned thornlist does not parse")?;
    let key = |c: &Component| (c.repo.clone(), c.target.clone(), c.checkout.clone());
    let mut want: Vec<_> = original.components().iter().map(key).collect();
    let mut got: Vec<_> = pinned.components().iter().map(key).collect();
    want.sort();
    got.sort();
    if want != got {
        bail!("Pinning changed which checkouts the thornlist names (a bug in pinning this list)");
    }
    for c in pinned.components() {
        let expected = pins.get(&c.repo).or(original.components().iter().find(|o| o.repo == c.repo).and_then(|o| o.commit.as_ref()));
        if c.commit.as_ref() != expected {
            bail!("Pinning left repo {} at {:?} instead of {:?} (a bug in pinning this list)", c.repo, c.commit, expected);
        }
    }
    Ok(out)
}

/// One section, pinned to `commit` (empty: no pin, any existing one dropped)
/// and, with `checkouts`, narrowed to them. Of a split section's checkout
/// lines, the comments among the checkouts (a `#DISABLED` one included) go
/// in the first copy and whatever follows the last checkout (blank lines and
/// comments leading into the next section) after the last, so a split
/// repeats nothing and moves nothing out of place.
fn pin_section(section: &str, checkouts: Option<(&Vec<String>, bool, bool)>, commit: &str) -> String {
    let key_re = regex::Regex::new(r"(?m)^\s*!([^\s=]+)\s*=").expect("valid regex");
    let keys: Vec<(String, usize)> =
        key_re.captures_iter(section).map(|c| (c[1].to_owned(), c.get(0).expect("match").start())).collect();
    let value_end = |i: usize| keys.get(i + 1).map_or(section.len(), |k| k.1);
    let mut out = String::new();
    let mut done = 0;
    for (i, (key, start)) in keys.iter().enumerate() {
        match key.as_str() {
            // Dropped here, written back just before !CHECKOUT.
            "REPO_COMMIT" => {
                out.push_str(&section[done..*start]);
                done = next_line(section, value_end(i));
            }
            "CHECKOUT" => {
                out.push_str(&section[done..*start]);
                if !commit.is_empty() {
                    out.push_str(&format!("!REPO_COMMIT = {commit}\n"));
                }
                match checkouts {
                    None => done = *start,
                    Some((checkouts, first, last)) => {
                        out.push_str(&format!("!CHECKOUT = {}\n", checkouts.join(" ")));
                        let lines: Vec<&str> = section[*start..value_end(i)].split_inclusive('\n').collect();
                        let is_token = |l: &&str| !l.trim().is_empty() && !l.trim_start().starts_with('#');
                        // The `!CHECKOUT` line itself is always a token line
                        // here (or holds none, which changes nothing).
                        let last_token = lines.iter().rposition(|l| is_token(l)).unwrap_or(0);
                        if first {
                            let among = lines[1..=last_token].iter();
                            out.extend(among.filter(|l| l.trim_start().starts_with('#')).copied());
                        }
                        if last {
                            out.extend(lines[last_token + 1..].iter().copied());
                        }
                        done = value_end(i);
                    }
                }
            }
            _ => {}
        }
    }
    out.push_str(&section[done..]);
    out
}

/// The index just past the line `pos` falls in (or the end).
fn next_line(text: &str, pos: usize) -> usize {
    match text[..pos].rfind('\n') {
        Some(nl) if nl + 1 == pos => pos,
        _ => text[pos..].find('\n').map_or(text.len(), |nl| pos + nl + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain Cactus `ThornList` resolves through the tree's links (§7.4):
    /// each linked thorn becomes its repo's component, the flesh comes from
    /// the `Makefile` link, and a thorn that is a real directory is kept for
    /// its provider but named as untracked. A broken CRL list stays an error
    /// rather than being reread as a plain one.
    #[test]
    fn parse_config_list_resolves_a_plain_thornlist_through_the_tree() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let inst = tmp.path();
        let cactus = inst.join("Cactus");
        for dir in ["repos/flesh", "repos/cactusbase/Boundary", "arrangements/CactusBase", "arrangements/Local/Mine"] {
            std::fs::create_dir_all(cactus.join(dir)).unwrap();
        }
        symlink("repos/flesh/Makefile", cactus.join("Makefile")).unwrap();
        symlink("../../repos/cactusbase/Boundary", cactus.join("arrangements/CactusBase/Boundary")).unwrap();

        // `!CRL_VERSION` in a comment does not make this a (broken) CRL list.
        let text = "# converted from a !CRL_VERSION list\nCactusBase/Boundary   # comment\n\n\
                    #DISABLED CactusBase/IOUtil\nLocal/Mine\n";
        let list = parse_config_list(text, inst, &cactus).unwrap().unwrap();
        assert!(list.is_plain());
        assert_eq!(list.root(), "Cactus");
        let git: Vec<(&str, &str)> = list
            .components()
            .iter()
            .filter(|c| c.ty == ComponentType::Git)
            .map(|c| (c.repo.as_str(), c.checkout.as_str()))
            .collect();
        assert_eq!(git, vec![("flesh", "Makefile"), ("cactusbase", "CactusBase/Boundary")]);
        assert_eq!(list.untracked(), ["Local/Mine".to_string()]);
        assert_eq!(list.disabled_thorns(), ["CactusBase/IOUtil".to_string()]);
        let providers = list.thorn_providers();
        assert_eq!(providers["Boundary"], "arrangements/CactusBase/Boundary");
        assert_eq!(providers["Mine"], "arrangements/Local/Mine", "an untracked thorn keeps its provider");

        let broken_crl = "!CRL_VERSION = 1.0\n!TARGET = x\n!TYPE = bogus\n!URL = u\n!CHECKOUT = A/B\n";
        assert!(parse(broken_crl).is_err(), "the fixture must be a broken CRL list");
        let why = parse_config_list(broken_crl, inst, &cactus).unwrap().unwrap_err();
        assert!(why.contains("not a valid GetComponents (CRL) list"), "{why}");
        let indented = parse_config_list("  !CRL_VERSION = 1.0\nA/B\n", inst, &cactus).unwrap().unwrap_err();
        assert!(indented.contains("not a valid GetComponents (CRL) list"), "{indented}");
        // A bare header is still a CRL list, never a plain one.
        assert!(!parse("!CRL_VERSION\n").unwrap().is_plain());
        // A nearly plain list: the reason names the offending line, not the
        // first good one.
        let why = parse_config_list("CactusBase/Boundary\nBoundary\n", inst, &cactus).unwrap().unwrap_err();
        assert!(why.contains("line 2") && why.contains("\"Boundary\""), "{why}");
        assert!(parse_config_list("# nothing\n", inst, &cactus).unwrap().unwrap_err().contains("no thorns"));
    }

    /// The canonicalize fallback: an absolute link, a whole arrangement linked
    /// into `repos/` (thorns real directories inside it), and a flesh whose
    /// `Makefile` is a plain file, which is then named as untracked.
    #[test]
    fn parse_config_list_follows_absolute_and_arrangement_links() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let inst = tmp.path();
        let cactus = inst.join("Cactus");
        for dir in ["repos/carpetx/Loop", "repos/abs/Thorn", "arrangements/Abs"] {
            std::fs::create_dir_all(cactus.join(dir)).unwrap();
        }
        std::fs::write(cactus.join("Makefile"), "all:\n").unwrap();
        symlink("../repos/carpetx", cactus.join("arrangements/CarpetX")).unwrap();
        symlink(cactus.join("repos/abs/Thorn"), cactus.join("arrangements/Abs/Thorn")).unwrap();

        let list = parse_config_list("CarpetX/Loop\nAbs/Thorn\n", inst, &cactus).unwrap().unwrap();
        let git: Vec<&str> =
            list.components().iter().filter(|c| c.ty == ComponentType::Git).map(|c| c.repo.as_str()).collect();
        assert_eq!(git, ["carpetx", "abs"]);
        assert_eq!(list.untracked(), ["the Cactus flesh".to_string()]);
    }


    /// `pin_text`: a one-repo section gains `!REPO_COMMIT` before its
    /// `!CHECKOUT` (or has its old one rewritten); a section spanning repos
    /// through `$2` in its URL is split per repo, keeping its `#DISABLED`
    /// line; everything without a pin stays byte for byte.
    #[test]
    fn pin_text_pins_each_repo_and_leaves_the_rest_alone() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n# the flesh\n\
                    !TARGET = $ROOT\n!TYPE = git\n!URL = https://x/flesh.git\n!REPO_COMMIT = 0000000000000000000000000000000000000000\n!CHECKOUT = Makefile\n\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/$2.git\n!REPO_PATH = ../$2\n\
                    !CHECKOUT =\n  ET/Alpha\n#DISABLED ET/Gamma\n  ET/Beta\n\n\
                    # untouched\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/other.git\n!CHECKOUT = O/One\n";
        let pins: std::collections::BTreeMap<String, String> =
            [("flesh", &a), ("Alpha", &b), ("Beta", &c)].iter().map(|(r, p)| (r.to_string(), p.to_string())).collect();
        let out = pin_text(text, &pins).unwrap();

        let list = parse(&out).unwrap();
        let commit = |repo: &str| list.components().iter().find(|c| c.repo == repo).unwrap().commit.clone();
        assert_eq!(commit("flesh"), Some(a.clone()));
        assert_eq!(commit("Alpha"), Some(b.clone()));
        assert_eq!(commit("Beta"), Some(c.clone()));
        assert_eq!(commit("other"), None);
        assert_eq!(list.disabled_thorns(), ["ET/Gamma".to_string()]);
        assert!(!out.contains("0000000000"), "the old pin is rewritten, not doubled: {out}");
        assert!(out.starts_with("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n# the flesh\n"), "{out}");
        assert!(out.ends_with("# untouched\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/other.git\n!CHECKOUT = O/One\n"), "{out}");
        assert_eq!(out.matches("!URL = https://x/$2.git").count(), 2, "the $2 section is split per repo: {out}");

        // No pins: nothing changes at all.
        assert_eq!(pin_text(text, &Default::default()).unwrap(), text);
        // A pin for a repo the list does not name.
        let stray = [("nope".to_string(), a.clone())].into_iter().collect();
        assert!(format!("{:#}", pin_text(text, &stray).unwrap_err()).contains("nope"));
    }
}
