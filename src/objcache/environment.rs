//! The environment a compile runs in, as far as it can change the object:
//! the part of a cache key for what the compiler reads from its
//! surroundings rather than from its command line.
//!
//! The whole environment cannot be it. Cactus's makefiles export well over
//! a hundred variables into every recipe, many of them paths of the
//! configuration, and a login session adds its own; a key over all of them
//! would never match anything. So this is a list of what compilers are
//! known to read ([`KEYED`], [`KEYED_PREFIXES`]) — which leaves a variable
//! that is on no list and still changes some compiler's output as the way
//! this part could be wrong. The platform digest's environment-setup part
//! and the loaded-modules variables here are what narrows that.

use super::hash::Hasher;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

/// The locale: diagnostics, and character handling in the source. Keyed
/// only for a compiler whose objects its trial showed to depend on it
/// (`identity`, §18.8).
pub const LOCALE: &[&str] = &["LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_MESSAGES"];

/// Variables that change what a compiler does or writes.
const KEYED: &[&str] = &[
    // Reproducible-build inputs to `__DATE__` and friends.
    "SOURCE_DATE_EPOCH", "TZ",
    // What the dynamic loader gives the compiler itself.
    "LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT",
    // Search paths the compiler adds to the ones on its command line.
    "CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "OBJC_INCLUDE_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX",
    "SDKROOT",
    // Behavior switches.
    "GCC_COLORS", "CLANG_NO_DEFAULT_CONFIG",
    // The modules a machine's environment setup loaded.
    "LOADEDMODULES", "_LMFILES_",
];

/// Families of variables that wrapper compilers and vendor toolchains read.
const KEYED_PREFIXES: &[&str] =
    &["OMPI_", "MPICH_", "I_MPI_", "CRAY", "PE_", "NVCC_", "NVHPC", "NVCOMPILER_", "PGI", "HIP", "ROCM_", "SPACK_", "NIX_", "__INTEL_"];

/// Variables that make a compiler write a file besides its object: a hit
/// would skip writing it.
const MORE_OUTPUT: &[&str] =
    &["DEPENDENCIES_OUTPUT", "SUNPRO_DEPENDENCIES", "CC_PRINT_OPTIONS", "CC_PRINT_HEADERS", "CC_LOG_DIAGNOSTICS"];

/// Variables that add to a compiler's flags or rewrite them. Such flags
/// never pass the reader of the command line (`compile`), so nothing it
/// would decline is declined: keying the variable's value is not enough.
const MORE_FLAGS: &[&str] = &["CCC_OVERRIDE_OPTIONS", "QA_OVERRIDE_GCC3_OPTIONS", "GCC_COMPARE_DEBUG"];

/// The digest of the keyed part of `vars`, or the variable that rules
/// caching out.
fn digest_of(vars: impl Iterator<Item = (OsString, OsString)>, locale: bool) -> Result<String, String> {
    let mut keyed = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else { continue };
        if MORE_OUTPUT.contains(&name) {
            return Err(format!("{name} is set, which makes the compiler write another file"));
        }
        if MORE_FLAGS.contains(&name) {
            return Err(format!("{name} is set, which changes the compiler's flags behind its command line"));
        }
        if KEYED.contains(&name) || (locale && LOCALE.contains(&name)) || KEYED_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
            keyed.push((name.to_owned(), value));
        }
    }
    // The environment has no order.
    keyed.sort();
    let mut hasher = Hasher::new("environment");
    for (name, value) in keyed {
        hasher.feed(name.as_bytes());
        hasher.feed(value.as_bytes());
    }
    Ok(hasher.hex())
}

/// The environment digest of this process, which is the compiler's; with
/// the locale, or without it.
pub fn digest(locale: bool) -> Result<String, String> {
    digest_of(std::env::vars_os(), locale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(vars: &[(&str, &str)]) -> Result<String, String> {
        digest_of(vars.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))), true)
    }

    #[test]
    fn only_what_compilers_read_is_keyed() {
        let base = of(&[("LANG", "C"), ("CPATH", "/opt/include")]).unwrap();
        // Order, and what no compiler reads, change nothing.
        assert_eq!(base, of(&[("CPATH", "/opt/include"), ("LANG", "C")]).unwrap());
        assert_eq!(base, of(&[("LANG", "C"), ("CPATH", "/opt/include"), ("CCTK_HOME", "/x"), ("SSH_TTY", "/dev/pts/3")]).unwrap());
        // A keyed variable's value, or presence, does.
        assert_ne!(base, of(&[("LANG", "de_DE.UTF-8"), ("CPATH", "/opt/include")]).unwrap());
        assert_ne!(base, of(&[("LANG", "C")]).unwrap());
        assert_ne!(base, of(&[("LANG", "C"), ("CPATH", "/opt/include"), ("LOADEDMODULES", "gcc/13")]).unwrap());
        assert_ne!(base, of(&[("LANG", "C"), ("CPATH", "/opt/include"), ("OMPI_CC", "icx")]).unwrap());
        assert_ne!(base, of(&[("LANG", "C"), ("CPATH", "/opt/include"), ("PE_ENV", "GNU")]).unwrap());
        // Empty is not unset.
        assert_ne!(base, of(&[("LANG", "C"), ("CPATH", "/opt/include"), ("TZ", "")]).unwrap());
    }

    /// For a compiler whose trial showed the locale does not matter, it is
    /// not keyed; anything else still is.
    #[test]
    fn the_locale_is_keyed_only_where_asked() {
        let without = |vars: &[(&str, &str)]| {
            digest_of(vars.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))), false).unwrap()
        };
        let base = without(&[("CPATH", "/opt/include")]);
        assert_eq!(base, without(&[("CPATH", "/opt/include"), ("LANG", "de_DE.UTF-8"), ("LC_ALL", "C"), ("LANGUAGE", "de")]));
        assert_ne!(base, without(&[("CPATH", "/opt/include"), ("TZ", "UTC")]));
        assert_ne!(of(&[("LANG", "C")]).unwrap(), of(&[]).unwrap());
    }

    #[test]
    fn a_variable_that_adds_an_output_or_flags_rules_caching_out() {
        let err = of(&[("LANG", "C"), ("DEPENDENCIES_OUTPUT", "deps.d")]).unwrap_err();
        assert!(err.starts_with("DEPENDENCIES_OUTPUT is set"), "{err}");
        let err = of(&[("LANG", "C"), ("CCC_OVERRIDE_OPTIONS", "# +-grecord-command-line")]).unwrap_err();
        assert_eq!(err, "CCC_OVERRIDE_OPTIONS is set, which changes the compiler's flags behind its command line");
    }
}
