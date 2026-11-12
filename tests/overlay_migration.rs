//! Concurrent loads of one stale user-MDB overlay (design/mdb-overlay-migration-plan.md
//! §7 P1): eight `cactup machine show` processes on one scratch home must all
//! succeed, exactly one may say it updated the machine, one backup results,
//! and the files end up as a single process leaves them.
//!
//! The binary is a dev build (it reads the repo's `mdb/`, never syncs or
//! updates itself), and every run gets its own scratch `CACTUP_HOME`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CACTUP: &str = env!("CARGO_BIN_EXE_cactup");

fn cactup(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(CACTUP);
    cmd.args(args).env("CACTUP_HOME", home).env("NO_COLOR", "1");
    cmd
}

fn check(out: Output, what: &str) -> Output {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Every file under `dir`, relative, with its bytes.
fn contents(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                out.push((path.strip_prefix(root).unwrap().to_owned(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn concurrent_loads_migrate_an_overlay_once() {
    let scratch = tempfile::tempdir().unwrap();
    let (home, reference) = (scratch.path().join("home"), scratch.path().join("reference"));

    // A current overlay, made by the real command, then marked one
    // generation older: the step to the current generation only raises the
    // number, so this is what an older binary left behind.
    check(
        cactup(&home, &["--hostname", "box.example.org", "machine", "create", "box", "--silent", "--no-discover"])
            .output()
            .unwrap(),
        "machine create",
    );
    let meta = home.join("machines/box/meta.toml");
    let text = fs::read_to_string(&meta).unwrap();
    let current = text.lines().find(|l| l.starts_with("mdb-generation = ")).expect("create stamps the generation");
    let want: u32 = current["mdb-generation = ".len()..].trim().parse().unwrap();
    fs::write(&meta, text.replace(current, &format!("mdb-generation = {}", want - 1))).unwrap();

    // The same overlay in a second home, for the single-process run.
    fs::create_dir_all(reference.join("machines/box")).unwrap();
    for (rel, bytes) in contents(&home.join("machines/box")) {
        let path = reference.join("machines/box").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    let children: Vec<_> = (0..8)
        .map(|_| {
            cactup(&home, &["machine", "show", "box"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<Output> =
        children.into_iter().map(|child| check(child.wait_with_output().unwrap(), "machine show")).collect();
    let updated = outputs.iter().filter(|o| String::from_utf8_lossy(&o.stderr).contains("Updated user-MDB machine")).count();
    assert_eq!(updated, 1, "exactly one process migrates");
    for out in &outputs {
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(&format!("mdb generation: {want}")), "{stdout}");
        assert!(stdout.contains("earlier generations kept in:"), "{stdout}");
    }

    // One backup, finished; no lock left behind.
    let backups: Vec<_> = fs::read_dir(home.join("machine-backups/box")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(backups, [format!("generation-{}", want - 1)]);
    assert!(home.join(format!("machine-backups/box/generation-{}/.complete", want - 1)).is_file());
    assert!(!home.join("machines/.box.migrate.lock").exists());

    // The result is what one process alone makes of the same overlay.
    let single = check(cactup(&reference, &["machine", "show", "box"]).output().unwrap(), "single machine show");
    assert!(String::from_utf8_lossy(&single.stderr).contains("Updated user-MDB machine"));
    assert_eq!(contents(&home.join("machines/box")), contents(&reference.join("machines/box")));
    assert_eq!(contents(&home.join("machine-backups/box")), contents(&reference.join("machine-backups/box")));
}
