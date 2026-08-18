//! Arrangement symlinks: materialize `$TARGET/$CHECKOUT` as a relative
//! symlink into `$ROOT/repos/<repo>[/<REPO_PATH>]`, porting `handle_git`'s
//! abs2rel logic with `ln -nsf` repoint semantics.
//! Implemented by the FETCH stream.
//!
//! Ported from `Cactus/bin/GetComponents`'s `handle_git`, the `checkout`
//! method's linking block (lines ~1549-1615), which hg/darcs's own handlers
//! duplicate verbatim (`handle_hg` lines ~2386-2455, `handle_darcs` lines
//! ~2017-2084) — one port here serves all three. The relative-path math
//! (`File::Spec->abs2rel(realpath(...), realpath(...))`, lines ~1564-1565)
//! is [`relative_from`].
//!
//! Deliberate strengthening vs. GetComponents' `ln -nsf`: raw `ln -nsf`
//! unconditionally overwrites whatever is at the link path (as long as
//! `unlink()` on it succeeds — which fails outright on a real directory, but
//! silently replaces *any* symlink, even a hand-made one pointing somewhere
//! unrelated) — and the plain-checkout/`checkout == '.'` branches (lines
//! ~1597, ~1608) skip re-linking entirely whenever *anything* already
//! exists at the link path (`return if (-e ...)`), even a symlink that
//! resolves to the wrong repo. [`link_component`] instead inspects what's
//! there: a correct symlink is left alone ([`LinkOutcome::Unchanged`]), a
//! symlink into `<root>/repos/` pointing at the wrong place is repointed
//! ([`LinkOutcome::Repointed`]), and anything else — a real file/dir, or a
//! symlink pointing outside `<root>/repos/` (hand-placed, or from some other
//! tool) — is left completely untouched and reported
//! ([`LinkOutcome::Blocked`]) for the caller to surface, rather than
//! silently doing nothing (the Perl) or force-clobbering it (raw `ln -nsf`).

use crate::thornlist::Component;
use anyhow::{anyhow, bail, Context};
use std::path::{Component as PathComponent, Path, PathBuf};

/// What [`link_component`] did (or didn't do) to the symlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// No symlink existed at the link path; one was created.
    Created,
    /// A symlink existed, pointing into `<root>/repos/` but at the wrong
    /// place (e.g. the component's `!REPO` changed); it was replaced.
    Repointed { from: PathBuf },
    /// A symlink already existed and already pointed at the right place.
    Unchanged,
    /// The path exists and is NOT a symlink into <root>/repos — never touched.
    Blocked { existing: PathBuf },
}

/// Where a component's arrangement symlink belongs and what it must point at.
/// The pure path half of [`link_component`], factored out so the read-only
/// [`inspect_link`] resolves *exactly* the same path: a reporting view that
/// disagreed with the fetcher about which path on disk is "the thorn's link"
/// would be worse than no view at all.
///
/// Touches the filesystem only to resolve (never to create): `physical_resolve`
/// and `canonicalize` read, and a not-yet-existing tail collapses lexically.
struct LinkPlan {
    /// `<install_root>/<root>/repos`, canonicalized. "Resolves under here" is
    /// the test that separates a cactup-managed link from a hand-made one.
    repos_dir: PathBuf,
    /// The directory the symlink lives in.
    target_dir: PathBuf,
    /// The symlink itself.
    link_path: PathBuf,
    /// The absolute path the symlink must resolve to.
    desired: PathBuf,
}

fn plan_link(install_root: &Path, root: &str, component: &Component) -> crate::Res<LinkPlan> {
    // Canonicalized so the relative-path math and the physically-resolved
    // target dir below live in one namespace even when `install_root` itself
    // contains symlinks.
    let repos_dir = std::fs::canonicalize(install_root.join(root).join("repos"))
        .unwrap_or_else(|_| lexical_normalize(&install_root.join(root).join("repos")));
    let repo_base = repos_dir.join(&component.repo);

    // GetComponents lines 1549-1557: `($checkout_dir, $checkout_item) =
    // split(/\//, $checkout)`, with the no-slash case special-cased.
    let (checkout_dir, checkout_item) = split_checkout_dir_item(&component.checkout);
    // Physically resolved (symlinks followed, `.`/`..` collapsed): a `./Foo`
    // checkout must not leave a trailing `/.` for mkdir, and a target that
    // routes through an existing arrangement symlink must land where the
    // symlink points, not where the text lexically collapses to.
    let target_dir = physical_resolve(&if checkout_dir.is_empty() {
        install_root.join(&component.target)
    } else {
        install_root.join(&component.target).join(&checkout_dir)
    });

    // The three cases, lines 1568-1615 (see the module doc for the ln -nsf
    // divergence in how each result is applied).
    let (link_name, desired_absolute): (String, PathBuf) =
        if let Some(repo_path) = component.repo_path.as_deref() {
            // Lines 1568-1583: !REPO_PATH.
            if repo_path.contains("$1") || repo_path.contains("$2") {
                // Lines 1571-1573: $1/$2 come from the LAST '/' in $checkout,
                // both empty if there is none.
                let (dir1, dir2) = last_slash_split(&component.checkout);
                let substituted = repo_path.replace("$1", &dir1).replace("$2", &dir2);
                (checkout_item.clone(), lexical_normalize(&append_relative(&repo_base, &substituted)))
            } else {
                let joined = format!("{repo_path}/{}", component.checkout);
                (checkout_item.clone(), lexical_normalize(&append_relative(&repo_base, &joined)))
            }
        } else if component.checkout == "." {
            // Lines 1593-1604: the repo IS the target — link the whole repo
            // dir (no subpath) under !NAME.
            let name = component.name.as_deref().ok_or_else(|| {
                anyhow!(
                    "component '{}' has !CHECKOUT = . but no !NAME to link it as \
                     (GetComponents line 1596 uses !NAME as the symlink's basename here)",
                    component.checkout
                )
            })?;
            (name.to_owned(), repo_base.clone())
        } else {
            // Lines 1605-1615: plain checkout.
            (checkout_item.clone(), lexical_normalize(&append_relative(&repo_base, &component.checkout)))
        };

    if link_name.is_empty() {
        bail!("component '{}' resolves to an empty symlink name", component.checkout);
    }

    Ok(LinkPlan {
        link_path: target_dir.join(&link_name),
        repos_dir,
        target_dir,
        desired: desired_absolute,
    })
}

/// Materialize the arrangement symlink for `component` under `install_root`,
/// given the thornlist's `!DEFINE ROOT` value (`Thornlist::root()`). Creates
/// parent directories as needed; never touches a pre-existing non-symlink
/// path or a symlink that doesn't already point into `<root>/repos/`.
pub fn link_component(install_root: &Path, root: &str, component: &Component) -> crate::Res<LinkOutcome> {
    let plan = plan_link(install_root, root, component)?;
    std::fs::create_dir_all(&plan.target_dir)
        .with_context(|| format!("Failed to create {}", plan.target_dir.display()))?;

    match std::fs::symlink_metadata(&plan.link_path) {
        Err(_) => {
            create_symlink(&plan.target_dir, &plan.desired, &plan.link_path)?;
            Ok(LinkOutcome::Created)
        }
        Ok(meta) => {
            if !meta.file_type().is_symlink() {
                return Ok(LinkOutcome::Blocked { existing: plan.link_path });
            }
            let raw = std::fs::read_link(&plan.link_path).with_context(|| {
                format!("Failed to read existing symlink {}", plan.link_path.display())
            })?;
            let resolved_old = join_normalized(&plan.target_dir, &raw);
            if !resolved_old.starts_with(&plan.repos_dir) {
                return Ok(LinkOutcome::Blocked { existing: plan.link_path });
            }
            if resolved_old == plan.desired {
                return Ok(LinkOutcome::Unchanged);
            }
            remove_symlink(&plan.link_path)?;
            create_symlink(&plan.target_dir, &plan.desired, &plan.link_path)?;
            Ok(LinkOutcome::Repointed { from: resolved_old })
        }
    }
}

/// What is at a component's arrangement link path *right now* — the read-only
/// counterpart to [`LinkOutcome`], for `cactup installation delta`.
///
/// [`LinkOutcome::Blocked`] deliberately collapses two situations the fetcher
/// treats identically (both mean "not mine, don't touch"). A report must not:
/// a hand-placed thorn directory and a foreign symlink call for different
/// responses from the user, so they are separate here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// A symlink into `<root>/repos/`, pointing exactly where the thornlist
    /// says, with the thorn source it names present.
    Linked,
    /// Pointed correctly, but nothing is at the other end — the thorn source
    /// the thornlist names is not in the repo (a `!CHECKOUT` path that moved
    /// upstream, or a half-removed repo).
    Dangling { target: PathBuf },
    /// Nothing exists at the link path at all: the thornlist names this thorn
    /// and nothing links it into the build.
    Missing,
    /// A symlink into `<root>/repos/`, but at the wrong thorn — a refetch
    /// would repoint it ([`LinkOutcome::Repointed`]).
    Misdirected { target: PathBuf },
    /// A real file or directory sits where the symlink belongs: a hand-placed
    /// thorn, standing in for the checkout. cactup will never overwrite it,
    /// and the build compiles it — so saying nothing about it is how a tree
    /// silently stops matching its thornlist.
    Replaced { existing: PathBuf },
    /// A symlink pointing outside `<root>/repos/` — hand-made, or another
    /// tool's. Left untouched by a fetch, exactly like `Replaced`.
    Foreign { target: PathBuf },
}

impl LinkState {
    /// Whether this is worth telling the user about; `Linked` is not.
    pub fn is_divergence(&self) -> bool {
        !matches!(self, LinkState::Linked)
    }
}

/// Classify `component`'s arrangement link without touching anything —
/// no `create_dir_all`, no repointing. Resolves the same path
/// [`link_component`] would act on, by construction (both go through
/// [`plan_link`]).
pub fn inspect_link(install_root: &Path, root: &str, component: &Component) -> crate::Res<LinkState> {
    let plan = plan_link(install_root, root, component)?;
    let Ok(meta) = std::fs::symlink_metadata(&plan.link_path) else {
        return Ok(LinkState::Missing);
    };
    if !meta.file_type().is_symlink() {
        return Ok(LinkState::Replaced { existing: plan.link_path });
    }
    let raw = std::fs::read_link(&plan.link_path)
        .with_context(|| format!("Failed to read symlink {}", plan.link_path.display()))?;
    let resolved = join_normalized(&plan.target_dir, &raw);
    if !resolved.starts_with(&plan.repos_dir) {
        return Ok(LinkState::Foreign { target: resolved });
    }
    if resolved != plan.desired {
        return Ok(LinkState::Misdirected { target: resolved });
    }
    // `exists()` follows the link, which is the question being asked here.
    if !plan.link_path.exists() {
        return Ok(LinkState::Dangling { target: plan.desired });
    }
    Ok(LinkState::Linked)
}

fn create_symlink(link_dir: &Path, absolute_target: &Path, link_path: &Path) -> crate::Res<()> {
    let rel = relative_from(absolute_target, link_dir);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&rel, link_path)
        .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), rel.display()))?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&rel, link_path)
        .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), rel.display()))?;
    Ok(())
}

fn remove_symlink(link_path: &Path) -> crate::Res<()> {
    #[cfg(unix)]
    std::fs::remove_file(link_path)
        .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
    #[cfg(windows)]
    std::fs::remove_dir(link_path) // dir symlinks on Windows are removed with remove_dir
        .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
    Ok(())
}

/// GetComponents lines 1549-1557: `($checkout_dir, $checkout_item) =
/// split(/\//, $checkout)`, i.e. split at the FIRST '/' (a `split` on every
/// '/' assigned into just two variables silently discards any segment past
/// the second — not exercised by any real checkout, which are always
/// exactly two levels, `Arrangement/Thorn`). No slash at all: `checkout_dir`
/// is `''` and `checkout_item` is the whole token (the explicit `unless`
/// branch, lines 1550-1557).
fn split_checkout_dir_item(checkout: &str) -> (String, String) {
    match checkout.split_once('/') {
        Some((dir, rest)) => {
            let item = rest.split_once('/').map(|(item, _)| item).unwrap_or(rest);
            (dir.to_owned(), item.to_owned())
        }
        None => (String::new(), checkout.to_owned()),
    }
}

/// GetComponents line 1571: `my ($dir1, $dir2) = $checkout =~ m!(.*)/(.*)!`
/// — a greedy match on the LAST '/'; both capture groups are undef (Perl
/// interpolates as `''`) when `$checkout` has no '/' at all, since the whole
/// regex then fails to match.
fn last_slash_split(checkout: &str) -> (String, String) {
    match checkout.rfind('/') {
        Some(idx) => (checkout[..idx].to_owned(), checkout[idx + 1..].to_owned()),
        None => (String::new(), String::new()),
    }
}

/// Append `rel` onto `base` treating every `/`-separated segment as a plain
/// path component, exactly like GetComponents' string concatenation
/// (`"$git_repos_dir/$git_repo/$repo_path"` etc.) — unlike [`Path::join`],
/// a `rel` that happens to look absolute (a leading `/`) is never treated as
/// replacing `base` wholesale.
fn append_relative(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for seg in rel.split('/') {
        if !seg.is_empty() {
            out.push(seg);
        }
    }
    out
}

/// Lexically collapse `.`/`..` components — no filesystem access, no
/// `canonicalize`. `..` past anything poppable (the start of a relative
/// path, or a root/prefix) is kept as a leading `..`, never dropped.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<PathComponent> = Vec::new();
    for comp in path.components() {
        match comp {
            PathComponent::CurDir => {}
            PathComponent::ParentDir => match out.last() {
                Some(PathComponent::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    out.into_iter().collect()
}

/// Resolve `path` the way the filesystem would: canonicalize the longest
/// *existing* prefix (following symlinks physically — this is what makes the
/// ET list's "crazy path" Fuka target, which deliberately routes through the
/// `arrangements/Fuka/KadathThorn` symlink with `..` segments, land inside
/// `repos/KadathThorn` the way GetComponents' `realpath` did), then append
/// the not-yet-existing remainder and collapse it lexically.
fn physical_resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            PathComponent::CurDir => {}
            // `out` is physical up to here (existing prefixes are snapped to
            // their canonical identity below), so popping IS the kernel's
            // `..` semantics; on a not-yet-existing tail it is the lexical
            // collapse, which is all that's left to do.
            PathComponent::ParentDir => {
                out.pop();
            }
            other => {
                out.push(other);
                if let Ok(canonical) = std::fs::canonicalize(&out) {
                    out = canonical;
                }
            }
        }
    }
    out
}

/// `base.join(rel)`, then lexically normalized — except when `rel` is
/// itself absolute, in which case it replaces `base` outright (this is only
/// used to resolve a symlink's stored target, which — unlike `!REPO_PATH` —
/// genuinely may be absolute if hand-created).
fn join_normalized(base: &Path, rel: &Path) -> PathBuf {
    if rel.is_absolute() { lexical_normalize(rel) } else { lexical_normalize(&base.join(rel)) }
}

/// Pure, lexical port of `File::Spec->abs2rel(realpath($target),
/// realpath($link_dir))` (GetComponents lines 1564-1565): the relative path
/// from `link_dir` (the directory the symlink will live in) to `target` (the
/// absolute path it should point at), built from a common-prefix diff with
/// `..` components — no `canonicalize`, no filesystem access. Both inputs
/// are assumed already absolute-style and free of `.`/`..` (callers
/// lexically normalize first).
fn relative_from(target: &Path, link_dir: &Path) -> PathBuf {
    let target_comps: Vec<_> = target.components().collect();
    let link_comps: Vec<_> = link_dir.components().collect();
    let common = target_comps.iter().zip(link_comps.iter()).take_while(|(a, b)| a == b).count();

    let mut result = PathBuf::new();
    for _ in common..link_comps.len() {
        result.push("..");
    }
    for comp in &target_comps[common..] {
        result.push(comp.as_os_str());
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thornlist::ComponentType;

    fn git_component(checkout: &str, repo: &str) -> Component {
        Component {
            ty: ComponentType::Git,
            target: "Cactus/arrangements".to_owned(),
            checkout: checkout.to_owned(),
            name: None,
            url: Some("https://example.com/repo.git".to_owned()),
            auth_url: None,
            anon_user: None,
            anon_pass: None,
            repo_path: None,
            branch: None,
            repo: repo.to_owned(),
        }
    }

    // --- relative_from ---

    #[test]
    fn relative_from_sibling() {
        assert_eq!(relative_from(Path::new("/a/repos"), Path::new("/a/target")), PathBuf::from("../repos"));
    }

    #[test]
    fn relative_from_deeper() {
        assert_eq!(
            relative_from(Path::new("/a/repos"), Path::new("/a/target/checkout")),
            PathBuf::from("../../repos")
        );
    }

    #[test]
    fn relative_from_shallower() {
        assert_eq!(relative_from(Path::new("/a/repos"), Path::new("/a")), PathBuf::from("repos"));
    }

    #[test]
    fn relative_from_same_dir_is_dot() {
        assert_eq!(relative_from(Path::new("/a/b"), Path::new("/a/b")), PathBuf::from("."));
    }

    // --- split_checkout_dir_item / last_slash_split ---

    #[test]
    fn split_checkout_dir_item_two_levels() {
        assert_eq!(split_checkout_dir_item("McLachlan/ML_BSSN"), ("McLachlan".to_owned(), "ML_BSSN".to_owned()));
    }

    #[test]
    fn split_checkout_dir_item_no_slash() {
        assert_eq!(split_checkout_dir_item("flat"), (String::new(), "flat".to_owned()));
    }

    #[test]
    fn split_checkout_dir_item_truncates_third_level() {
        // Matches Perl's list-assignment truncation (see the doc comment).
        assert_eq!(split_checkout_dir_item("a/b/c"), ("a".to_owned(), "b".to_owned()));
    }

    #[test]
    fn last_slash_split_uses_last_slash() {
        assert_eq!(last_slash_split("McLachlan/ML_BSSN"), ("McLachlan".to_owned(), "ML_BSSN".to_owned()));
        assert_eq!(last_slash_split("a/b/c"), ("a/b".to_owned(), "c".to_owned()));
        assert_eq!(last_slash_split("flat"), (String::new(), String::new()));
    }

    // --- end-to-end with a tempdir ---

    struct Fixture {
        _dir: tempfile::TempDir,
        install_root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let install_root = dir.path().to_path_buf();
        Fixture { _dir: dir, install_root }
    }

    fn make_repo(install_root: &Path, repo: &str) {
        std::fs::create_dir_all(install_root.join("Cactus/repos").join(repo)).unwrap();
    }

    #[test]
    fn creates_a_relative_symlink() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        std::fs::create_dir_all(f.install_root.join("Cactus/repos/Foo/McLachlan/ML_BSSN")).unwrap();
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        assert_eq!(outcome, LinkOutcome::Created);

        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");
        let raw = std::fs::read_link(&link_path).unwrap();
        assert!(!raw.is_absolute(), "symlink target should be relative: {raw:?}");
        assert_eq!(raw, PathBuf::from("../../repos/Foo/McLachlan/ML_BSSN"));

        // The link resolves to the repo dir we made.
        let resolved = link_path.parent().unwrap().join(&raw);
        assert!(resolved.exists());
    }

    /// `inspect_link` must agree with `link_component` about which path is
    /// the thorn's link, and must classify every way that path can go wrong.
    /// The case that motivated it: a hand-placed thorn directory standing in
    /// for the checkout is invisible to a per-repo status walk, because the
    /// divergence is not inside any repo.
    #[test]
    fn inspect_link_classifies_every_way_a_link_can_diverge() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        make_repo(&f.install_root, "Bar");
        let thorn = f.install_root.join("Cactus/repos/Foo/McLachlan/ML_BSSN");
        std::fs::create_dir_all(&thorn).unwrap();
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");

        // Nothing there yet.
        assert_eq!(inspect_link(&f.install_root, "Cactus", &c).unwrap(), LinkState::Missing);

        // What the fetcher creates must read back as sound — the two halves
        // going through `plan_link` is what guarantees it.
        assert_eq!(link_component(&f.install_root, "Cactus", &c).unwrap(), LinkOutcome::Created);
        assert_eq!(inspect_link(&f.install_root, "Cactus", &c).unwrap(), LinkState::Linked);

        // Correctly pointed, but the thorn source is gone from the repo.
        std::fs::remove_dir_all(&thorn).unwrap();
        match inspect_link(&f.install_root, "Cactus", &c).unwrap() {
            LinkState::Dangling { target } => assert!(target.ends_with("Foo/McLachlan/ML_BSSN")),
            other => panic!("expected Dangling, got {other:?}"),
        }
        std::fs::create_dir_all(&thorn).unwrap();

        // Into repos/, but at the wrong thorn: a refetch repoints this one.
        std::fs::remove_file(&link_path).unwrap();
        std::os::unix::fs::symlink("../../repos/Bar/McLachlan/ML_BSSN", &link_path).unwrap();
        match inspect_link(&f.install_root, "Cactus", &c).unwrap() {
            LinkState::Misdirected { target } => assert!(target.ends_with("Bar/McLachlan/ML_BSSN")),
            other => panic!("expected Misdirected, got {other:?}"),
        }

        // Outside repos/ entirely. `link_component` calls this Blocked and
        // leaves it alone; the report must distinguish it from a hand-placed
        // directory, since the two call for different responses.
        std::fs::remove_file(&link_path).unwrap();
        let elsewhere = f.install_root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link_path).unwrap();
        match inspect_link(&f.install_root, "Cactus", &c).unwrap() {
            LinkState::Foreign { target } => assert!(target.ends_with("elsewhere"), "{target:?}"),
            other => panic!("expected Foreign, got {other:?}"),
        }

        // The reported case: a real directory of someone's own source. cactup
        // never replaces it, and the build compiles it as-is.
        std::fs::remove_file(&link_path).unwrap();
        std::fs::create_dir_all(link_path.join("src")).unwrap();
        match inspect_link(&f.install_root, "Cactus", &c).unwrap() {
            LinkState::Replaced { existing } => assert_eq!(existing, link_path),
            other => panic!("expected Replaced, got {other:?}"),
        }
        // …and `link_component` agrees it is not to be touched.
        assert!(matches!(
            link_component(&f.install_root, "Cactus", &c).unwrap(),
            LinkOutcome::Blocked { .. }
        ));
        assert!(link_path.join("src").is_dir(), "inspection and linking both leave it alone");
    }

    /// Inspection is read-only: it must not create the arrangement directory
    /// the way `link_component` does, or merely *looking* at an unfetched tree
    /// would start building one.
    #[test]
    fn inspect_link_creates_nothing() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        assert_eq!(inspect_link(&f.install_root, "Cactus", &c).unwrap(), LinkState::Missing);
        assert!(
            !f.install_root.join("Cactus/arrangements").exists(),
            "inspection must not materialize the target directory"
        );
    }

    #[test]
    fn relinking_is_unchanged() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        assert_eq!(link_component(&f.install_root, "Cactus", &c).unwrap(), LinkOutcome::Created);
        assert_eq!(link_component(&f.install_root, "Cactus", &c).unwrap(), LinkOutcome::Unchanged);
    }

    #[test]
    fn repoints_when_repo_name_changes() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        make_repo(&f.install_root, "Bar");
        let mut c = git_component("McLachlan/ML_BSSN", "Foo");
        assert_eq!(link_component(&f.install_root, "Cactus", &c).unwrap(), LinkOutcome::Created);

        c.repo = "Bar".to_owned();
        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        match outcome {
            LinkOutcome::Repointed { from } => {
                assert!(from.ends_with("Foo/McLachlan/ML_BSSN"), "{from:?}");
            }
            other => panic!("expected Repointed, got {other:?}"),
        }
        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");
        let raw = std::fs::read_link(&link_path).unwrap();
        assert_eq!(raw, PathBuf::from("../../repos/Bar/McLachlan/ML_BSSN"));
    }

    #[test]
    fn a_real_directory_blocks_and_is_untouched() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");
        std::fs::create_dir_all(&link_path).unwrap();
        std::fs::write(link_path.join("sentinel"), b"keep me").unwrap();

        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        assert_eq!(outcome, LinkOutcome::Blocked { existing: link_path.clone() });
        // Untouched: still a real directory with our sentinel file in it.
        assert!(link_path.join("sentinel").exists());
        assert!(!std::fs::symlink_metadata(&link_path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn a_symlink_outside_repos_blocks_and_is_untouched() {
        let f = fixture();
        make_repo(&f.install_root, "Foo");
        let c = git_component("McLachlan/ML_BSSN", "Foo");
        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");
        std::fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        let elsewhere = f.install_root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link_path).unwrap();

        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        assert_eq!(outcome, LinkOutcome::Blocked { existing: link_path.clone() });
        let raw = std::fs::read_link(&link_path).unwrap();
        assert_eq!(raw, elsewhere);
    }

    #[test]
    fn repo_path_with_dollar_1_and_dollar_2() {
        let f = fixture();
        make_repo(&f.install_root, "cactusbase");
        let mut c = git_component("McLachlan/ML_BSSN", "cactusbase");
        c.repo_path = Some("arr/$1/$2".to_owned());
        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        assert_eq!(outcome, LinkOutcome::Created);

        let link_path = f.install_root.join("Cactus/arrangements/McLachlan/ML_BSSN");
        let raw = std::fs::read_link(&link_path).unwrap();
        assert_eq!(raw, PathBuf::from("../../repos/cactusbase/arr/McLachlan/ML_BSSN"));
    }

    #[test]
    fn checkout_dot_links_whole_repo_under_name() {
        let f = fixture();
        make_repo(&f.install_root, "Whole");
        let mut c = git_component(".", "Whole");
        c.name = Some("WholeRepo".to_owned());
        let outcome = link_component(&f.install_root, "Cactus", &c).unwrap();
        assert_eq!(outcome, LinkOutcome::Created);

        let link_path = f.install_root.join("Cactus/arrangements/WholeRepo");
        let raw = std::fs::read_link(&link_path).unwrap();
        assert_eq!(raw, PathBuf::from("../repos/Whole"));
    }

    #[test]
    fn checkout_dot_without_name_is_an_error() {
        let f = fixture();
        make_repo(&f.install_root, "Whole");
        let c = git_component(".", "Whole");
        let err = link_component(&f.install_root, "Cactus", &c).unwrap_err();
        assert!(format!("{err:#}").contains("!NAME"));
    }
}
