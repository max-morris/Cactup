//! Where a compile runs: the part of a cache key that keeps objects built
//! for one kind of machine away from another (D15).
//!
//! One `$CACTUP_HOME` can be on a filesystem that several machines mount —
//! different clusters, or nodes of one cluster with different processors.
//! Two things key an object to its place:
//!
//! - what `prepare` froze: the cactup **machine** the build was prepared
//!   for, its build **universe**, and a digest of the build-phase
//!   environment setup (the machine's module loads);
//! - what the wrapper finds on the host that actually compiles: the
//!   processor architecture, the processors' models and feature flags, and
//!   the operating system release.
//!
//! A host's processors need not be alike (performance and efficiency cores,
//! big.LITTLE). All their kinds are in the key; and since `-march=native`
//! means the core the compiler happens to run on, a compile that asks for
//! it on such a host has no one object to cache ([`Platform::mixed`]).
//!
//! The second half is there because the first can be wrong or too coarse.
//! Machine detection keeps the last machine when nobody claims a host, and
//! `generic` covers every host nobody claims; and one machine's login and
//! compute nodes may differ. `-march=native`, `-xHost`, and compilers that
//! tune for the build host without being asked, all make the processor part
//! of what an object is — so it is always in the key, not only when a flag
//! says so.

use super::hash::{bytes_digest, Hasher};
use super::BuildConf;
use crate::Res;
use anyhow::Context;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// The `/proc/cpuinfo` fields that say what kind of processor one is, on
/// x86 and on arm64 — the cache size among them, since `-march=native`
/// hands it to the optimizer as a parameter. Not its speed, its number, or
/// its microcode revision: those vary between identical nodes and across
/// reboots.
const CPU_FIELDS: &[&str] = &[
    "vendor_id", "cpu family", "model", "model name", "stepping", "cache size", "flags", // x86
    "CPU implementer", "CPU architecture", "CPU variant", "CPU part", "CPU revision", "Features", // arm64
];

/// The kinds of processor in `cpuinfo` (the text of `/proc/cpuinfo`): one
/// entry per distinct kind, sorted, so that a machine with performance and
/// efficiency cores reads the same whichever is listed first. `caches`
/// gives what else is known of a processor by its number: its caches,
/// which `/proc/cpuinfo` shows only the last level of.
fn processor_kinds(cpuinfo: &str, caches: impl Fn(&str) -> String) -> BTreeSet<String> {
    cpuinfo
        .split("\n\n")
        .map(|processor| {
            let mut number = "";
            let fields = processor.lines().filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                let name = name.trim();
                if name == "processor" {
                    number = value.trim();
                }
                // Feature flags in a fixed order: the kernel's is by bit
                // number, which is stable, but nothing promises it.
                let mut words: Vec<&str> = value.split_whitespace().collect();
                if matches!(name, "flags" | "Features") {
                    words.sort_unstable();
                }
                CPU_FIELDS.contains(&name).then(|| format!("{name}={}", words.join(" ")))
            });
            let fields = fields.collect::<Vec<_>>().join("\n");
            if fields.is_empty() { fields } else { format!("{fields}\ncaches={}", caches(number)) }
        })
        .filter(|kind| !kind.is_empty())
        .collect()
}

/// The caches of processor `number` as the kernel describes them
/// (`/sys/devices/system/cpu/cpu<N>/cache/index<M>/`): level, type, size
/// and line size of each. Empty where the kernel does not say.
fn caches_of(number: &str) -> String {
    let dir = Path::new("/sys/devices/system/cpu").join(format!("cpu{number}")).join("cache");
    let mut caches = Vec::new();
    for index in 0.. {
        let index = dir.join(format!("index{index}"));
        let read = |file: &str| fs::read_to_string(index.join(file)).map(|text| text.trim().to_owned());
        let Ok(level) = read("level") else { break };
        let rest = ["type", "size", "coherency_line_size"].map(|file| read(file).unwrap_or_default());
        caches.push(format!("L{level} {}", rest.join(" ")));
    }
    caches.join(", ")
}

/// What the host itself says it is.
struct Host {
    fingerprint: String,
    /// Its processors are not all of one kind.
    mixed: bool,
}

/// The host half of the platform, read from the host itself.
fn host() -> Res<Host> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").context("Failed to read /proc/cpuinfo")?;
    let kinds = processor_kinds(&cpuinfo, caches_of);
    anyhow::ensure!(!kinds.is_empty(), "/proc/cpuinfo names no processor cactup can read");
    // Present on every current distribution, and inside a container it is
    // the container's: a build in another universe is another platform.
    let os = fs::read("/etc/os-release").or_else(|_| fs::read("/usr/lib/os-release")).context("Failed to read /etc/os-release")?;

    let mut hasher = Hasher::new("host");
    hasher.feed(std::env::consts::ARCH.as_bytes());
    for kind in &kinds {
        hasher.feed(kind.as_bytes());
    }
    hasher.feed(&os);
    Ok(Host { fingerprint: hasher.hex(), mixed: kinds.len() > 1 })
}

/// Where a compile runs, as its key has it.
pub struct Platform {
    pub digest: String,
    /// The host's processors are not all of one kind: what a compiler makes
    /// of "this processor" depends on the one it finds itself on.
    pub mixed: bool,
}

/// The platform of a compile of this build on this host.
///
/// Reading `/proc/cpuinfo` is not free on a large node (the kernel builds
/// it per core, on request), so the host half is kept in `<attempt>/cc/`
/// for the build attempt — per host and per boot, should one attempt's
/// compiles ever run on more than one.
pub fn platform(conf: &BuildConf, cc_dir: &Path) -> Res<Platform> {
    let read = |path: &str| fs::read_to_string(path).with_context(|| format!("Failed to read {path}"));
    let name = read("/proc/sys/kernel/hostname")?;
    let boot = read("/proc/sys/kernel/random/boot_id")?;
    let memo = cc_dir.join("hosts").join(bytes_digest(format!("{name}{boot}").as_bytes()));
    // `<fingerprint> mixed` or `<fingerprint> uniform`.
    let remembered = fs::read_to_string(&memo).ok().and_then(|text| {
        let (fingerprint, kinds) = text.split_once(' ')?;
        let mixed = match kinds {
            "mixed" => true,
            "uniform" => false,
            _ => return None,
        };
        (fingerprint.len() == 64).then(|| Host { fingerprint: fingerprint.to_owned(), mixed })
    });
    let host = match remembered {
        Some(host) => host,
        None => {
            let host = host()?;
            // Best-effort, written whole and moved into place.
            if let Some(dir) = memo.parent()
                && fs::create_dir_all(dir).is_ok()
                && let Ok(mut temp) = tempfile::NamedTempFile::new_in(dir)
            {
                use std::io::Write;
                let text = format!("{} {}", host.fingerprint, if host.mixed { "mixed" } else { "uniform" });
                if temp.write_all(text.as_bytes()).is_ok() {
                    let _ = temp.persist(&memo);
                }
            }
            host
        }
    };

    let mut hasher = Hasher::new("platform");
    hasher.feed(conf.machine.as_bytes());
    hasher.feed(conf.universe.as_deref().unwrap_or_default().as_bytes());
    hasher.feed(conf.build_env_digest.as_bytes());
    hasher.feed(host.fingerprint.as_bytes());
    Ok(Platform { digest: hasher.hex(), mixed: host.mixed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objcache::Mode;
    use std::path::PathBuf;

    const X86: &str = "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 197\n\
        model name\t: Intel(R) Core(TM) Ultra 7 255H\nstepping\t: 2\nmicrocode\t: 0x114\ncpu MHz\t\t: 1697.2\n\
        cache size\t: 24576 KB\nflags\t\t: fpu vme avx2\nbogomips\t: 6220.80\n\n\
        processor\t: 1\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 197\n\
        model name\t: Intel(R) Core(TM) Ultra 7 255H\nstepping\t: 2\nmicrocode\t: 0x114\ncpu MHz\t\t: 800.0\n\
        cache size\t: 24576 KB\nflags\t\t: avx2 fpu vme\nbogomips\t: 6220.80\n\n";

    /// Every processor with the same caches.
    fn processor_kinds(cpuinfo: &str) -> BTreeSet<String> {
        super::processor_kinds(cpuinfo, |_| "L1 Data 48K 64".to_owned())
    }

    #[test]
    fn identical_processors_are_one_kind_whatever_their_speed_or_flag_order() {
        let kinds = processor_kinds(X86);
        assert_eq!(kinds.len(), 1, "{kinds:?}");
        let kind = kinds.first().unwrap();
        assert!(kind.contains("model name=Intel(R) Core(TM) Ultra 7 255H") && kind.contains("flags=avx2 fpu vme"), "{kind}");
        assert!(!kind.contains("MHz") && !kind.contains("microcode") && !kind.contains("processor"), "{kind}");
    }

    #[test]
    fn another_model_or_another_flag_is_another_kind() {
        assert_ne!(processor_kinds(X86), processor_kinds(&X86.replace("avx2", "avx512f")));
        assert_ne!(processor_kinds(X86), processor_kinds(&X86.replace("model\t\t: 197", "model\t\t: 85")));
        // The same model in another stepping, or reporting another cache
        // (a virtual machine): `-march=native` may tune for either.
        assert_ne!(processor_kinds(X86), processor_kinds(&X86.replace("stepping\t: 2", "stepping\t: 3")));
        assert_ne!(processor_kinds(X86), processor_kinds(&X86.replace("24576 KB", "16384 KB")));
        // The same in everything `/proc/cpuinfo` says, with another first-
        // level cache: an efficiency core beside a performance core.
        let by_number = |number: &str| if number == "1" { "L1 Data 32K 64".to_owned() } else { "L1 Data 48K 64".to_owned() };
        assert_eq!(super::processor_kinds(X86, by_number).len(), 2);
        // One core of another kind makes the host another host.
        let mixed = X86.replacen("fpu vme avx2", "fpu vme", 1);
        assert_eq!(processor_kinds(&mixed).len(), 2);
        // Listed in the other order, it is the same host.
        let blocks: Vec<&str> = mixed.split("\n\n").collect();
        assert_eq!(processor_kinds(&mixed), processor_kinds(&format!("{}\n\n{}\n\n", blocks[1], blocks[0])));
    }

    #[test]
    fn reads_arm64() {
        let arm = "processor\t: 0\nBogoMIPS\t: 50.00\nFeatures\t: fp asimd sve\nCPU implementer\t: 0x41\n\
                   CPU architecture: 8\nCPU variant\t: 0x1\nCPU part\t: 0xd40\nCPU revision\t: 1\n\n";
        let kinds = processor_kinds(arm);
        let kind = kinds.first().unwrap();
        assert!(kind.contains("CPU part=0xd40") && kind.contains("Features=asimd fp sve"), "{kind}");
        assert!(processor_kinds("").is_empty());
    }

    fn conf(machine: &str, universe: Option<&str>, build_env_digest: &str) -> BuildConf {
        BuildConf {
            mode: Mode::Record,
            cactup: PathBuf::from("/opt/cactup"),
            config_dir: PathBuf::from("/c/configs/sim"),
            cactus_root: PathBuf::from("/c"),
            machine: machine.to_owned(),
            universe: universe.map(str::to_owned),
            build_env_digest: build_env_digest.to_owned(),
            store: PathBuf::from("/nonexistent/cache"),
            relocate: true,
        }
    }

    #[test]
    fn the_machine_the_universe_and_its_environment_setup_each_key_differently() {
        let tmp = tempfile::tempdir().unwrap();
        let platform = |conf: &BuildConf, cc_dir: &Path| super::platform(conf, cc_dir).unwrap().digest;
        let here = platform(&conf("athena", None, "e1"), tmp.path());
        assert_eq!(here, platform(&conf("athena", None, "e1"), tmp.path()));
        assert_ne!(here, platform(&conf("saturn", None, "e1"), tmp.path()));
        assert_ne!(here, platform(&conf("athena", Some("et-sing"), "e1"), tmp.path()));
        assert_ne!(here, platform(&conf("athena", None, "e2"), tmp.path()));
        // The host half is remembered for the attempt.
        assert_eq!(fs::read_dir(tmp.path().join("hosts")).unwrap().count(), 1);
        let memo = fs::read_dir(tmp.path().join("hosts")).unwrap().next().unwrap().unwrap().path();
        let remembered = fs::read_to_string(&memo).unwrap();
        assert!(remembered.ends_with(" mixed") || remembered.ends_with(" uniform"), "{remembered}");
        // What is remembered is what is used: of a host whose processors
        // differ, that they do.
        fs::write(&memo, format!("{} mixed", "0".repeat(64))).unwrap();
        assert!(super::platform(&conf("athena", None, "e1"), tmp.path()).unwrap().mixed);
        fs::write(&memo, format!("{} uniform", "0".repeat(64))).unwrap();
        assert!(!super::platform(&conf("athena", None, "e1"), tmp.path()).unwrap().mixed);
        // A memo that is not one is not used.
        fs::write(&memo, "0".repeat(64)).unwrap();
        assert_eq!(here, platform(&conf("athena", None, "e1"), tmp.path()));
        // And is the same in another attempt on this host.
        let other = tempfile::tempdir().unwrap();
        assert_eq!(here, platform(&conf("athena", None, "e1"), other.path()));
    }
}
