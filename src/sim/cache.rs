//! The executable hard-link cache and TRASH (§8.1, §8.7): one physical copy
//! per `build-id` under `<sim-home>/CACHE/exe/<build-id>`, keyed by build-id
//! (not a content hash). GC reaps an entry once its on-disk link count shows
//! no live-or-trashed simulation still references it.

use crate::Res;
use anyhow::Context;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn cache_dir(sim_home: &Path) -> PathBuf {
    sim_home.join("CACHE").join("exe")
}

pub fn trash_dir(sim_home: &Path) -> PathBuf {
    sim_home.join("TRASH")
}

/// Ensure `CACHE/exe/<build-id>` exists, populating it from `exe_src` (hard
/// link when same-filesystem, else copy — the Cactus tree and sim-home are
/// frequently on different filesystems, §8.1). Returns the cache entry path.
pub fn ensure_cached(sim_home: &Path, build_id: &str, exe_src: &Path) -> Res<PathBuf> {
    let dir = cache_dir(sim_home);
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let entry = dir.join(build_id);
    if entry.is_file() {
        return Ok(entry);
    }
    if fs::hard_link(exe_src, &entry).is_err() {
        // Cross-filesystem (or a source that forbids links): copy via a temp
        // name so a concurrent create never sees a half-written binary.
        let temp = dir.join(format!(".{build_id}.{}", std::process::id()));
        fs::copy(exe_src, &temp)
            .with_context(|| format!("Failed to copy {} into the cache", exe_src.display()))?;
        if let Err(e) = fs::rename(&temp, &entry) {
            let _ = fs::remove_file(&temp);
            // A concurrent create may have won the rename; that's fine.
            if !entry.is_file() {
                return Err(e).with_context(|| format!("Failed to move {} into place", entry.display()));
            }
        }
    }
    Ok(entry)
}

/// Hard-link a cache entry to `dst` (the sim's `.cactup/exe`), falling back
/// to a plain copy when the sim dir is on a different filesystem (§8.1).
/// Ensures the result is executable.
pub fn link_into(cache_entry: &Path, dst: &Path) -> Res<()> {
    if dst.exists() {
        fs::remove_file(dst).with_context(|| format!("Failed to replace {}", dst.display()))?;
    }
    if fs::hard_link(cache_entry, dst).is_err() {
        fs::copy(cache_entry, dst)
            .with_context(|| format!("Failed to copy {} to {}", cache_entry.display(), dst.display()))?;
    }
    let mut perms = fs::metadata(dst)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(dst, perms)
        .with_context(|| format!("Failed to make {} executable", dst.display()))?;
    Ok(())
}

/// Garbage-collect orphaned cache entries (§8.1): an entry whose only
/// remaining hard link is the cache entry itself (nlink == 1) is referenced
/// by no live sim and no trashed sim, and is reaped. `keep` protects the
/// entry currently being (re)created. Returns the reaped build-ids.
pub fn gc(sim_home: &Path, keep: Option<&str>) -> Res<Vec<String>> {
    let dir = cache_dir(sim_home);
    let mut reaped = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(reaped),
        other => other.with_context(|| format!("Failed to read {}", dir.display()))?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || Some(name.as_str()) == keep {
            continue; // temp files / the entry being created
        }
        let meta = entry.metadata()?;
        if meta.is_file() && meta.nlink() == 1 {
            fs::remove_file(entry.path())
                .with_context(|| format!("Failed to reap cache entry {}", name))?;
            reaped.push(name);
        }
    }
    Ok(reaped)
}

/// Recursive copy for the cross-filesystem trash fallback (§8.7).
fn copy_dir_all(src: &Path, dst: &Path) -> Res<()> {
    fs::create_dir_all(dst).with_context(|| format!("Failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("Failed to read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if ty.is_symlink() {
            let target = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(target, &to)?;
        } else {
            fs::copy(entry.path(), &to)
                .with_context(|| format!("Failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// `shutil.move`-equivalent of the whole simulation dir into
/// `TRASH/<simulation-id>/` (§8.7); degrades to copy-then-delete across
/// filesystems. Returns the trash destination.
pub fn move_to_trash(sim_home: &Path, sim_dir: &Path, simulation_id: &str) -> Res<PathBuf> {
    let trash = trash_dir(sim_home);
    fs::create_dir_all(&trash).with_context(|| format!("Failed to create {}", trash.display()))?;
    let dst = trash.join(simulation_id);
    if fs::rename(sim_dir, &dst).is_err() {
        copy_dir_all(sim_dir, &dst)?;
        fs::remove_dir_all(sim_dir)
            .with_context(|| format!("Failed to remove {} after trashing", sim_dir.display()))?;
    }
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_populate_link_and_gc() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let exe_src = home.join("cactus_sim");
        fs::write(&exe_src, b"#!/bin/sh\necho cactus").unwrap();

        let entry = ensure_cached(home, "build-1", &exe_src).unwrap();
        assert!(entry.is_file());
        // Idempotent.
        assert_eq!(ensure_cached(home, "build-1", &exe_src).unwrap(), entry);

        // Link into a "sim": entry now has ≥ 2 links (src + cache + sim)…
        let sim_exe = home.join("exe-link");
        link_into(&entry, &sim_exe).unwrap();
        assert!(fs::metadata(&sim_exe).unwrap().permissions().mode() & 0o111 != 0);

        // …so GC keeps it, but reaps an orphaned build.
        fs::write(cache_dir(home).join("build-0"), b"old").unwrap();
        let reaped = gc(home, None).unwrap();
        assert_eq!(reaped, vec!["build-0".to_owned()]);
        assert!(entry.is_file());

        // Dropping the last sim link (and the original source) orphans it.
        fs::remove_file(&sim_exe).unwrap();
        fs::remove_file(&exe_src).unwrap();
        // `keep` protects it…
        assert!(gc(home, Some("build-1")).unwrap().is_empty());
        // …and without protection it is reaped.
        assert_eq!(gc(home, None).unwrap(), vec!["build-1".to_owned()]);
        assert!(!entry.exists());
    }

    #[test]
    fn trash_move() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let sim = home.join("sim").join("bbh");
        fs::create_dir_all(sim.join(".cactup")).unwrap();
        fs::write(sim.join("log.txt"), b"[LOG] hi").unwrap();

        let dst = move_to_trash(home, &sim, "simulation-bbh-x").unwrap();
        assert!(!sim.exists());
        assert_eq!(dst, trash_dir(home).join("simulation-bbh-x"));
        assert!(dst.join("log.txt").is_file());
    }
}
