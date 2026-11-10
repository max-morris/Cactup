//! Thornlists. The CRL 1.0 parser itself (spec §3.2) — a port of
//! GetComponents' `parse_list` plus cactup's extensions (`#DISABLED`,
//! `!REPO_COMMIT`), and `pin_text`, which writes pins into a list — live in
//! the Cactup ecosystem's shared crate, where the Cactarium registry parses
//! and pins lists by exactly the same rules; they are re-exported here.
//! What stays is what only an installed tree can answer:
//! [`parse_config_list`], which resolves a plain Cactus `ThornList` through
//! the tree's arrangement links.

pub use cactup_ecosystem_common::crl::{parse, parse_with_base, pin_text, Component, ComponentType, Thornlist};

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
        fetch_only: false,
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
}
