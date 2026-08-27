//! Runtime hardware detection (spec §4.6): fills missing `max-cpus-per-node`/
//! `threads-per-cpu`/`memory`/`max-gpus-per-node` for machines with
//! `[hardware].autodetect = true`
//! (or when some queue would otherwise resolve no value), so the built-in
//! `generic` works on any laptop with zero configuration. Explicit `meta.toml`
//! values always win.

#[cfg(target_os = "linux")]
use std::path::Path;

/// Best-effort detected hardware. `cores` always has a value (min 1);
/// `threads_per_cpu`, `memory_mb` and `gpus` are `None` when the OS query fails
/// or finds nothing.
#[derive(Debug, Clone, Copy)]
pub struct DetectedHardware {
    /// Physical cores per node, never hardware threads — a `cpu` throughout the
    /// MDB is a core (§4.2/§4.6). A machine may boot with SMT disabled, and the
    /// process layout must not move under it; the SMT factor is a separate fact
    /// (`threads_per_cpu`) that a script multiplies in if it wants threads.
    pub cores: u32,
    /// Hardware threads per core (simfactory's `num-smt`), when the OS reports a
    /// thread count that is a clean multiple of the core count. `None` — not 1 —
    /// when SMT could not be established, so an explicit `threads-per-cpu` stays
    /// the only thing that can claim it.
    pub threads_per_cpu: Option<u32>,
    pub memory_mb: Option<u64>,
    /// GPUs on *this* host. On a cluster that is usually the login node, which
    /// typically has none even though the compute nodes are full of them — so
    /// a `None` here means "say nothing", never "this machine has no GPUs".
    pub gpus: Option<u32>,
}

pub fn detect() -> DetectedHardware {
    let (cores, threads_per_cpu) = detect_cpu_topology().unwrap_or_else(|| (fallback_cpus(), None));
    let (memory_mb, gpus) = (detect_memory_mb(), detect_gpus());
    DetectedHardware { cores: cores.max(1), threads_per_cpu, memory_mb, gpus }
}

/// Last resort when the topology probe comes up empty: the thread count. It
/// over-reports a hyperthreaded node by the SMT factor, which is exactly why it
/// is the fallback and not the measurement.
fn fallback_cpus() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}

/// `(cores, threads-per-core)` from `/proc/cpuinfo`: one `processor` block per
/// hardware thread, with siblings sharing a `(physical id, core id)` pair. Read
/// as a single file rather than walking `/sys/devices/system/cpu/*/topology/`,
/// since this runs on every machine load.
#[cfg(target_os = "linux")]
fn detect_cpu_topology() -> Option<(u32, Option<u32>)> {
    cpu_topology(&std::fs::read_to_string("/proc/cpuinfo").ok()?)
}

/// The `/proc/cpuinfo` parser, split out so it can be tested against a real
/// hyperthreaded machine's file on a dev box that is not one.
#[cfg(target_os = "linux")]
fn cpu_topology(cpuinfo: &str) -> Option<(u32, Option<u32>)> {
    let field = |line: &str, key: &str| -> Option<u32> {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().parse().ok())?
    };
    let mut cores = std::collections::HashSet::new();
    let (mut threads, mut socket, mut core) = (0u32, None, None);
    for line in cpuinfo.lines() {
        if line.trim().is_empty() {
            // Block boundary: bank whatever this processor claimed.
            if let (Some(s), Some(c)) = (socket, core) {
                cores.insert((s, c));
            }
            (socket, core) = (None, None);
            continue;
        }
        if line.starts_with("processor") && field(line, "processor").is_some() {
            threads += 1;
        }
        socket = socket.or_else(|| field(line, "physical id"));
        core = core.or_else(|| field(line, "core id"));
    }
    if let (Some(s), Some(c)) = (socket, core) {
        cores.insert((s, c));
    }
    if threads == 0 {
        return None;
    }
    // A kernel that reports no topology at all (some VMs, some arches) leaves
    // the set empty: fall back to one core per thread rather than inventing a
    // core count, and claim nothing about SMT — the threads may well be
    // siblings we simply cannot see.
    if cores.is_empty() {
        return Some((threads, None));
    }
    let cores = cores.len() as u32;
    let smt = (threads % cores == 0).then_some(threads / cores).filter(|smt| *smt > 0);
    Some((cores, smt))
}

#[cfg(target_os = "macos")]
fn detect_cpu_topology() -> Option<(u32, Option<u32>)> {
    let sysctl = |key: &str| -> Option<u32> {
        let mut command = std::process::Command::new("sysctl");
        command.args(["-n", key]);
        crate::shell::trace_command(&command);
        String::from_utf8_lossy(&command.output().ok()?.stdout).trim().parse().ok()
    };
    let cores = sysctl("hw.physicalcpu")?;
    let smt = sysctl("hw.logicalcpu")
        .filter(|threads| cores > 0 && threads % cores == 0)
        .map(|threads| threads / cores);
    Some((cores, smt))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_cpu_topology() -> Option<(u32, Option<u32>)> {
    None
}

/// Best-effort GPU count, from the kernel's own bookkeeping only — no
/// `nvidia-smi`/`rocm-smi` subprocess (this runs on every machine load) and no
/// libc/NVML binding (the binary must stay statically linked — D13). Takes the
/// largest of three independent probes, since a host may expose only one of
/// them; `None` when they all come up empty.
///
/// The probes take their roots as arguments so the tests can point them at a
/// fixture tree — a dev box has no GPUs, so this code would otherwise ship
/// having only ever exercised its empty path.
#[cfg(target_os = "linux")]
fn detect_gpus() -> Option<u32> {
    gpus_under(Path::new("/"))
}

#[cfg(target_os = "linux")]
fn gpus_under(root: &Path) -> Option<u32> {
    let n = [
        gpus_nvidia(&root.join("proc/driver/nvidia/gpus")),
        gpus_amd(&root.join("sys/class/kfd/kfd/topology/nodes")),
        gpus_pci(&root.join("sys/bus/pci/devices")),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    (n > 0).then_some(n)
}

/// NVIDIA proprietary driver: one directory per GPU, keyed by PCI address.
/// Present whenever the `nvidia` kernel module is loaded.
#[cfg(target_os = "linux")]
fn gpus_nvidia(dir: &Path) -> u32 {
    count_dir_entries(dir, |entry| entry.path().is_dir())
}

/// ROCm/amdkfd: one topology node per agent, CPUs included — the GPU agents are
/// the ones reporting a non-zero `simd_count`.
#[cfg(target_os = "linux")]
fn gpus_amd(dir: &Path) -> u32 {
    count_dir_entries(dir, |entry| {
        let Ok(props) = std::fs::read_to_string(entry.path().join("properties")) else {
            return false;
        };
        props
            .lines()
            .find_map(|line| line.strip_prefix("simd_count "))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|count| count > 0)
    })
}

/// Generic PCI fallback: class `0x0302` is "3D controller", i.e. an accelerator
/// with no display attached. Deliberately does NOT count `0x0300` ("VGA
/// compatible"), so a workstation's integrated graphics cannot inflate the
/// count into a bogus `max-gpus-per-node`.
#[cfg(target_os = "linux")]
fn gpus_pci(dir: &Path) -> u32 {
    count_dir_entries(dir, |entry| {
        std::fs::read_to_string(entry.path().join("class"))
            .is_ok_and(|class| class.trim().starts_with("0x0302"))
    })
}

#[cfg(target_os = "linux")]
fn count_dir_entries(dir: &Path, keep: impl Fn(&std::fs::DirEntry) -> bool) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries.flatten().filter(keep).count() as u32
}

#[cfg(not(target_os = "linux"))]
fn detect_gpus() -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn detect_memory_mb() -> Option<u64> {
    // /proc/meminfo: `MemTotal:       196608000 kB`
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1024)
}

#[cfg(target_os = "macos")]
fn detect_memory_mb() -> Option<u64> {
    let mut command = std::process::Command::new("sysctl");
    command.args(["-n", "hw.memsize"]);
    crate::shell::trace_command(&command);
    let output = command.output().ok()?;
    let bytes: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    Some(bytes / (1024 * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_memory_mb() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cpu` is a core: `/proc/cpuinfo` is counted by distinct
    /// `(physical id, core id)` pairs, with the thread count only supplying the
    /// SMT factor. The fixture is omnia's shape — 2 sockets × 2 cores × 2
    /// threads, abridged — because the dev box this runs on is not it.
    #[cfg(target_os = "linux")]
    #[test]
    fn cpuinfo_is_counted_in_cores_not_threads() {
        let mut cpuinfo = String::new();
        for cpu in 0..8 {
            cpuinfo.push_str(&format!(
                "processor\t: {cpu}\nmodel name\t: Intel(R) Xeon(R) Platinum 8352V CPU @ 2.10GHz\n\
                 physical id\t: {}\ncore id\t\t: {}\nsiblings\t: 4\ncpu cores\t: 2\n\n",
                cpu / 4,
                (cpu / 2) % 2,
            ));
        }
        assert_eq!(cpu_topology(&cpuinfo), Some((4, Some(2))));

        // No SMT: threads == cores, so the factor is 1 rather than absent.
        let single = "processor\t: 0\nphysical id\t: 0\ncore id\t\t: 0\n\n\
                      processor\t: 1\nphysical id\t: 0\ncore id\t\t: 1\n";
        assert_eq!(cpu_topology(single), Some((2, Some(1))));

        // A kernel reporting no topology (some VMs): one core per thread, and
        // nothing claimed about SMT.
        let bare = "processor\t: 0\nBogoMIPS\t: 50.00\n\nprocessor\t: 1\nBogoMIPS\t: 50.00\n";
        assert_eq!(cpu_topology(bare), Some((2, None)));

        // Not a cpuinfo at all.
        assert_eq!(cpu_topology("MemTotal: 12 kB\n"), None);
    }

    #[test]
    fn detects_plausible_values() {
        let hw = detect();
        assert!(hw.cores >= 1);
        // No cross-check against `available_parallelism()` here: that call is
        // affinity/cgroup-aware while `/proc/cpuinfo` is the whole box, so on
        // a cpuset-limited runner cores would legitimately exceed it. The
        // cores-vs-threads arithmetic is pinned by the fixture test above.
        if let Some(smt) = hw.threads_per_cpu {
            assert!(smt >= 1, "implausible SMT factor: {smt}");
        }
        // Every dev/CI box this runs on has at least ~256 MB.
        let mem = hw.memory_mb.expect("memory detection should work on Linux/macOS");
        assert!(mem > 256, "implausible memory: {mem} MB");
    }

    /// Dev and CI boxes have no GPUs, so the probes are driven against fixture
    /// trees shaped like the real `/proc` and `/sys` entries.
    #[cfg(target_os = "linux")]
    #[test]
    fn gpu_probes_read_the_kernel_trees() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Nothing there at all: absent, not zero — the caller must be able to
        // tell "no GPUs found" from "this node has zero GPUs".
        assert_eq!(gpus_under(root), None);

        // NVIDIA: one directory per GPU.
        let nv = root.join("proc/driver/nvidia/gpus");
        fs::create_dir_all(nv.join("0000:07:00.0")).unwrap();
        fs::create_dir_all(nv.join("0000:08:00.0")).unwrap();
        assert_eq!(gpus_under(root), Some(2));

        // amdkfd: CPU agents share the tree and must not be counted.
        let kfd = root.join("sys/class/kfd/kfd/topology/nodes");
        for (node, simd) in [("0", 0), ("1", 256), ("2", 256), ("3", 256)] {
            fs::create_dir_all(kfd.join(node)).unwrap();
            fs::write(kfd.join(node).join("properties"), format!("cpu_cores_count 8\nsimd_count {simd}\n")).unwrap();
        }
        // The largest probe wins: 3 GPU agents beats NVIDIA's 2.
        assert_eq!(gpus_under(root), Some(3));

        // PCI: 3D controllers count, the VGA display adapter does not.
        let pci = root.join("sys/bus/pci/devices");
        for (dev, class) in [
            ("0000:01:00.0", "0x030000"), // VGA — integrated graphics
            ("0000:07:00.0", "0x030200"),
            ("0000:08:00.0", "0x030200"),
            ("0000:09:00.0", "0x030200"),
            ("0000:0a:00.0", "0x030200"),
            ("0000:00:1f.0", "0x060100"), // ISA bridge
        ] {
            fs::create_dir_all(pci.join(dev)).unwrap();
            fs::write(pci.join(dev).join("class"), format!("{class}\n")).unwrap();
        }
        assert_eq!(gpus_pci(&pci), 4, "VGA and non-display devices excluded");
        assert_eq!(gpus_under(root), Some(4));
    }
}
