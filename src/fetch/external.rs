//! svn/cvs/hg/darcs components via the system tools, with a clear error
//! naming a missing binary and the components that need it.
//! Implemented by the FETCH stream.
//!
//! Ported from `Cactus/bin/GetComponents`'s `handle_svn` (lines ~1277-1466),
//! `handle_cvs` (lines ~1123-1275), `handle_hg` (lines ~2265-2545), and
//! `handle_darcs` (lines ~1944-2163). All four types are rare in practice —
//! per the porting brief, "these types don't occur in the real ET
//! thornlist" — so this module keeps only the portable, load-bearing
//! behavior and treats the exotic flags/workarounds as out of scope:
//!
//! * `!AUTH_URL` (svn/cvs/hg/darcs) and svn/cvs's `!ANON_USER`/`!ANON_PASS`
//!   credential embedding are ported, since both are plain, portable string
//!   substitutions with a direct home in [`crate::thornlist::Component`].
//! * GetComponents' interactive `$component{USER}` login-prompt flow (svn's
//!   `--username`, cvs' `:pserver:$user@` with a *separately* prompted
//!   password), its `--date`/`-D`/`-r {DATE}` time-travel checkouts, the
//!   `svn.cct.lsu.edu` TLS cert-trust hack, and cvs' tmp-dir-then-`mv`
//!   checkout dance (a workaround for a `cvs checkout -d` quirk) are not —
//!   cactup has no equivalent CLI flag, interactive prompt, or that specific
//!   quirk (we pass `cvs checkout -d` directly; see [`cvs_checkout_args`]).
//!
//! Deliberate simplification vs. GetComponents' hg/darcs handling: this
//! module fetches hg/darcs straight into `<target>/<name-or-checkout>`, the
//! same destination svn/cvs use, rather than into a shared
//! `<install_root>/<root>/repos/<repo>` mirror with an arrangement symlink
//! (what `handle_hg`/`handle_darcs` do in the Perl, mirroring `handle_git` —
//! see `link.rs`, which still ports that shared-mirror indirection for git).
//! The shared mirror only pays for itself when many checkouts share one
//! repo, which none of these four types ever do in the real Einstein
//! Toolkit list, and `fetch_external`'s signature has no `root` to place a
//! mirror under. Whether hg/darcs should instead be routed through
//! `link.rs` (as `fetch/mod.rs`'s current `Plan.links` grouping suggests) is
//! a decision for whoever writes the executor that calls this module.

use crate::thornlist::{Component, ComponentType};
use anyhow::{anyhow, bail, Context};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Fetch one svn/cvs/hg/darcs component: a fresh checkout/clone if its
/// destination doesn't exist yet, else an in-place update/pull.
pub fn fetch_external(install_root: &Path, component: &Component) -> crate::Res<()> {
    match component.ty {
        ComponentType::Svn => fetch_svn(install_root, component),
        ComponentType::Cvs => fetch_cvs(install_root, component),
        ComponentType::Hg => fetch_hg(install_root, component),
        ComponentType::Darcs => fetch_darcs(install_root, component),
        other => {
            unreachable!("fetch_external called with non-external component type {other:?} (caller bug)")
        }
    }
}

/// `<install_root>/<target>/<name-or-checkout>` — the destination every
/// handler here fetches into (see the module doc's note on hg/darcs).
fn destination(install_root: &Path, component: &Component) -> PathBuf {
    let dir_name = component.name.as_deref().unwrap_or(&component.checkout);
    install_root.join(&component.target).join(dir_name)
}

fn require_url(component: &Component) -> crate::Res<&str> {
    component.url.as_deref().ok_or_else(|| anyhow!("component '{}' has no !URL", component.checkout))
}

// --- svn: GetComponents handle_svn, checkout lines 1309-1336, update lines 1338-1402 ---

/// `!AUTH_URL` wins over `!URL` (lines 1291-1293). No portable equivalent of
/// the interactive `$component{USER}` (line 1284) or the LSU cert-trust hack
/// (lines 1304-1307) exists here.
fn svn_url(component: &Component) -> crate::Res<&str> {
    if let Some(auth) = &component.auth_url { Ok(auth) } else { require_url(component) }
}

fn svn_checkout_args(url: &str, dest: &Path) -> (&'static str, Vec<String>) {
    ("svn", vec!["checkout".into(), "--non-interactive".into(), url.into(), dest.display().to_string()])
}

fn svn_update_args() -> (&'static str, Vec<String>) {
    ("svn", vec!["update".into(), "--non-interactive".into()])
}

fn fetch_svn(install_root: &Path, component: &Component) -> crate::Res<()> {
    let dest = destination(install_root, component);
    if dest.exists() {
        let (prog, args) = svn_update_args();
        run_tool(prog, &args, Some(&dest), component)
    } else {
        let url = svn_url(component)?.to_owned();
        let target_dir = install_root.join(&component.target);
        std::fs::create_dir_all(&target_dir)
            .with_context(|| format!("Failed to create {}", target_dir.display()))?;
        let (prog, args) = svn_checkout_args(&url, &dest);
        run_tool(prog, &args, Some(&target_dir), component)
    }
}

// --- cvs: GetComponents handle_cvs, checkout lines 1166-1200, update lines 1202-1218 ---

/// `!AUTH_URL` wins (embeds no username — cactup has no `$component{USER}`
/// equivalent, lines 1136-1148); else `!ANON_USER`/`!ANON_PASS` are embedded
/// into the URL exactly as GetComponents does (lines 1149-1159); else the
/// bare `!URL` (external auth, e.g. `:ext:`, lines 1160-1164).
fn cvs_url(component: &Component) -> crate::Res<String> {
    if let Some(auth) = &component.auth_url {
        return Ok(auth.clone());
    }
    if let (Some(user), Some(pass)) = (&component.anon_user, &component.anon_pass) {
        let url = require_url(component)?;
        return Ok(if let Some(rest) = url.strip_prefix(":pserver:") {
            format!(":pserver:{user}:{pass}@{rest}")
        } else {
            format!("{user}:{pass}@{url}")
        });
    }
    require_url(component).map(|u| u.to_owned())
}

/// `cvs -q -d $url checkout -r $branch -d $dir_name $checkout` — a direct
/// `cvs checkout -d` instead of GetComponents' tmp-dir-then-`mv` dance
/// (lines 1178-1183), which exists there to dodge a `cvs`/pre-existing-`CVS`
/// directory quirk that a direct `-d` can also hit in principle; not
/// replicated (see the module doc).
fn cvs_checkout_args(url: &str, branch: Option<&str>, dir_name: &str, checkout: &str) -> (&'static str, Vec<String>) {
    let mut args = vec!["-q".into(), "-d".into(), url.into(), "checkout".into()];
    if let Some(b) = branch {
        args.push("-r".into());
        args.push(b.into());
    }
    args.push("-d".into());
    args.push(dir_name.into());
    args.push(checkout.into());
    ("cvs", args)
}

/// `cvs -q update -dP -r $branch` (line 1205; no `-d $url` here — cvs reads
/// `CVS/Root` recorded at checkout time, same as the Perl).
fn cvs_update_args(branch: Option<&str>) -> (&'static str, Vec<String>) {
    let mut args = vec!["-q".into(), "update".into(), "-dP".into()];
    if let Some(b) = branch {
        args.push("-r".into());
        args.push(b.into());
    }
    ("cvs", args)
}

fn fetch_cvs(install_root: &Path, component: &Component) -> crate::Res<()> {
    let dest = destination(install_root, component);
    if dest.exists() {
        let (prog, args) = cvs_update_args(component.branch.as_deref());
        run_tool(prog, &args, Some(&dest), component)
    } else {
        let url = cvs_url(component)?;
        let target_dir = install_root.join(&component.target);
        std::fs::create_dir_all(&target_dir)
            .with_context(|| format!("Failed to create {}", target_dir.display()))?;
        let dir_name = component.name.as_deref().unwrap_or(&component.checkout);
        let (prog, args) = cvs_checkout_args(&url, component.branch.as_deref(), dir_name, &component.checkout);
        run_tool(prog, &args, Some(&target_dir), component)
    }
}

// --- hg: GetComponents handle_hg, checkout lines 2285-2361, update lines 2458-2487 ---

/// `!AUTH_URL` wins over `!URL` (lines 2270-2272).
fn hg_url(component: &Component) -> crate::Res<&str> {
    if let Some(auth) = &component.auth_url { Ok(auth) } else { require_url(component) }
}

/// `hg clone -u $branch $url $dest` — `!REPO_BRANCH`, if present, updates to
/// that branch/rev as part of the clone via `-u`, a single-command
/// replacement for GetComponents' separate post-clone `hg checkout $branch`
/// (lines 2309-2333, including its failure-quarantine `mv` to
/// `$repo_loc.branch.failed`, not replicated here).
fn hg_clone_args(url: &str, branch: Option<&str>, dest: &Path) -> (&'static str, Vec<String>) {
    let mut args = vec!["clone".into()];
    if let Some(b) = branch {
        args.push("-u".into());
        args.push(b.into());
    }
    args.push(url.into());
    args.push(dest.display().to_string());
    ("hg", args)
}

/// `hg pull -u` (brief: "hg: clone vs pull -u"), a single-command
/// replacement for GetComponents' separate `hg pull` (line 2368) done at
/// clone-reuse time.
fn hg_pull_args() -> (&'static str, Vec<String>) {
    ("hg", vec!["pull".into(), "-u".into()])
}

fn fetch_hg(install_root: &Path, component: &Component) -> crate::Res<()> {
    let dest = destination(install_root, component);
    if dest.exists() {
        let (prog, args) = hg_pull_args();
        run_tool(prog, &args, Some(&dest), component)
    } else {
        let url = hg_url(component)?.to_owned();
        let target_dir = install_root.join(&component.target);
        std::fs::create_dir_all(&target_dir)
            .with_context(|| format!("Failed to create {}", target_dir.display()))?;
        let (prog, args) = hg_clone_args(&url, component.branch.as_deref(), &dest);
        run_tool(prog, &args, Some(&target_dir), component)
    }
}

// --- darcs: GetComponents handle_darcs, checkout lines 1967-2085, update lines 2087-2116 ---

/// `!AUTH_URL` wins over `!URL` (lines 1949-1951).
fn darcs_url(component: &Component) -> crate::Res<&str> {
    if let Some(auth) = &component.auth_url { Ok(auth) } else { require_url(component) }
}

/// `darcs clone -t $branch $url $dest` (brief: "darcs: clone vs pull -a";
/// GetComponents itself uses the older `darcs get`, line 1978). `!REPO_BRANCH`
/// becomes darcs' `-t` (tag-match) filter, the same flag GetComponents'
/// `$tag` applies both here and to `pull` (lines 1960-1961).
fn darcs_clone_args(url: &str, branch: Option<&str>, dest: &Path) -> (&'static str, Vec<String>) {
    let mut args = vec!["clone".into()];
    if let Some(b) = branch {
        args.push("-t".into());
        args.push(b.into());
    }
    args.push(url.into());
    args.push(dest.display().to_string());
    ("darcs", args)
}

/// `darcs pull -a -t $branch` — `-a` (`--all`) makes the pull
/// non-interactive, matching GetComponents' own non-interactive `darcs pull`
/// invocation (line 1999) which relies on there being no conflicting local
/// changes to prompt about (the same assumption a clean-repo-only fetcher
/// makes elsewhere in cactup).
fn darcs_pull_args(branch: Option<&str>) -> (&'static str, Vec<String>) {
    let mut args = vec!["pull".into(), "-a".into()];
    if let Some(b) = branch {
        args.push("-t".into());
        args.push(b.into());
    }
    ("darcs", args)
}

fn fetch_darcs(install_root: &Path, component: &Component) -> crate::Res<()> {
    let dest = destination(install_root, component);
    if dest.exists() {
        let (prog, args) = darcs_pull_args(component.branch.as_deref());
        run_tool(prog, &args, Some(&dest), component)
    } else {
        let url = darcs_url(component)?.to_owned();
        let target_dir = install_root.join(&component.target);
        std::fs::create_dir_all(&target_dir)
            .with_context(|| format!("Failed to create {}", target_dir.display()))?;
        let (prog, args) = darcs_clone_args(&url, component.branch.as_deref(), &dest);
        run_tool(prog, &args, Some(&target_dir), component)
    }
}

/// Spawn `program` with `args` (cwd `cwd`, if given), tracing it first (house
/// style — see `crate::shell::trace_command`). A missing binary and a
/// non-zero exit are both reported with the tool name and `component`'s
/// checkout name, so the error is actionable without re-running with
/// `--trace`.
fn run_tool(program: &str, args: &[String], cwd: Option<&Path>, component: &Component) -> crate::Res<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    crate::shell::trace_command(&cmd);
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!(
            "{program} exited with {} while fetching '{}': {}",
            out.status,
            component.checkout,
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "{program} is not installed, but is needed to fetch '{}' — install {program} and try again",
            component.checkout,
        ),
        Err(e) => Err(e).with_context(|| format!("Failed to run {program} for '{}'", component.checkout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(ty: ComponentType, checkout: &str) -> Component {
        Component {
            ty,
            target: "Cactus/repos".to_owned(),
            checkout: checkout.to_owned(),
            name: None,
            url: Some("https://example.com/repo".to_owned()),
            auth_url: None,
            anon_user: None,
            anon_pass: None,
            repo_path: None,
            branch: None,
            repo: "repo".to_owned(),
        }
    }

    #[test]
    fn svn_checkout_args_shape() {
        let (prog, args) = svn_checkout_args("https://example.com/repo", Path::new("/tmp/dest"));
        assert_eq!(prog, "svn");
        assert_eq!(args, vec!["checkout", "--non-interactive", "https://example.com/repo", "/tmp/dest"]);
    }

    #[test]
    fn svn_update_args_shape() {
        let (prog, args) = svn_update_args();
        assert_eq!(prog, "svn");
        assert_eq!(args, vec!["update", "--non-interactive"]);
    }

    #[test]
    fn cvs_url_prefers_auth_url() {
        let mut c = component(ComponentType::Cvs, "x");
        c.auth_url = Some(":ext:me@example.com:/cvsroot".to_owned());
        c.anon_user = Some("anon".to_owned());
        c.anon_pass = Some("anon".to_owned());
        assert_eq!(cvs_url(&c).unwrap(), ":ext:me@example.com:/cvsroot");
    }

    #[test]
    fn cvs_url_embeds_anon_creds_into_pserver_url() {
        let mut c = component(ComponentType::Cvs, "x");
        c.url = Some(":pserver:cvs.example.com:/cvsroot".to_owned());
        c.anon_user = Some("anonymous".to_owned());
        c.anon_pass = Some("".to_owned());
        assert_eq!(cvs_url(&c).unwrap(), ":pserver:anonymous:@cvs.example.com:/cvsroot");
    }

    #[test]
    fn cvs_url_embeds_anon_creds_into_plain_url() {
        let mut c = component(ComponentType::Cvs, "x");
        c.url = Some("cvs.example.com:/cvsroot".to_owned());
        c.anon_user = Some("anonymous".to_owned());
        c.anon_pass = Some("secret".to_owned());
        assert_eq!(cvs_url(&c).unwrap(), "anonymous:secret@cvs.example.com:/cvsroot");
    }

    #[test]
    fn cvs_checkout_args_shape_with_branch() {
        let (prog, args) = cvs_checkout_args(":ext:cvs.example.com:/root", Some("REL_1"), "MyDir", "module/x");
        assert_eq!(prog, "cvs");
        assert_eq!(
            args,
            vec!["-q", "-d", ":ext:cvs.example.com:/root", "checkout", "-r", "REL_1", "-d", "MyDir", "module/x"]
        );
    }

    #[test]
    fn cvs_update_args_shape_without_branch() {
        let (prog, args) = cvs_update_args(None);
        assert_eq!(prog, "cvs");
        assert_eq!(args, vec!["-q", "update", "-dP"]);
    }

    #[test]
    fn hg_clone_args_shape_with_branch() {
        let (prog, args) = hg_clone_args("https://example.com/repo", Some("stable"), Path::new("/tmp/dest"));
        assert_eq!(prog, "hg");
        assert_eq!(args, vec!["clone", "-u", "stable", "https://example.com/repo", "/tmp/dest"]);
    }

    #[test]
    fn hg_pull_args_shape() {
        let (prog, args) = hg_pull_args();
        assert_eq!(prog, "hg");
        assert_eq!(args, vec!["pull", "-u"]);
    }

    #[test]
    fn darcs_clone_args_shape_with_branch() {
        let (prog, args) = darcs_clone_args("https://example.com/repo", Some("release"), Path::new("/tmp/dest"));
        assert_eq!(prog, "darcs");
        assert_eq!(args, vec!["clone", "-t", "release", "https://example.com/repo", "/tmp/dest"]);
    }

    #[test]
    fn darcs_pull_args_shape_without_branch() {
        let (prog, args) = darcs_pull_args(None);
        assert_eq!(prog, "darcs");
        assert_eq!(args, vec!["pull", "-a"]);
    }

    #[test]
    fn missing_binary_names_the_tool_and_checkout() {
        let c = component(ComponentType::Svn, "my-checkout");
        let err = run_tool("cactup-definitely-not-a-real-tool", &[], None, &c).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("cactup-definitely-not-a-real-tool"), "{message}");
        assert!(message.contains("my-checkout"), "{message}");
        assert!(message.contains("not installed"), "{message}");
    }

    #[test]
    fn fetch_external_rejects_non_external_type() {
        let dir = tempfile::tempdir().unwrap();
        let c = component(ComponentType::Git, "x");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch_external(dir.path(), &c)));
        assert!(result.is_err(), "expected fetch_external to panic on a non-external type");
    }
}
