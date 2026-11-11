//! A `.sim`'s inputs fetched from the network (spec §8.9): a file, kept as
//! downloaded, or an archive, unpacked, edited by its fixups and kept as a
//! tree. Both live in the inputs directory (the `inputs-home` path: the
//! knob, else the machine's `[paths]` value, else `~/.cactup/inputs`),
//! shared by every simulation that needs the same bytes:
//!
//! - `<dir>/<sha256>/<file name>`: a file input, by its content hash;
//! - `<dir>/unpacked/<key>/`: an unpacked archive, by [`tree_key`] (the
//!   archive's hash and its fixups, so two `.sim`s that edit the same archive
//!   differently never share a tree). A fixup bakes the tree's absolute
//!   path into the files it edits, so a tree is only good where it was
//!   unpacked: only the current inputs directory is ever looked in, and
//!   moving `inputs-home` means unpacking again there.
//!
//! Only `cactup import` reaches this, on the login node (D11). An archive is
//! never unpacked unverified: its SHA-256 is required (the shared crate's
//! validation) and checked before the first byte is unpacked. A tree is
//! built in a temporary directory beside its final place and renamed in
//! whole, so an interrupted import leaves no half-made tree for the next one
//! to trust.

use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::sim::{ArchiveKind, Fixup, Input};
use colored::Colorize;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

/// A download bigger than this is confirmed first.
const CONFIRM_ABOVE: u64 = 1 << 30;
/// No download grows past this, `-y` or not: a server streaming without end
/// must not fill the disk.
const MAX_DOWNLOAD: u64 = 1 << 40;
/// At most this many entries in one archive.
const MAX_ENTRIES: u64 = 1_000_000;
/// At most this many unpacked bytes: [`MIN_UNPACKED_CAP`], or this many
/// times the archive's size if that is more. Generous for real data (a
/// tarball of binary solution files barely compresses); a bomb is stopped
/// after writing at most that much, so the floor is kept small enough that
/// a home quota survives it.
const UNPACKED_RATIO: u64 = 32;
const MIN_UNPACKED_CAP: u64 = 4 << 30;
/// The longest symlink target an archive may hold (a zip keeps it as the
/// entry's content, which is otherwise unbounded).
const MAX_LINK_TARGET: u64 = 4096;
/// At most this many symlinks in one archive, each target at most this many
/// components deep: checking where a link leads walks its target, so these
/// bound how long that takes.
const MAX_LINKS: usize = 100_000;
const MAX_LINK_DEPTH: usize = 256;
/// The memory an `.xz` stream may ask for, in KiB: `xz -9` needs 674 MiB.
const XZ_MEM_LIMIT_KIB: u32 = 1 << 20;
/// Written into a finished tree just before it is moved into place; a tree
/// without it (a scratch purge removes files but keeps directories) is
/// unpacked again.
const COMPLETE_MARKER: &str = ".cactup-unpacked";

/// The inputs directory: the `inputs-home` knob, else the machine's
/// `[paths] inputs-home` (`@USER@`/`@ENV(…)@` resolved), else
/// `~/.cactup/inputs`. A host no machine matches has only the knob and the
/// fallback.
pub fn inputs_dir(ctx: &crate::commands::Ctx) -> Res<PathBuf> {
    let db = ctx.db.read()?;
    let from_machine = match crate::commands::machine::resolve_quietly(ctx)? {
        Some(machine) => machine.meta.path_for(&db, "inputs-home")?,
        None => db.knob("inputs-home").filter(|v| Path::new(v).is_absolute()).map(str::to_owned),
    };
    Ok(match from_machine {
        Some(dir) => PathBuf::from(dir),
        None => crate::mdb::meta::fallback_home("inputs-home").expect("inputs-home has a fallback"),
    })
}

/// Whether to go ahead with a download of `size` bytes: yes below
/// [`CONFIRM_ABOVE`]; above it, asked on a terminal, taken with `yes`
/// (import's `-y`), refused otherwise.
pub struct SizePolicy {
    pub interactive: bool,
    pub yes: bool,
    pub confirm_above: u64,
}

impl SizePolicy {
    pub fn new(interactive: bool, yes: bool) -> SizePolicy {
        SizePolicy { interactive, yes, confirm_above: CONFIRM_ABOVE }
    }
}

/// The value of `input`'s knob, fetched: the downloaded file, or for an
/// archive, the unpacked tree (or the file `path` names in it). One line of
/// progress for the whole input, named for its knob (downloading, checking,
/// unpacking), left in the scrollback as one green line saying what
/// happened; a size question comes first, before anything is drawn.
pub fn fetch(dir: &Path, input: &Input, policy: &SizePolicy) -> Res<PathBuf> {
    let url = input.url.as_deref().ok_or_else(|| anyhow!("Input {} has no URL to fetch", input.knob))?;
    if let Some(done) = already_fetched(dir, input, url)? {
        return Ok(done);
    }
    let limit = confirm_size(url, policy)?;
    let (progress, renderer) =
        crate::manifest::setup_prodash_with(Some(crate::manifest::progress_level_filter(1)), true);
    let layout = crate::progress::Layout::for_names([input.knob.as_str()]);
    let mut line = crate::progress::Line::counting(progress.add_child(input.knob.clone()), &input.knob, layout);
    let fetched = if input.unpack {
        unpacked(dir, input, url, limit, &mut line)
    } else {
        fetch_file(dir, url, input.sha256.as_deref(), limit, &mut line)
    };
    if let Ok((_, done)) = &fetched {
        line.succeeded(done.clone());
    }
    drop(line);
    renderer.shutdown_and_wait();
    let (value, _) = fetched?;
    if input.sha256.is_none() {
        println!("{}", format!("The .sim records no hash for {url}, so the download could not be checked").yellow());
    }
    Ok(value)
}

/// The input's value if a finished copy is already in `dir`: a file input
/// at its hash, an archive's tree with its completion marker and `path`. An
/// incomplete tree (a scratch purge removes files, not directories, or a
/// crash came before the marker) is removed, to be made again.
fn already_fetched(dir: &Path, input: &Input, url: &str) -> Res<Option<PathBuf>> {
    if !input.unpack {
        let cached = input.sha256.as_deref().map(|h| dir.join(h.to_ascii_lowercase()).join(file_name(url)));
        return Ok(cached.filter(|c| c.is_file()));
    }
    let tree = dir.join("unpacked").join(tree_key(input));
    if !tree.is_dir() {
        return Ok(None);
    }
    let value = input.path.as_deref().map_or_else(|| tree.clone(), |p| tree.join(p));
    // The marker is read, not just looked at: its access time then moves
    // with the tree's use, so a purge by access time does not take it first.
    if std::fs::read(tree.join(COMPLETE_MARKER)).is_ok() && std::fs::symlink_metadata(&value).is_ok() {
        return Ok(Some(value));
    }
    // Set aside, not deleted: simulations may still have files open in it,
    // and another import may be doing the same. `cache inputs --clear`
    // removes it.
    println!("{}", format!("'{}' is incomplete; unpacking it again", tree.display()).yellow());
    let aside = tree.with_file_name(format!(".stale-{}-{}", tree.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    if let Err(e) = std::fs::rename(&tree, &aside)
        && tree.exists()
    {
        return Err(e).with_context(|| format!("Failed to set '{}' aside", tree.display()));
    }
    Ok(None)
}

/// A file input at `<dir>/<sha256>/<file name>`, checked against the
/// `.sim`'s hash when it records one. Returns it and what to say of it.
fn fetch_file(
    dir: &Path,
    url: &str,
    sha256: Option<&str>,
    limit: Option<u64>,
    line: &mut crate::progress::Line,
) -> Res<(PathBuf, String)> {
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    let temp = download(dir, url, limit, line)?;
    line.phase("checking");
    let actual = crate::update::sha256_hex_counting(temp.path(), line)?;
    if let Some(expected) = sha256
        && !expected.eq_ignore_ascii_case(&actual)
    {
        bail!("The download of {url} has SHA-256 {actual}, but the .sim records {expected}");
    }
    let hash_dir = dir.join(&actual);
    std::fs::create_dir_all(&hash_dir).with_context(|| format!("Failed to create '{}'", hash_dir.display()))?;
    let dest = hash_dir.join(file_name(url));
    temp.persist(&dest).with_context(|| format!("Failed to save '{}'", dest.display()))?;
    Ok((dest.clone(), format!("downloaded to '{}'", dest.display())))
}

/// An archive input: its tree at `<dir>/unpacked/<key>/`, made once — the
/// archive downloaded beside it, verified, unpacked into a temporary
/// directory, its fixups applied, then renamed into place (and the download
/// removed: the tree is what is kept). Returns the knob's value and what to
/// say of it.
fn unpacked(
    dir: &Path,
    input: &Input,
    url: &str,
    limit: Option<u64>,
    line: &mut crate::progress::Line,
) -> Res<(PathBuf, String)> {
    let expected = input.sha256.as_deref().ok_or_else(|| anyhow!("Input {} unpacks an archive with no SHA-256", input.knob))?;
    let kind = ArchiveKind::of_url(url).ok_or_else(|| anyhow!("{url} is not an archive cactup can unpack"))?;
    let trees = dir.join("unpacked");
    let tree = trees.join(tree_key(input));
    let value = |tree: &Path| input.path.as_deref().map_or_else(|| tree.to_owned(), |p| tree.join(p));
    std::fs::create_dir_all(&trees).with_context(|| format!("Failed to create '{}'", trees.display()))?;

    let archive = download(&trees, url, limit, line)?;
    line.phase("checking");
    let actual = crate::update::sha256_hex_counting(archive.path(), line)?;
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("The download of {url} has SHA-256 {actual}, but the .sim records {expected}; nothing was unpacked");
    }
    let staging = tempfile::Builder::new()
        .prefix(".unpacking-")
        .tempdir_in(&trees)
        .with_context(|| format!("Failed to create a directory in '{}'", trees.display()))?;
    line.phase("unpacking");
    let entries = unpack(archive.path(), kind, staging.path(), &file_name(url), line)?;
    drop(archive);
    let tree_str = tree.to_str().ok_or_else(|| anyhow!("'{}' is not UTF-8", tree.display()))?;
    let mut fixed = 0;
    for fixup in &input.fixups {
        // Through no link: the tree's links are checked to stay inside it,
        // but an edit must also never land on a file a link merely names.
        let (file, meta) = within(staging.path(), Path::new(&fixup.file))
            .with_context(|| format!("A fixup cannot edit {}", fixup.file))?;
        if !meta.is_file() {
            bail!("{} in the archive is not a regular file, so no fixup can edit it", fixup.file);
        }
        let text = std::fs::read(&file).with_context(|| format!("Failed to read '{}'", file.display()))?;
        let (edited, n) = replace_all(&text, fixup.replace.as_bytes(), fixup.replacement(tree_str).as_bytes());
        if n == 0 {
            bail!("A fixup of {} replaces \"{}\", which the file does not contain", fixup.file, fixup.replace);
        }
        // A new file renamed over the old: works on a read-only member, keeps
        // its mode, and a rename never follows a link.
        let dir = file.parent().expect("a member has a directory");
        let mut temp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("Failed to edit '{}'", file.display()))?;
        temp.write_all(&edited).with_context(|| format!("Failed to edit '{}'", file.display()))?;
        temp.as_file().set_permissions(meta.permissions())?;
        temp.persist(&file).with_context(|| format!("Failed to edit '{}'", file.display()))?;
        fixed += n;
    }
    if let Some(path) = &input.path {
        within(staging.path(), Path::new(path))
            .with_context(|| format!("The archive has no usable {path}, which the .sim points input {} at", input.knob))?;
    }
    // Written new: an archive member of that name (a link, say) is refused,
    // not written through.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging.path().join(COMPLETE_MARKER))
        .with_context(|| format!("The archive has a member named {COMPLETE_MARKER}, which cactup keeps for itself"))?;
    // Another import may have finished the same tree meanwhile: theirs is as
    // good as ours, so ours is dropped.
    let staged = staging.keep();
    if let Err(e) = std::fs::rename(&staged, &tree) {
        let _ = std::fs::remove_dir_all(&staged);
        if !tree.is_dir() {
            return Err(e).with_context(|| format!("Failed to move the unpacked archive to '{}'", tree.display()));
        }
    }
    let fixes = if fixed == 0 { String::new() } else { format!(", {fixed} edit(s) made") };
    Ok((value(&tree), format!("unpacked {entries} entries{fixes} into '{}'", tree.display())))
}

/// The directory name of an archive input's tree: a digest of the
/// archive's hash and of every fixup, in order.
pub fn tree_key(input: &Input) -> String {
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut part = |bytes: &[u8]| {
        ctx.update(&(bytes.len() as u64).to_le_bytes());
        ctx.update(bytes);
    };
    part(b"cactup input tree 1");
    part(input.sha256.as_deref().unwrap_or("").to_ascii_lowercase().as_bytes());
    for f in &input.fixups {
        part(f.file.as_bytes());
        part(f.replace.as_bytes());
        part(f.with.as_bytes());
    }
    let digest = ctx.finish();
    digest.as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The last segment of `url`'s path, for naming what it downloads.
fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("input").to_owned()
}

/// Download `url` into a temporary file in `dir`, reporting on `line`, no
/// more than `limit` bytes.
fn download(
    dir: &Path,
    url: &str,
    limit: Option<u64>,
    line: &mut crate::progress::Line,
) -> Res<tempfile::NamedTempFile> {
    let mut temp = tempfile::NamedTempFile::new_in(dir).context("Failed to create a download file")?;
    line.phase("downloading");
    crate::fetch::download::download_to_limited(crate::fetch::download::client(), url, temp.as_file_mut(), line, limit)?;
    Ok(temp)
}

/// Ask the server how big `url` is (a HEAD request), and above
/// [`SizePolicy::confirm_above`] ask the user. Returns the most the GET may
/// then bring: with `-y`, [`MAX_DOWNLOAD`] (an announced size is not held
/// against a server that under-reports it); otherwise what the server
/// announced, with a little slack, or the threshold itself when it would
/// not say.
fn confirm_size(url: &str, policy: &SizePolicy) -> Res<Option<u64>> {
    // Bounded: a server that will not answer a HEAD must not hold up a
    // Ctrl-C for the client's whole timeout.
    let size = crate::fetch::download::client()
        .head(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.headers().get(reqwest::header::CONTENT_LENGTH)?.to_str().ok()?.parse::<u64>().ok());
    if policy.yes {
        if let Some(size) = size.filter(|s| *s > policy.confirm_above) {
            println!("Downloading {url} ({}).", crate::progress::bytes(size as usize));
        }
        return Ok(Some(MAX_DOWNLOAD));
    }
    let Some(size) = size else { return Ok(Some(policy.confirm_above)) };
    let limit = Some(size.saturating_add((size / 100).max(1 << 20)).min(MAX_DOWNLOAD));
    if size <= policy.confirm_above {
        return Ok(limit);
    }
    let shown = crate::progress::bytes(size as usize);
    if !policy.interactive {
        bail!("{url} is {shown}; pass -y to download it without asking");
    }
    if !crate::commands::confirm(&format!("{url} is {shown}. Download it? [y/N] "))? {
        bail!("Not downloading {url}");
    }
    Ok(limit)
}

/// Unpack `archive` into the empty directory `dest`, refusing anything that
/// could reach outside it: an absolute or `..` path, a link whose target
/// leaves the tree, a device or FIFO, and a write through a link an earlier
/// entry made. Returns the number of entries. Progress is the share of the
/// archive read; the interrupt flag is polled per entry and per chunk.
fn unpack(archive: &Path, kind: ArchiveKind, dest: &Path, name: &str, progress: &mut impl prodash::Progress) -> Res<u64> {
    let len = std::fs::metadata(archive)?.len();
    let mut budget = Budget { bytes: MIN_UNPACKED_CAP.max(len.saturating_mul(UNPACKED_RATIO)), entries: MAX_ENTRIES };
    let file = std::fs::File::open(archive).with_context(|| format!("Failed to open '{}'", archive.display()))?;
    progress.init(
        Some(len as usize),
        Some(prodash::unit::dynamic_and_mode(prodash::unit::Bytes, prodash::unit::display::Mode::with_percentage())),
    );
    let reader = Counting { inner: file, progress };
    let result = match kind {
        ArchiveKind::Tar => unpack_tar(reader, dest, &mut budget),
        ArchiveKind::TarGz => unpack_tar(flate2::read::MultiGzDecoder::new(reader), dest, &mut budget),
        ArchiveKind::TarXz => unpack_tar(lzma_rust2::XzReader::new_mem_limit(reader, true, XZ_MEM_LIMIT_KIB), dest, &mut budget),
        ArchiveKind::Zip => unpack_zip(reader, dest, &mut budget),
    };
    let (n, links) = result.with_context(|| format!("Failed to unpack {name}"))?;
    make_links(dest, links, progress).with_context(|| format!("Failed to unpack {name}"))?;
    Ok(n)
}

/// The symlinks an archive asked for, made only after every file and
/// directory is in place, so that nothing was ever written through one;
/// then each is checked where it really resolves, through any link before
/// it, to stay inside the tree. A later entry of the same name replaced an
/// earlier one while unpacking, so a link still listed here wins over a
/// file or directory only if it came last.
fn make_links(dest: &Path, links: Links, progress: &mut impl prodash::Progress) -> Res<()> {
    if links.is_empty() {
        return Ok(());
    }
    // Made, then each checked: two steps per link, counted, the interrupt
    // flag polled per link (many links on a network filesystem take a while).
    progress.init(Some(2 * links.len()), Some(prodash::unit::label("link steps")));
    for (rel, target) in &links {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let path = place(dest, rel)?;
        std::os::unix::fs::symlink(target, &path).with_context(|| format!("Failed to create '{}'", path.display()))?;
        progress.inc();
    }
    let root = dest.canonicalize().with_context(|| format!("Failed to resolve '{}'", dest.display()))?;
    for (rel, target) in &links {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        if !resolves_inside(&root, &dest.join(rel), target) {
            bail!(
                "The archive links {} to {}, which leads outside its own tree or round in a loop",
                rel.display(),
                target.display()
            );
        }
        progress.inc();
    }
    Ok(())
}

/// Whether the link at `link` (pointing at `target`) resolves under `root`,
/// following every link on the way. A dangling one resolves through its
/// deepest existing ancestor, which must be under `root`, and must not climb
/// back out (`..`) past it.
fn resolves_inside(root: &Path, link: &Path, target: &Path) -> bool {
    if let Ok(real) = link.canonicalize() {
        return real.starts_with(root);
    }
    let mut existing = link.parent().unwrap_or(root).join(target);
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        let Some(name) = existing.file_name().map(ToOwned::to_owned) else { return false };
        rest.push(name);
        if !existing.pop() {
            return false;
        }
    }
    if existing.as_os_str().is_empty() || rest.iter().any(|c| c == "..") {
        return false;
    }
    existing.canonicalize().is_ok_and(|real| real.starts_with(root))
}

/// `dest/rel` (a member path the `.sim` names), reached through no link:
/// each directory on the way must be a real directory, and the path itself
/// must exist. Returns it with its own metadata (not followed).
fn within(dest: &Path, rel: &Path) -> Res<(PathBuf, std::fs::Metadata)> {
    let mut at = dest.to_owned();
    let parts: Vec<_> = rel.components().collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(_) = part else { bail!("{} is not a plain path inside the archive", rel.display()) };
        at.push(part);
        let meta = std::fs::symlink_metadata(&at).with_context(|| format!("The archive has no {}", rel.display()))?;
        if i + 1 == parts.len() {
            return Ok((at, meta));
        }
        if !meta.is_dir() {
            bail!("{} goes through {}, which is not a directory of the archive", rel.display(), at.strip_prefix(dest).unwrap_or(&at).display());
        }
    }
    bail!("{} names the archive's root", rel.display())
}

/// What an archive may still unpack.
struct Budget {
    bytes: u64,
    entries: u64,
}

impl Budget {
    fn entry(&mut self) -> Res<()> {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        self.entries = self.entries.checked_sub(1).ok_or_else(|| anyhow!("The archive has more than {MAX_ENTRIES} entries"))?;
        Ok(())
    }
}

/// A reader that advances `progress` by the bytes read through it.
struct Counting<'a, R, P> {
    inner: R,
    progress: &'a mut P,
}

impl<R: Read, P: prodash::Count> Read for Counting<'_, R, P> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.inc_by(n);
        Ok(n)
    }
}

impl<R: std::io::Seek, P: prodash::Count> std::io::Seek for Counting<'_, R, P> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let at = self.inner.seek(pos)?;
        self.progress.set(at as usize);
        Ok(at)
    }
}

/// An entry's path as a relative path inside the tree: `.` and empty
/// components dropped, anything absolute or with `..` refused. `None` for
/// the root itself.
fn member_path(name: &Path) -> Res<Option<PathBuf>> {
    let mut out = PathBuf::new();
    for c in name.components() {
        match c {
            Component::Normal(part) => {
                if part.to_string_lossy().contains('\\') {
                    bail!("The archive has an entry with a backslash in its path: {}", name.display());
                }
                out.push(part)
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("The archive has an entry outside its own tree: {}", name.display())
            }
        }
    }
    Ok((!out.as_os_str().is_empty()).then_some(out))
}

/// `dest.join(rel)`, after checking that no directory on the way is a
/// link (an earlier entry could have made one pointing anywhere), and with
/// the parents made.
fn place(dest: &Path, rel: &Path) -> Res<PathBuf> {
    let mut at = dest.to_owned();
    let parts: Vec<_> = rel.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("The archive writes through {}, which is not a directory", at.strip_prefix(dest).unwrap_or(&at).display()),
            Err(_) => std::fs::create_dir(&at).with_context(|| format!("Failed to create '{}'", at.display()))?,
        }
    }
    let path = dest.join(rel);
    // A later entry of the same name replaces an earlier file (as tar does);
    // anything else in the way is refused.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => std::fs::remove_file(&path)?,
        Ok(m) if m.is_dir() => bail!("The archive has both a directory and a file at {}", rel.display()),
        Ok(_) => bail!("The archive writes through the link {}", rel.display()),
        Err(_) => {}
    }
    Ok(path)
}

/// The directory `rel`, made along with its parents, none of them a link.
fn place_dir(dest: &Path, rel: &Path) -> Res<()> {
    let mut at = dest.to_owned();
    for part in rel.components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("The archive has both a file and a directory at {}", rel.display()),
            Err(_) => std::fs::create_dir(&at).with_context(|| format!("Failed to create '{}'", at.display()))?,
        }
    }
    Ok(())
}

/// A link at `rel` pointing at `target` stays inside the tree: `target` is
/// relative, and resolving it lexically from `rel`'s directory never climbs
/// above the root.
fn link_stays_inside(rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth = rel.components().count() as i64 - 1;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Write one regular file from `body`, charging its bytes to `budget`, with
/// `mode`'s permission bits (never setuid/setgid/sticky).
fn write_file(path: &Path, body: &mut impl Read, mode: Option<u32>, budget: &mut Budget) -> Res<()> {
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("Failed to create '{}'", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        budget.bytes = budget
            .bytes
            .checked_sub(n as u64)
            .ok_or_else(|| anyhow!("The archive unpacks to more than the size cactup allows for it"))?;
        out.write_all(&buf[..n]).with_context(|| format!("Failed to write '{}'", path.display()))?;
    }
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}

/// The symlinks an archive asks for, `(member, target)`, in archive order:
/// [`make_links`] makes them once everything else is written.
type Links = indexmap::IndexMap<PathBuf, PathBuf>;

/// A link entry, checked lexically now (a cheap early refusal) and made
/// later; any earlier link or entry of the same name gives way to it.
fn defer_link(links: &mut Links, rel: PathBuf, target: PathBuf) -> Res<()> {
    if target.as_os_str().len() as u64 > MAX_LINK_TARGET
        || target.components().count() > MAX_LINK_DEPTH
        || !link_stays_inside(&rel, &target)
    {
        bail!("The archive links {} to {}, outside its own tree", rel.display(), target.display());
    }
    if links.len() >= MAX_LINKS && !links.contains_key(&rel) {
        bail!("The archive has more than {MAX_LINKS} symlinks");
    }
    links.shift_remove(&rel);
    links.insert(rel, target);
    Ok(())
}

/// The most a tar archive may read between two pieces of file data: its
/// headers, and the long names and extended headers the tar reader takes in
/// whole, in memory, before an entry reaches cactup.
const MAX_TAR_HEADER_BYTES: u64 = 1 << 20;

/// A reader that counts the bytes read through it since `since_data` was
/// last reset, and fails past [`MAX_TAR_HEADER_BYTES`]; file data resets it
/// as it is read ([`DataRead`]).
struct HeaderGuard<R> {
    inner: R,
    since_data: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for HeaderGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let since = self.since_data.get() + n as u64;
        if since > MAX_TAR_HEADER_BYTES {
            return Err(std::io::Error::other(
                "the archive has a header (a long name or an extended header) bigger than cactup reads",
            ));
        }
        self.since_data.set(since);
        Ok(n)
    }
}

/// An entry's data, read through [`HeaderGuard`]: each read resets its count.
struct DataRead<'a, R> {
    inner: R,
    since_data: &'a std::cell::Cell<u64>,
}

impl<R: Read> Read for DataRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.since_data.set(0);
        Ok(n)
    }
}

fn unpack_tar(reader: impl Read, dest: &Path, budget: &mut Budget) -> Res<(u64, Links)> {
    let since_data = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut archive = tar::Archive::new(HeaderGuard { inner: reader, since_data: since_data.clone() });
    let mut links = Links::new();
    let mut n = 0;
    for entry in archive.entries().context("The archive is not a tar archive")? {
        budget.entry()?;
        let mut entry = entry.context("The archive is damaged")?;
        let name = entry.path().context("An entry has an unreadable path")?.into_owned();
        let Some(rel) = member_path(&name)? else { continue };
        n += 1;
        let ty = entry.header().entry_type();
        if !ty.is_symlink() {
            // A later entry of this name replaces a link asked for earlier.
            links.shift_remove(&rel);
        }
        if ty.is_dir() {
            place_dir(dest, &rel)?;
            continue;
        }
        if ty.is_file() || ty == tar::EntryType::Continuous {
            let mode = entry.header().mode().ok();
            let path = place(dest, &rel)?;
            write_file(&path, &mut DataRead { inner: &mut entry, since_data: &since_data }, mode, budget)?;
        } else if ty.is_symlink() || ty.is_hard_link() {
            let target = entry.link_name().context("A link has an unreadable target")?.ok_or_else(|| anyhow!("A link has no target"))?.into_owned();
            if ty.is_symlink() {
                defer_link(&mut links, rel, target)?;
            } else {
                // A hard link names another member by its path from the root:
                // a regular file already unpacked, reached through no link.
                let Some(source) = member_path(&target)? else { bail!("The archive hard-links {} to its root", rel.display()) };
                let from = within(dest, &source).ok().filter(|(_, m)| m.is_file()).map(|(p, _)| p).ok_or_else(|| {
                    anyhow!("The archive hard-links {} to {}, which is not a file it unpacked before", rel.display(), source.display())
                })?;
                let path = place(dest, &rel)?;
                std::fs::hard_link(&from, &path)?;
            }
        } else if matches!(ty, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader | tar::EntryType::GNULongName | tar::EntryType::GNULongLink) {
            n -= 1;
        } else {
            bail!("The archive has {} of a kind cactup does not unpack (a device, FIFO or similar)", rel.display());
        }
    }
    Ok((n, links))
}

fn unpack_zip<R: Read + std::io::Seek>(reader: R, dest: &Path, budget: &mut Budget) -> Res<(u64, Links)> {
    let mut archive = zip::ZipArchive::new(reader).context("The archive is not a zip archive")?;
    let mut links = Links::new();
    let mut n = 0;
    for i in 0..archive.len() {
        budget.entry()?;
        let mut entry = archive.by_index(i).context("The archive is damaged")?;
        let raw = entry.name().context("An entry has an unreadable path")?.into_owned();
        if raw.contains('\\') {
            bail!("The archive has an entry with a backslash in its path: {raw}");
        }
        let name = PathBuf::from(raw);
        let Some(rel) = member_path(&name)? else { continue };
        n += 1;
        if !entry.is_symlink() {
            links.shift_remove(&rel);
        }
        if entry.is_dir() {
            place_dir(dest, &rel)?;
        } else if entry.is_symlink() {
            // The target is the entry's content: bounded, since nothing else
            // limits it.
            let mut target = String::new();
            (&mut entry).take(MAX_LINK_TARGET + 1).read_to_string(&mut target).context("A link has an unreadable target")?;
            defer_link(&mut links, rel, PathBuf::from(target))?;
        } else {
            let mode = entry.unix_mode();
            let path = place(dest, &rel)?;
            write_file(&path, &mut entry, mode, budget)?;
        }
    }
    Ok((n, links))
}

/// `haystack` with every non-overlapping `needle` (left to right) replaced
/// by `with`, and how many there were.
fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(haystack.len());
    let mut n = 0;
    let mut i = 0;
    while i < haystack.len() {
        if !needle.is_empty() && haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
            n += 1;
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    (out, n)
}

/// What an author's answers about one input come to, for `sim export` and
/// `sim pack`: the knob, where its file can be downloaded, and the knob's
/// value here (to compare the download with).
pub struct Authoring<'a> {
    pub url: &'a str,
    /// The knob's current value on this machine, if it names something.
    pub local: Option<&'a Path>,
    /// The archive member the knob points at, when the author named it.
    pub member: Option<&'a str>,
    /// Questions may be asked (a terminal, no `-y`).
    pub interactive: bool,
}

/// The largest member whose text is compared with the author's copy when
/// looking for fixups; bigger ones (solution data) are compared by size.
const MAX_COMPARED: u64 = 16 << 20;

/// Fill `input` for an author who says its file can be downloaded from
/// `how.url`: downloaded once (into `dir`, the inputs directory: archives
/// are big) and hashed. An archive is unpacked in a scratch directory
/// there: `path` is the member the knob's local value ends with (or the one
/// named), and each text member that differs from the author's copy only on
/// lines naming the author's unpack directory becomes a fixup, that
/// directory written `{dir}`; any other difference is reported, not
/// recorded. Returns what to tell the author.
pub fn author(dir: &Path, input: &mut Input, how: &Authoring, policy: &SizePolicy) -> Res<Vec<String>> {
    let url = how.url;
    let mut notes = Vec::new();
    let limit = confirm_size(url, policy)?;
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    let scratch = tempfile::Builder::new()
        .prefix(".tmp-authoring-")
        .tempdir_in(dir)
        .with_context(|| format!("Failed to create a directory in '{}'", dir.display()))?;
    let (progress, renderer) =
        crate::manifest::setup_prodash_with(Some(crate::manifest::progress_level_filter(1)), true);
    let layout = crate::progress::Layout::for_names([input.knob.as_str()]);
    let mut line = crate::progress::Line::counting(progress.add_child(input.knob.clone()), &input.knob, layout);
    let fetched = (|| -> Res<(String, Option<PathBuf>)> {
        let download = download(scratch.path(), url, limit, &mut line)?;
        line.phase("checking");
        let sha = crate::update::sha256_hex_counting(download.path(), &mut line)?;
        let Some(kind) = ArchiveKind::of_url(url) else {
            if let Some(local) = how.local.filter(|l| l.is_file())
                && crate::update::sha256_hex(local)? != sha
            {
                notes.push(format!("Your {} differs from the download", local.display()));
            }
            return Ok((sha, None));
        };
        let tree = scratch.path().join("tree");
        std::fs::create_dir(&tree)?;
        line.phase("unpacking");
        unpack(download.path(), kind, &tree, &file_name(url), &mut line)?;
        Ok((sha, Some(tree)))
    })();
    drop(line);
    renderer.shutdown_and_wait();
    let (sha, tree) = fetched?;
    input.url = Some(url.to_owned());
    input.sha256 = Some(sha);
    let Some(tree) = tree else { return Ok(notes) };
    input.unpack = true;

    // The member the knob points at, and with it the author's unpack
    // directory: the knob's value without that member's path.
    let members = regular_members(&tree)?;
    let member = match how.member {
        Some(m) => {
            within(&tree, Path::new(m)).with_context(|| format!("The archive at {url} has no usable {m}"))?;
            Some(m.to_owned())
        }
        None => how
            .local
            .and_then(|local| members.iter().filter(|m| local.ends_with(m)).max_by_key(|m| m.components().count()))
            .map(|m| m.to_string_lossy().into_owned()),
    };
    let member = match member {
        Some(m) => Some(m),
        None if how.interactive => {
            let answer = crate::commands::prompt_with_default(
                &format!("Which file of the archive does knob {} point at? (empty: the unpacked folder)", input.knob),
                "",
            )?;
            if !answer.is_empty() {
                within(&tree, Path::new(&answer)).with_context(|| format!("The archive has no usable {answer}"))?;
            }
            Some(answer).filter(|a| !a.is_empty())
        }
        None => None,
    };
    let root = match (&member, how.local) {
        (Some(m), Some(local)) if local.ends_with(m) => local.ancestors().nth(Path::new(m).components().count()),
        (None, Some(local)) if local.is_dir() => Some(local),
        _ => None,
    };
    input.path = member;
    let Some(root) = root else {
        notes.push(format!("Knob {}'s value here is not inside an unpacked copy of the archive; no fixups looked for", input.knob));
        return Ok(notes);
    };
    let root_str = root.to_string_lossy().into_owned();
    for member in &members {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let (theirs, mine) = (tree.join(member), root.join(member));
        let (Ok(their_meta), Ok(my_meta)) = (std::fs::metadata(&theirs), std::fs::metadata(&mine)) else { continue };
        let name = member.display();
        if their_meta.len() > MAX_COMPARED || my_meta.len() > MAX_COMPARED {
            if their_meta.len() != my_meta.len() {
                notes.push(format!("Your copy of {name} differs from the archive's; not recorded"));
            }
            continue;
        }
        let (a, b) = (std::fs::read(&theirs)?, std::fs::read(&mine)?);
        if a == b {
            continue;
        }
        match line_fixups(&a, &b, &root_str) {
            Some(lines) => {
                let offered = lines.iter().map(|(from, to)| format!("    {from}  ->  {to}")).collect::<Vec<_>>().join("\n");
                let take = !how.interactive
                    || crate::commands::confirm(&format!(
                        "Your {name} differs from the archive's:\n{offered}\nRecord this as a fixup relative to the unpacked folder? [y/N] "
                    ))?;
                if take {
                    for (from, to) in lines {
                        input.fixups.push(Fixup { file: member.to_string_lossy().into_owned(), replace: from, with: to });
                    }
                    notes.push(format!("Recorded a fixup of {name}"));
                }
            }
            None => notes.push(format!("Your copy of {name} differs from the archive's in more than its paths; not recorded")),
        }
    }
    Ok(notes)
}

/// Each regular file under `tree`, relative to it, reached through no link.
fn regular_members(tree: &Path) -> Res<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in std::fs::read_dir(tree.join(&rel))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let child = rel.join(entry.file_name());
            if kind.is_dir() {
                stack.push(child);
            } else if kind.is_file() {
                out.push(child);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The fixups that turn the archive's text `theirs` into the author's `mine`,
/// one whole line each: every line that differs must name the author's unpack
/// directory `root` in the author's copy, which becomes `{dir}`. `None` when
/// the copies differ any other way (not text, other lines added or removed,
/// a change that is not the path, or text a fixup cannot hold).
fn line_fixups(theirs: &[u8], mine: &[u8], root: &str) -> Option<Vec<(String, String)>> {
    let (theirs, mine) = (std::str::from_utf8(theirs).ok()?, std::str::from_utf8(mine).ok()?);
    let (a, b): (Vec<&str>, Vec<&str>) = (theirs.lines().collect(), mine.lines().collect());
    if a.len() != b.len() {
        return None;
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (from, to) in a.iter().zip(&b).filter(|(x, y)| x != y) {
        if from.is_empty() || !to.contains(root) {
            return None;
        }
        let with = to.replace(root, "{dir}");
        if with.replace("{dir}", "").contains(['{', '}']) || from.contains(['{', '}']) {
            return None;
        }
        if !out.iter().any(|(f, _)| f == from) {
            out.push(((*from).to_owned(), with));
        }
    }
    Some(out)
}

/// `cactup cache inputs`: each input in `dir` with its size, or with
/// `clear`, every one removed — and whatever an interrupted import left
/// (an `.unpacking-*` directory, a `.tmp*` download), which is listed as
/// such. Clearing while an import runs removes its work in progress too.
pub fn list_or_clear(dir: &Path, clear: bool) -> Res<()> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    let read = |d: &Path| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect();
        paths.sort();
        paths
    };
    let leftover = |p: &Path| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with(".unpacking-") || n.starts_with(".tmp") || n.starts_with(".stale-")
        })
    };
    for entry in read(dir) {
        let name = entry.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if leftover(&entry) {
            found.push((entry, "interrupted".to_owned()));
        } else if name == "unpacked" {
            for tree in read(&entry) {
                if leftover(&tree) {
                    found.push((tree, "interrupted".to_owned()));
                } else if !tree.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                    found.push((tree, "unpacked archive".to_owned()));
                }
            }
        } else if entry.is_dir() && !name.starts_with('.') {
            for file in read(&entry) {
                found.push((file, "download".to_owned()));
            }
        }
    }
    if found.is_empty() {
        println!("No inputs in '{}'.", dir.display());
        return Ok(());
    }
    let mut total = 0u64;
    for (path, kind) in &found {
        let size = disk_size(path);
        total += size;
        if clear {
            let removed = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
            removed.with_context(|| format!("Failed to remove '{}'", path.display()))?;
            // A download's hash directory goes with its file.
            if let Some(parent) = path.parent().filter(|p| *p != dir && p.file_name().is_some_and(|n| n != "unpacked")) {
                let _ = std::fs::remove_dir(parent);
            }
        } else {
            println!("  {:>9}  {kind:<16}  {}", crate::progress::bytes(size as usize), path.display());
        }
    }
    let total = crate::progress::bytes(total as usize);
    if clear {
        println!("Removed {} input(s), {total}, from '{}'.", found.len(), dir.display());
    } else {
        println!("{} input(s), {total}, in '{}'.", found.len(), dir.display());
    }
    Ok(())
}

/// The bytes under `path` (a file, or a tree, links not followed).
fn disk_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| disk_size(&e.path())).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::download::test_server::serve;
    use cactup_ecosystem_common::sim::Fixup;

    const INFO: &[u8] = b"eos = /path/to/gam2.polytrope\nother = 1\n";

    /// A tar of the gallery's shape: `bhns_gw230529_ID/{X.info, gam2.polytrope}`.
    fn gallery_tar() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, body: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, path, body).unwrap();
        };
        add("bhns_gw230529_ID/X.info", INFO);
        add("bhns_gw230529_ID/gam2.polytrope", b"polytrope\n");
        b.into_inner().unwrap()
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn input(url: String, archive: &[u8]) -> Input {
        Input {
            knob: "kadath-initial-data".into(),
            description: None,
            url: Some(url),
            sha256: Some(sha(archive)),
            unpack: true,
            path: Some("bhns_gw230529_ID/X.info".into()),
            fixups: vec![Fixup {
                file: "bhns_gw230529_ID/X.info".into(),
                replace: "/path/to/gam2.polytrope".into(),
                with: "{dir}/bhns_gw230529_ID/gam2.polytrope".into(),
            }],
        }
    }

    fn quiet() -> SizePolicy {
        SizePolicy::new(false, false)
    }

    /// The pretty-bhns case end to end: download, verify, unpack, fix the
    /// `.info`'s placeholder to the tree's own absolute path, point the knob
    /// at the `.info`; a second fetch reuses the tree without a request.
    #[test]
    fn an_archive_is_unpacked_fixed_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gz(&gallery_tar());
        // HEAD for the size, then GET.
        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let input = input(format!("{base}/bhns_gw230529_ID.tar.gz"), &archive);
        let value = fetch(dir.path(), &input, &quiet()).unwrap();
        server.join().unwrap();
        let tree = dir.path().join("unpacked").join(tree_key(&input));
        assert_eq!(value, tree.join("bhns_gw230529_ID/X.info"));
        let info = std::fs::read_to_string(&value).unwrap();
        let want = format!("eos = {}/bhns_gw230529_ID/gam2.polytrope\nother = 1\n", tree.display());
        assert_eq!(info, want);
        assert_eq!(std::fs::read(tree.join("bhns_gw230529_ID/gam2.polytrope")).unwrap(), b"polytrope\n");
        // Nothing left over: no download, no staging directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("unpacked")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        // Reused: nobody is listening any more.
        assert_eq!(fetch(dir.path(), &input, &quiet()).unwrap(), value);
        // Different fixups make a different tree.
        let mut other = input.clone();
        other.fixups.clear();
        assert_ne!(tree_key(&other), tree_key(&input));
    }

    /// A wrong hash unpacks nothing; a fixup whose text is absent, or a
    /// `path` the archive lacks, leaves no tree behind.
    #[test]
    fn nothing_unverified_or_half_done_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gz(&gallery_tar());
        let check = |edit: &dyn Fn(&mut Input), expect: &str| {
            let (base, server) = serve(vec![("*", archive.clone())], 2);
            let mut input = input(format!("{base}/id.tar.gz"), &archive);
            edit(&mut input);
            let err = format!("{:#}", fetch(dir.path(), &input, &quiet()).unwrap_err());
            assert!(err.contains(expect), "{err}");
            server.join().unwrap();
            let trees: Vec<_> = std::fs::read_dir(dir.path().join("unpacked")).unwrap().flatten().map(|e| e.file_name()).collect();
            assert!(trees.is_empty(), "{trees:?}");
        };
        check(&|i| i.sha256 = Some("0".repeat(64)), "nothing was unpacked");
        check(&|i| i.fixups[0].replace = "/not/there".into(), "does not contain");
        check(&|i| i.path = Some("bhns_gw230529_ID/missing.info".into()), "has no bhns_gw230529_ID/missing.info");
    }

    /// Entries that would reach outside the tree are refused: `..`, an
    /// absolute link, a link climbing out, and a file written through a link.
    #[test]
    fn archives_cannot_escape_their_tree() {
        let raw_entry = |b: &mut tar::Builder<Vec<u8>>, name: &[u8], ty: tar::EntryType, link: Option<&str>, body: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(ty);
            if let Some(link) = link {
                h.set_link_name(link).unwrap();
            }
            h.set_cksum();
            b.append(&h, body).unwrap();
        };
        let cases: Vec<(Vec<u8>, &str)> = vec![
            ({ let mut b = tar::Builder::new(Vec::new()); raw_entry(&mut b, b"../evil", tar::EntryType::Regular, None, b"x"); b.into_inner().unwrap() }, "outside its own tree"),
            ({ let mut b = tar::Builder::new(Vec::new()); raw_entry(&mut b, b"/etc/evil", tar::EntryType::Regular, None, b"x"); b.into_inner().unwrap() }, "outside its own tree"),
            ({ let mut b = tar::Builder::new(Vec::new()); raw_entry(&mut b, b"l", tar::EntryType::Symlink, Some("/etc"), b""); b.into_inner().unwrap() }, "outside its own tree"),
            ({ let mut b = tar::Builder::new(Vec::new()); raw_entry(&mut b, b"d/l", tar::EntryType::Symlink, Some("../../x"), b""); b.into_inner().unwrap() }, "outside its own tree"),
            ({
                let mut b = tar::Builder::new(Vec::new());
                raw_entry(&mut b, b"l", tar::EntryType::Symlink, Some("sub"), b"");
                raw_entry(&mut b, b"l/f", tar::EntryType::Regular, None, b"x");
                b.into_inner().unwrap()
            }, "both a directory and a file at l"),
            // A chain of links, each lexically inside, that resolves outside
            // (both reviewers' archives): caught where the links really lead.
            ({
                let mut b = tar::Builder::new(Vec::new());
                raw_entry(&mut b, b"x", tar::EntryType::Symlink, Some("."), b"");
                raw_entry(&mut b, b"y", tar::EntryType::Symlink, Some("x/x/../.."), b"");
                b.into_inner().unwrap()
            }, "outside its own tree"),
            ({
                let mut b = tar::Builder::new(Vec::new());
                raw_entry(&mut b, b"y", tar::EntryType::Symlink, Some("."), b"");
                raw_entry(&mut b, b"p1", tar::EntryType::Symlink, Some("y/.."), b"");
                raw_entry(&mut b, b"p2", tar::EntryType::Symlink, Some("p1/.."), b"");
                b.into_inner().unwrap()
            }, "outside its own tree"),
            // A dangling link that climbs out past its existing ancestor.
            ({
                let mut b = tar::Builder::new(Vec::new());
                raw_entry(&mut b, b"x", tar::EntryType::Symlink, Some("."), b"");
                raw_entry(&mut b, b"d", tar::EntryType::Symlink, Some("x/x/../../missing"), b"");
                b.into_inner().unwrap()
            }, "outside its own tree"),
            // A hard link may only name a regular file already unpacked.
            ({
                let mut b = tar::Builder::new(Vec::new());
                raw_entry(&mut b, b"x", tar::EntryType::Symlink, Some("."), b"");
                raw_entry(&mut b, b"h", tar::EntryType::Link, Some("x/victim"), b"");
                b.into_inner().unwrap()
            }, "not a file it unpacked before"),
        ];
        for (tar_bytes, expect) in cases {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("a.tar");
            std::fs::write(&file, &tar_bytes).unwrap();
            let dest = dir.path().join("out");
            std::fs::create_dir(&dest).unwrap();
            let err = format!("{:#}", unpack(&file, ArchiveKind::Tar, &dest, "a.tar", &mut prodash::progress::Discard).unwrap_err());
            assert!(err.contains(expect), "{expect}: {err}");
        }
        // A link that stays inside is fine.
        let mut b = tar::Builder::new(Vec::new());
        raw_entry(&mut b, b"d/f", tar::EntryType::Regular, None, b"x");
        raw_entry(&mut b, b"d/l", tar::EntryType::Symlink, Some("../d/f"), b"");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tar");
        std::fs::write(&file, b.into_inner().unwrap()).unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        unpack(&file, ArchiveKind::Tar, &dest, "a.tar", &mut prodash::progress::Discard).unwrap();
        assert_eq!(std::fs::read(dest.join("d/l")).unwrap(), b"x");
    }

    /// A fixup edits a read-only member (keeping its mode), refuses one it
    /// would reach through a link, and an incomplete tree (a scratch purge
    /// keeps directories) is unpacked again rather than reused.
    #[test]
    fn fixups_reach_only_real_members_and_purged_trees_are_redone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(INFO.len() as u64);
        h.set_mode(0o444);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, "bhns_gw230529_ID/X.info", INFO).unwrap();
        let mut l = tar::Header::new_gnu();
        l.set_size(0);
        l.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut l, "alias", "bhns_gw230529_ID").unwrap();
        let archive = gz(&b.into_inner().unwrap());

        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let good = input(format!("{base}/id.tar.gz"), &archive);
        let value = fetch(dir.path(), &good, &quiet()).unwrap();
        server.join().unwrap();
        assert!(std::fs::read_to_string(&value).unwrap().contains("/bhns_gw230529_ID/gam2.polytrope"));
        assert_eq!(std::fs::metadata(&value).unwrap().permissions().mode() & 0o777, 0o444, "mode kept");

        // Purged: the files go, the directories stay; the next fetch redoes it.
        let tree = dir.path().join("unpacked").join(tree_key(&good));
        std::fs::remove_file(tree.join(COMPLETE_MARKER)).unwrap();
        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let again = Input { url: Some(format!("{base}/id.tar.gz")), ..good.clone() };
        assert_eq!(fetch(dir.path(), &again, &quiet()).unwrap(), value);
        server.join().unwrap();
        assert!(tree.join(COMPLETE_MARKER).is_file());

        // A fixup that names the file through the link is refused.
        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let mut through = input(format!("{base}/id.tar.gz"), &archive);
        through.fixups[0].file = "alias/X.info".into();
        through.path = None;
        let err = format!("{:#}", fetch(dir.path(), &through, &quiet()).unwrap_err());
        assert!(err.contains("not a directory of the archive"), "{err}");
        server.join().unwrap();
    }

    /// Only a line that differs by naming the author's directory is a
    /// fixup; anything else is not, and the whole line is what is replaced.
    #[test]
    fn line_fixups_take_only_path_differences() {
        let root = "/work/max/bhns";
        let theirs = b"a = 1\neos = /path/to/gam2.polytrope\n";
        let mine = b"a = 1\neos = /work/max/bhns/bhns_gw230529_ID/gam2.polytrope\n";
        assert_eq!(
            line_fixups(theirs, mine, root).unwrap(),
            [("eos = /path/to/gam2.polytrope".to_owned(), "eos = {dir}/bhns_gw230529_ID/gam2.polytrope".to_owned())]
        );
        assert!(line_fixups(b"a = 1\n", b"a = 2\n", root).is_none(), "not a path change");
        assert!(line_fixups(b"a\n", b"a\nb\n", root).is_none(), "a line added");
        assert!(line_fixups(&[0xff, 0xfe], b"x", root).is_none(), "not text");
        assert!(line_fixups(b"x = 1\n", b"x = /work/max/bhns/{y}\n", root).is_none(), "a brace a fixup cannot hold");
    }

    /// The pretty-bhns case from the author's side: the gallery tarball, and
    /// the author's own unpacked copy with the placeholder replaced. The
    /// knob's member and the fixup are found, and fetching the input as an
    /// importer reproduces the author's edit in the importer's own tree.
    #[test]
    fn an_author_s_copy_becomes_path_and_fixup() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gz(&gallery_tar());
        let mine = dir.path().join("work/bhns");
        std::fs::create_dir_all(mine.join("bhns_gw230529_ID")).unwrap();
        let edited = format!("eos = {}/bhns_gw230529_ID/gam2.polytrope\nother = 1\n", mine.display());
        std::fs::write(mine.join("bhns_gw230529_ID/X.info"), &edited).unwrap();
        std::fs::write(mine.join("bhns_gw230529_ID/gam2.polytrope"), b"polytrope\n").unwrap();
        let local = mine.join("bhns_gw230529_ID/X.info");

        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let url = format!("{base}/bhns_gw230529_ID.tar.gz");
        let mut input = Input {
            knob: "kadath-initial-data".into(),
            description: None,
            url: None,
            sha256: None,
            unpack: false,
            path: None,
            fixups: vec![],
        };
        let how = Authoring { url: &url, local: Some(&local), member: None, interactive: false };
        let notes = author(&dir.path().join("inputs"), &mut input, &how, &quiet()).unwrap();
        server.join().unwrap();
        assert!(input.unpack);
        assert_eq!(input.sha256.as_deref(), Some(sha(&archive).as_str()));
        assert_eq!(input.path.as_deref(), Some("bhns_gw230529_ID/X.info"));
        assert_eq!(input.fixups.len(), 1, "{notes:?}");
        assert_eq!(input.fixups[0].with, "eos = {dir}/bhns_gw230529_ID/gam2.polytrope");

        let (base, server) = serve(vec![("*", archive.clone())], 2);
        input.url = Some(format!("{base}/bhns_gw230529_ID.tar.gz"));
        let value = fetch(&dir.path().join("importer"), &input, &quiet()).unwrap();
        server.join().unwrap();
        let tree = value.parent().unwrap().parent().unwrap();
        let expected = format!("eos = {}/bhns_gw230529_ID/gam2.polytrope\nother = 1\n", tree.display());
        assert_eq!(std::fs::read_to_string(&value).unwrap(), expected);
    }

    /// `-y` is not held to a size the server under-reports (a HEAD saying
    /// 0 bytes); without it, the announced size bounds the download.
    #[test]
    fn yes_lifts_an_announced_size() {
        let dir = tempfile::tempdir().unwrap();
        // Small enough to fit a socket buffer: the test server answers the
        // HEAD with the body too, and a big one breaks its pipe.
        let body = vec![b'x'; 48 << 10];
        let (base, server) = crate::fetch::download::test_server::serve_with(4, move |_| {
            ("200 OK", String::new(), body.clone())
        });
        let input = |sha: String| Input {
            knob: "data".into(),
            description: None,
            url: Some(format!("{base}/data.bin")),
            sha256: Some(sha),
            unpack: false,
            path: None,
            fixups: vec![],
        };
        let want = sha(&vec![b'x'; 48 << 10]);
        fetch(dir.path(), &input(want.clone()), &SizePolicy { interactive: false, yes: true, confirm_above: 10 }).unwrap();
        // Without -y and above the threshold: refused before the GET.
        let err = format!("{:#}", fetch(&dir.path().join("b"), &input(want), &SizePolicy { interactive: false, yes: false, confirm_above: 10 }).unwrap_err());
        assert!(err.contains("pass -y"), "{err}");
        drop(server);
    }

    /// A tar long name (read whole, in memory, by the tar reader) is cut off
    /// past 1 MiB rather than taking gigabytes of memory.
    #[test]
    fn a_huge_tar_header_is_refused() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        let long = "d/".repeat(1 << 20) + "f";
        b.append_data(&mut h, &long, &b"x"[..]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tar");
        std::fs::write(&file, b.into_inner().unwrap()).unwrap();
        let dest = dir.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let err = format!("{:#}", unpack(&file, ArchiveKind::Tar, &dest, "a.tar", &mut prodash::progress::Discard).unwrap_err());
        assert!(err.contains("bigger than cactup reads"), "{}", &err[..err.len().min(300)]);
    }

    /// An archive member named like cactup's completion marker is refused,
    /// not written through.
    #[test]
    fn the_marker_name_is_cactup_s() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, "data.txt", &b"x"[..]).unwrap();
        let mut l = tar::Header::new_gnu();
        l.set_size(0);
        l.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut l, COMPLETE_MARKER, "data.txt").unwrap();
        let archive = gz(&b.into_inner().unwrap());
        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let mut input = input(format!("{base}/a.tar.gz"), &archive);
        input.fixups.clear();
        input.path = None;
        let err = format!("{:#}", fetch(dir.path(), &input, &quiet()).unwrap_err());
        assert!(err.contains("keeps for itself"), "{err}");
        server.join().unwrap();
    }

    /// Every supported kind unpacks the same tree.
    #[test]
    fn tar_xz_and_zip_unpack_too() {
        let tar_bytes = gallery_tar();
        let mut xz = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        xz.write_all(&tar_bytes).unwrap();
        let xz = xz.finish().unwrap();
        let mut zip_bytes = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut zip_bytes);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.add_directory("bhns_gw230529_ID/", opts).unwrap();
            z.start_file("bhns_gw230529_ID/X.info", opts).unwrap();
            z.write_all(INFO).unwrap();
            z.start_file("bhns_gw230529_ID/gam2.polytrope", opts).unwrap();
            z.write_all(b"polytrope\n").unwrap();
            z.finish().unwrap();
        }
        for (kind, bytes) in [(ArchiveKind::Tar, tar_bytes.clone()), (ArchiveKind::TarXz, xz), (ArchiveKind::Zip, zip_bytes.into_inner())] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("a");
            std::fs::write(&file, &bytes).unwrap();
            let dest = dir.path().join("out");
            std::fs::create_dir(&dest).unwrap();
            unpack(&file, kind, &dest, "a", &mut prodash::progress::Discard).unwrap_or_else(|e| panic!("{kind:?}: {e:#}"));
            assert_eq!(std::fs::read(dest.join("bhns_gw230529_ID/X.info")).unwrap(), INFO, "{kind:?}");
        }
    }

    /// A download above the threshold is refused without a terminal, and
    /// taken with -y.
    #[test]
    fn a_big_download_needs_a_yes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gz(&gallery_tar());
        let (base, server) = serve(vec![("*", archive.clone())], 1);
        let input = input(format!("{base}/id.tar.gz"), &archive);
        let strict = SizePolicy { interactive: false, yes: false, confirm_above: 10 };
        let err = format!("{:#}", fetch(dir.path(), &input, &strict).unwrap_err());
        assert!(err.contains("pass -y"), "{err}");
        server.join().unwrap();
        let (base, server) = serve(vec![("*", archive.clone())], 2);
        let input = self::input(format!("{base}/id.tar.gz"), &archive);
        fetch(dir.path(), &input, &SizePolicy { interactive: false, yes: true, confirm_above: 10 }).unwrap();
        server.join().unwrap();
    }
}
