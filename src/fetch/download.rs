//! http/https/ftp components: plain downloads of `$URL/$CHECKOUT` into
//! `$TARGET`, mirroring GetComponents' `handle_wget` (no archive extraction).
//! Implemented by the FETCH stream.
//!
//! Ported from `Cactus/bin/GetComponents`'s `handle_wget` (lines ~2165-2263;
//! `!TYPE = http`/`https`/`ftp` all dispatch to it, line ~120-122). The Perl
//! shells out to `wget`, letting *it* decide the saved filename from the
//! fetched URL when `!NAME` is absent; we fetch with `reqwest` instead (no
//! new process, no dependence on a `wget` binary being installed) and so
//! must decide that filename ourselves — [`derive_filename`] mirrors wget's
//! usual behavior (the URL's last path segment) closely enough for every
//! real thornlist entry, without trying to replicate wget's exact
//! `Content-Disposition`/redirect-chain heuristics.
//!
//! Deliberate divergence from GetComponents: `!TYPE = ftp` is rejected
//! outright. `reqwest` (rustls-backed, see `Cargo.toml`) speaks http/https
//! only; GetComponents' own `ftp` support was really just `wget`'s (line
//! ~121: `'ftp' => \&handle_wget`), which cactup has no equivalent for
//! without shelling out. Callers should fetch an `ftp` component manually
//! (e.g. with `wget`/`curl`/a browser) into the destination named in the
//! error, or install it another way.

use crate::thornlist::{Component, ComponentType};
use anyhow::{anyhow, bail, Context};
use std::path::{Path, PathBuf};

/// Download one http/https component into `<install_root>/<target>/`,
/// overwriting whatever is already there (refetch semantics: downloads are
/// always refreshed, matching GetComponents' `update` method never skipping
/// a re-fetch). Returns the final file path. Never extracts an archive —
/// neither does `handle_wget`. Streams the body in 64 KiB chunks rather than
/// buffering it whole, reporting bytes (with a percentage and throughput,
/// when the server sends `Content-Length`) via `progress`.
pub fn download_component(
    install_root: &Path,
    component: &Component,
    progress: &mut impl prodash::Progress,
) -> crate::Res<PathBuf> {
    if component.ty == ComponentType::Ftp {
        let dest_dir = install_root.join(&component.target);
        bail!(
            "component '{}' uses !TYPE = ftp, which cactup cannot fetch natively \
             (reqwest, cactup's HTTP client, speaks only http/https — GetComponents \
             itself only handled ftp by shelling out to wget, see the module docs). \
             Fetch it manually into '{}' or install the component another way.",
            component.checkout,
            dest_dir.display(),
        );
    }
    if !matches!(component.ty, ComponentType::Http | ComponentType::Https) {
        bail!(
            "download_component called on component '{}' with !TYPE {:?}, which is \
             not a download type (caller bug)",
            component.checkout,
            component.ty,
        );
    }

    let url = component
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("component '{}' has no !URL", component.checkout))?;
    let fetch_url = effective_url(url, &component.checkout);
    let filename = derive_filename(component.name.as_deref(), &component.checkout);

    let dest_dir = install_root.join(&component.target);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create {}", dest_dir.display()))?;
    let dest_path = dest_dir.join(&filename);
    let tmp_path = dest_dir.join(format!("{filename}.part"));

    let mut response = reqwest::blocking::get(&fetch_url)
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("Failed to download {fetch_url}"))?;
    progress.init(
        response.content_length().map(|l| l as usize),
        Some(prodash::unit::dynamic_and_mode(
            prodash::unit::Bytes,
            prodash::unit::display::Mode::with_throughput().and_percentage(),
        )),
    );

    // Write to a same-directory temp file, then rename into place, so a
    // reader never observes a partially-written destination file. Streamed
    // rather than buffered whole, so progress (and memory use) tracks the
    // download as it happens rather than jumping to 100% at the end.
    {
        let mut tmp_file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            if gix::interrupt::is_triggered() {
                bail!("interrupted");
            }
            let n = std::io::Read::read(&mut response, &mut buf)
                .with_context(|| format!("Failed to read response body for {fetch_url}"))?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut tmp_file, &buf[..n])
                .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
            progress.inc_by(n);
        }
    }
    std::fs::rename(&tmp_path, &dest_path).with_context(|| {
        format!("Failed to move {} into place at {}", tmp_path.display(), dest_path.display())
    })?;

    Ok(dest_path)
}

/// `"$url/$checkout"` (GetComponents lines 2193, 2198, 2221, 2226): a plain
/// string join, not a URL-aware one. If `checkout` is `.` or empty, the
/// result ends in a literal `/.` or `/` respectively — GetComponents never
/// special-cases either; whatever the join produces is handed straight to
/// the fetcher (wget there, reqwest here).
fn effective_url(url: &str, checkout: &str) -> String {
    format!("{url}/{checkout}")
}

/// The saved filename: `!NAME` if present (GetComponents renames the
/// wget-fetched file to it, lines 2183-2195), else the last `/`-separated
/// segment of `checkout` — matching wget's own default of naming the file
/// after the fetched URL's last path segment, since `checkout` is exactly
/// what gets appended last in [`effective_url`]. Falls back to `index.html`
/// (wget's own fallback for a URL with no usable trailing segment) when that
/// segment is empty or `.` — i.e. `checkout` is empty or `.`.
fn derive_filename(name: Option<&str>, checkout: &str) -> String {
    if let Some(n) = name {
        return n.to_owned();
    }
    let base = checkout.rsplit('/').next().unwrap_or(checkout);
    if base.is_empty() || base == "." {
        "index.html".to_owned()
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A standalone progress item — no renderer, just the in-memory counters
    /// `download_component` writes to.
    fn test_progress() -> prodash::tree::Item {
        prodash::tree::Root::new().add_child("test")
    }

    fn http_component(checkout: &str, name: Option<&str>) -> Component {
        Component {
            ty: ComponentType::Http,
            target: "Cactus/arrangements/Thorns".to_owned(),
            checkout: checkout.to_owned(),
            name: name.map(str::to_owned),
            url: Some("https://example.com/dist".to_owned()),
            auth_url: None,
            anon_user: None,
            anon_pass: None,
            repo_path: None,
            branch: None,
            repo: "dist".to_owned(),
        }
    }

    #[test]
    fn effective_url_is_a_plain_join() {
        assert_eq!(effective_url("https://example.com", "file.tar.gz"), "https://example.com/file.tar.gz");
        assert_eq!(effective_url("https://example.com", "."), "https://example.com/.");
        assert_eq!(effective_url("https://example.com", ""), "https://example.com/");
    }

    #[test]
    fn derive_filename_prefers_name() {
        assert_eq!(derive_filename(Some("custom.tar.gz"), "file.tar.gz"), "custom.tar.gz");
    }

    #[test]
    fn derive_filename_uses_checkout_basename() {
        assert_eq!(derive_filename(None, "file.tar.gz"), "file.tar.gz");
        assert_eq!(derive_filename(None, "sub/dir/file.tar.gz"), "file.tar.gz");
    }

    #[test]
    fn derive_filename_falls_back_for_dot_or_empty_checkout() {
        assert_eq!(derive_filename(None, "."), "index.html");
        assert_eq!(derive_filename(None, ""), "index.html");
    }

    #[test]
    fn ftp_type_bails_without_touching_network() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.ty = ComponentType::Ftp;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("ftp"), "{message}");
        assert!(message.contains("file.tar.gz"), "{message}");
    }

    #[test]
    fn wrong_type_is_a_caller_bug_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.ty = ComponentType::Git;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        assert!(format!("{err:#}").contains("caller bug"));
    }

    #[test]
    fn missing_url_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = http_component("file.tar.gz", None);
        c.url = None;
        let err = download_component(dir.path(), &c, &mut test_progress()).unwrap_err();
        assert!(format!("{err:#}").contains("!URL"));
    }
}
