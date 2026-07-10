//! Runtime hardware detection (spec §4.6): fills missing `ppn`/`num-threads`/
//! `memory` for machines with `[hardware].autodetect = true` (or absent keys),
//! so the built-in `generic` works on any laptop with zero configuration.
//! Explicit `meta.toml` values always win.

/// Best-effort detected hardware. `cores` always has a value (min 1);
/// `memory_mb` is `None` when the OS query fails.
#[derive(Debug, Clone, Copy)]
pub struct DetectedHardware {
    pub cores: u32,
    pub memory_mb: Option<u64>,
}

pub fn detect() -> DetectedHardware {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    DetectedHardware { cores, memory_mb: detect_memory_mb() }
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
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
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

    #[test]
    fn detects_plausible_values() {
        let hw = detect();
        assert!(hw.cores >= 1);
        // Every dev/CI box this runs on has at least ~256 MB.
        let mem = hw.memory_mb.expect("memory detection should work on Linux/macOS");
        assert!(mem > 256, "implausible memory: {mem} MB");
    }
}
