//! Build script:
//!
//! - parse and validate `resources/wisdom.txt` (spec §16) and generate the
//!   static corpus slices the `wisdom` command embeds. A malformed corpus
//!   fails the build itself, not just the test suite.
//! - stamp the build's identity into `build_info_gen.rs` (`src/build_info.rs`
//!   includes it): whether this is a CI distribution build (`CACTUP_DIST=1`
//!   with `CACTUP_BUILD_ID` and `CACTUP_BUILD_DATE`) or a dev build, the MDB
//!   generation from `mdb/GENERATION`, the target triple, and a content hash
//!   of the embedded `mdb/generic`.

use std::path::{Path, PathBuf};

include!("src/wisdom_parse.rs");

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set"));
    wisdom(&out);
    build_info(&out);
}

fn wisdom(out: &Path) {
    println!("cargo:rerun-if-changed=resources/wisdom.txt");
    println!("cargo:rerun-if-changed=src/wisdom_parse.rs");

    let raw = std::fs::read_to_string("resources/wisdom.txt")
        .expect("failed to read resources/wisdom.txt");
    let (tips, zens) = parse_wisdom(&raw);

    assert!(tips.len() >= 10, "wisdom.txt: suspiciously few tips ({})", tips.len());
    assert!(zens.len() >= 5, "wisdom.txt: suspiciously few zen entries ({})", zens.len());
    for entry in tips.iter().chain(&zens) {
        assert!(!entry.contains('\t'), "wisdom.txt: tabs render unpredictably: {entry:?}");
        for line in entry.lines() {
            assert!(line.chars().count() <= 100, "wisdom.txt: line too wide: {line:?}");
        }
    }
    for zen in &zens {
        assert!(zen.contains('—'), "wisdom.txt: zen entry lacks an — Name attribution: {zen:?}");
    }

    let literals = |entries: &[String]| {
        entries.iter().map(|e| format!("    {e:?},\n")).collect::<String>()
    };
    let generated = format!(
        "static TIPS: &[&str] = &[\n{}];\nstatic ZENS: &[&str] = &[\n{}];\n",
        literals(&tips),
        literals(&zens),
    );
    std::fs::write(out.join("wisdom_gen.rs"), generated).expect("failed to write wisdom_gen.rs");
}

/// A set, non-empty environment variable.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn build_info(out: &Path) {
    for var in ["CACTUP_DIST", "CACTUP_BUILD_ID", "CACTUP_BUILD_DATE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=mdb/GENERATION");
    // A directory: cargo rescans it recursively for changed mtimes.
    println!("cargo:rerun-if-changed=mdb/generic");

    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set");
    let target = std::env::var("TARGET").expect("TARGET is set");

    let raw = std::fs::read_to_string("mdb/GENERATION").expect("failed to read mdb/GENERATION");
    let generation: u32 = match raw.trim().parse() {
        Ok(n) if n >= 1 => n,
        _ => panic!("mdb/GENERATION must hold one positive integer, found {raw:?}"),
    };

    // The stamp CI sets for a distribution build. Anything else — every
    // local cargo build, debug or release — is a dev build.
    let dist = match env_nonempty("CACTUP_DIST").as_deref() {
        None => None,
        Some("1") => {
            let id = env_nonempty("CACTUP_BUILD_ID").unwrap_or_default();
            let date = env_nonempty("CACTUP_BUILD_DATE").unwrap_or_default();
            let hex = id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            assert!(
                hex && (7..=40).contains(&id.len()),
                "CACTUP_DIST=1 needs CACTUP_BUILD_ID set to a 7-40 digit lowercase hex commit id, got {id:?}"
            );
            // `git log --format=%cI`: strict ISO 8601, so the day is its first
            // ten characters.
            let day = date.get(..10).unwrap_or("");
            let day_ok = day.len() == 10
                && day
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
            assert!(
                day_ok,
                "CACTUP_DIST=1 needs CACTUP_BUILD_DATE set to an ISO 8601 committer date (git's %cI), got {date:?}"
            );
            Some((id, date))
        }
        Some(other) => panic!("CACTUP_DIST must be 1 or unset, got {other:?}"),
    };
    if dist.is_none()
        && (env_nonempty("CACTUP_BUILD_ID").is_some() || env_nonempty("CACTUP_BUILD_DATE").is_some())
    {
        println!("cargo:warning=CACTUP_BUILD_ID/CACTUP_BUILD_DATE are ignored without CACTUP_DIST=1");
    }

    let generic_hash = hash_dir(Path::new("mdb/generic"));

    let (dist_literal, long_version, user_agent) = match &dist {
        Some((id, date)) => (
            format!("Some(Stamp {{ id: {id:?}, date: {date:?} }})"),
            format!("{version} ({id} {}, mdb generation {generation})", &date[..10]),
            format!("cactup/{version}+{id}"),
        ),
        None => (
            "None".to_owned(),
            format!("{version} (dev build, mdb generation {generation})"),
            format!("cactup/{version}+dev"),
        ),
    };
    let generated = format!(
        "pub const DIST: Option<Stamp> = {dist_literal};\n\
         pub const MDB_GENERATION: u32 = {generation};\n\
         pub const TARGET: &str = {target:?};\n\
         pub const GENERIC_HASH: &str = {generic_hash:?};\n\
         pub const LONG_VERSION: &str = {long_version:?};\n\
         pub const USER_AGENT: &str = {user_agent:?};\n"
    );
    std::fs::write(out.join("build_info_gen.rs"), generated).expect("failed to write build_info_gen.rs");
}

/// FNV-1a 64 over the sorted relative paths and bytes of every file under
/// `root` (each followed by a NUL), skipping Python caches — the same
/// fingerprint `cactup machine create` records for `--from-existing`.
fn hash_dir(root: &Path) -> String {
    fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to list {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("readable directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "__pycache__" || name.ends_with(".pyc") {
                continue;
            }
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                collect(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).expect("child of root").to_string_lossy().into_owned());
            }
        }
    }
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    };
    for rel in files {
        feed(rel.as_bytes());
        feed(&[0]);
        let path = root.join(&rel);
        feed(&std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display())));
        feed(&[0]);
    }
    format!("{hash:016x}")
}
