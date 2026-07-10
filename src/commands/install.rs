//! `cactup install` — install an Einstein Toolkit release.

use super::{machine, p2s, prompt_with_default, Ctx};
use crate::args::InstallArgs;
use crate::database::CactusInstallation;
use crate::{manifest, shell, Res};
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use directories::BaseDirs;
use std::fs;
use std::path::Path;

pub fn dispatch(ctx: &Ctx, args: InstallArgs) -> Res<()> {
    let InstallArgs {
        release,
        alias,
        silent,
        install_prefix,
        no_symlink,
        symlink_prefix,
        symlink_name,
    } = args;

    let base_dirs = BaseDirs::new().ok_or(anyhow!("Failed to determine base directories"))?;
    let cactup_root = &crate::CACTUP_ROOT;

    let repo = manifest::ensure_manifest_repo(cactup_root, &ctx.globals.manifest_url)?;
    let tags = manifest::get_tags(&repo)?;
    // §2.3: a snapshot for the prompt-time checks; the lock is NOT held across
    // the download/build below. The final registration re-checks under the
    // lock in `ctx.db.update`.
    let database = ctx.db.read()?;

    if tags.is_empty() {
        println!("No releases found.");
        return Ok(());
    }

    let release_default = tags.first().unwrap().short_name.clone();
    let install_prefix_default = |alias: &str|
        p2s(cactup_root.join("cacti")
                       .join(alias));
    let symlink_prefix_default = base_dirs.home_dir();
    let symlink_name_default = "Cactus";

    let release_tag = match release {
        Some(release) => {
            match tags.iter().position(|tag| tag.short_name == release) {
                Some(pos) => &tags[pos],
                None => {
                    println!("{}", format!("{} is not a valid release.", release.bold()).bright_red());
                    return Ok(());
                }
            }
        }
        None if silent => &tags[0],
        None => loop {
            let release_sel = prompt_with_default("Which release do you want to install?", &release_default)?;
            match tags.iter().position(|tag| tag.short_name == release_sel) {
                Some(pos) => break &tags[pos],
                None => {
                    println!("{}", format!("{} is not a valid release.", release_sel.bold()).bright_red());
                }
            }
        }
    };

    let release = &release_tag.short_name;

    let alias = match alias {
        Some(alias) if database.installations.contains_key(&alias) => {
            println!("{}", format!("An installation with the alias {} already exists.", alias.bold()).bright_red());
            return Ok(());
        },
        Some(alias) if !valid_alias(&alias) => {
            println!("{}", format!("{} is not a valid alias: use only letters, digits, '-', '_', and '.'.", alias.bold()).bright_red());
            return Ok(());
        }
        Some(alias) => alias,
        None if silent && database.installations.contains_key(release) => {
            println!("{}", format!("An installation with the alias {} (derived from the release name) already exists. Please choose a different alias by passing {}.", release.bold(), "--alias".bold()).bright_red());
            return Ok(());
        }
        None if silent => release.to_owned(),
        None => loop {
            let alias_sel = prompt_with_default("What should the installation's alias be? This unique name will be used to identify the installation in the future.", release)?;
            if database.installations.contains_key(&alias_sel) {
                println!("{}", format!("An installation with the alias {} already exists. Please choose another.", alias_sel.bold()).bright_red());
            } else if !valid_alias(&alias_sel) {
                println!("{}", format!("{} is not a valid alias: use only letters, digits, '-', '_', and '.'.", alias_sel.bold()).bright_red());
            } else {
                break alias_sel;
            }
        }
    };

    let install_prefix = match install_prefix {
        Some(install_prefix) => install_prefix,
        None if silent => install_prefix_default(&alias)?,
        None => prompt_with_default("Where should the installation live? The Cactus directory will be created here.", &install_prefix_default(&alias)?)?
    };
    let install_prefix = shell::expand_path(&install_prefix, &base_dirs);

    let do_symlink = match no_symlink {
        true => false,
        false if silent => true,
        false => prompt_with_default("Do you want to create a symlink?", "yes")?.eq_ignore_ascii_case("yes")
    };

    let symlink_prefix = match do_symlink {
        true => {
            match symlink_prefix {
                Some(symlink_prefix) => symlink_prefix,
                None if silent => p2s(symlink_prefix_default.to_owned())?,
                None => prompt_with_default("Where should the symlink live?", &p2s(symlink_prefix_default.to_owned())?)?
            }
        },
        false => "".to_owned()
    };
    let symlink_prefix = shell::expand_path(&symlink_prefix, &base_dirs);

    let symlink_name = match do_symlink {
        true => {
            match symlink_name {
                Some(symlink_name) => symlink_name,
                None if silent => symlink_name_default.to_owned(),
                None => prompt_with_default("What should the symlink be called?", symlink_name_default)?
            }
        },
        false => "".to_owned()
    };

    // §4.7: an unrecognized host gets a machine persisted into the user MDB
    // right here — the successor to simfactory's `sim setup-silent`.
    let machine = machine::ensure_local_machine(ctx)?;

    // Knob prompts (§5): seeded from stored knobs or the derived defaults
    // ($USER, git email); empty answers are not stored.
    if !silent {
        let snapshot = ctx.db.read()?;
        let mut answers = Vec::new();
        for (knob, question) in [
            ("user", "Your username on this machine?"),
            ("email", "Your email (for job notifications)?"),
            ("allocation", "Your default allocation/account (empty for none)?"),
        ] {
            let default = snapshot.knob_or_default(&machine.name, knob).unwrap_or_default();
            let answer = prompt_with_default(question, &default)?;
            if !answer.is_empty() {
                answers.push((knob, answer));
            }
        }
        ctx.db.update(|db| {
            for (knob, value) in &answers {
                db.set_knob(&machine.name, knob, value.clone());
            }
            Ok(())
        })?;
    }

    let install_dir = Path::new(&install_prefix);
    fs::create_dir_all(install_dir)
       .with_context(|| format!("Failed to create installation directory {}", install_dir.display()))?;

    // Resolve to an absolute path. A symlink stores its target verbatim, so a
    // relative `install_prefix` would otherwise yield a target interpreted
    // relative to the *link's* directory, producing a dangling/wrong symlink.
    let install_dir = fs::canonicalize(install_dir)
       .with_context(|| format!("Failed to resolve installation directory {}", install_dir.display()))?;

    let thorn_list =
        release_tag.read_file("einsteintoolkit.th")
                   .with_context(|| format!("Failed to read einsteintoolkit.th from release {}", release_tag.short_name))?;

    fs::write(install_dir.join("einsteintoolkit.th"), &thorn_list)
       .with_context(|| format!("Failed to write einsteintoolkit.th to {}", install_dir.display()))?;

    // --- Fetch GetComponents ---
    const GET_COMPONENTS_URL: &str =
        "https://raw.githubusercontent.com/gridaphobe/CRL/ET_2025_05/GetComponents";

    let script_bytes =
        reqwest::blocking::get(GET_COMPONENTS_URL)
                          .and_then(|r| r.error_for_status())   // turn 404/5xx into an error
                          .with_context(|| format!("Failed to download {GET_COMPONENTS_URL}"))?
                          .bytes()
                          .with_context(|| "Failed to read GetComponents response body")?;

    let script_path = install_dir.join("GetComponents");
    fs::write(&script_path, &script_bytes)
       .with_context(|| format!("Failed to write {}", script_path.display()))?;

    // --- Make it executable (Unix only; no-op concept on Windows) ---
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755); // rwxr-xr-x
        fs::set_permissions(&script_path, perms)
           .with_context(|| format!("Failed to chmod {}", script_path.display()))?;
    }

    // --- Run it on the user's behalf ---
    let status =
        std::process::Command::new(&script_path)
                              .current_dir(&install_dir)
                              .arg("einsteintoolkit.th")
                              .status()
                              .with_context(|| format!("Failed to execute {}", script_path.display()))?;

    if !status.success() {
        return Err(anyhow!("GetComponents exited unsuccessfully: {status}"));
    }

    if do_symlink {
        fs::create_dir_all(Path::new(&symlink_prefix))
           .with_context(|| format!("Failed to create symlink prefix {}", symlink_prefix))?;
        let link_path = Path::new(&symlink_prefix).join(&symlink_name);
        let target = install_dir.join("Cactus");

        // Replace a stale symlink from a previous run, but refuse to clobber a real file/dir.
        match fs::symlink_metadata(&link_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                #[cfg(unix)]
                fs::remove_file(&link_path)
                   .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
                #[cfg(windows)]
                fs::remove_dir(&link_path) // dir symlinks on Windows are removed with remove_dir
                   .with_context(|| format!("Failed to remove existing symlink {}", link_path.display()))?;
            }
            Ok(_) => {
                return Err(anyhow!(
                    "Refusing to overwrite existing path {} (not a symlink)",
                    link_path.display()
                ));
            }
            Err(_) => {} // nothing there yet
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link_path)
                          .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), target.display()))?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link_path)
                             .with_context(|| format!("Failed to symlink {} -> {}", link_path.display(), target.display()))?;
    }

    // Fix sim-home/test-home into installation.toml now, from the machine's
    // [paths] (§8.1, §11.5) — every later sim/test command resolves against
    // these, and they are set exactly once, at install time.
    crate::installation::Installation::new(&alias, &install_dir).ensure_meta(&machine)?;

    let became_active = ctx.db.update(|database| {
        if database.installations.contains_key(&alias) {
            bail!(
                "an installation named {alias} appeared while this install ran; \
                 the new installation at {} was NOT registered",
                install_dir.display()
            );
        }
        database.installations.insert(alias.clone(), CactusInstallation {
            alias: alias.clone(),
            release: Some(release.clone()),
            path: install_dir.to_string_lossy().to_string(),
        });
        if database.active_installation.is_none() {
            database.active_installation = Some(alias.clone());
            Ok(true)
        } else {
            Ok(false)
        }
    })?;

    println!("{}", format!("Success! Installed release {} into {}", release_tag.short_name, install_dir.join("Cactus").display()).bold().bright_green());
    if do_symlink {
        println!("{}", format!("Created a symlink at {}/{}", symlink_prefix, symlink_name).bold().bright_green());
    }

    if !became_active {
        println!("Another installation is already active. To switch to this installation, run `{}`", format!("cactup use {}", alias).bold());
    }

    Ok(())
}

/// An alias must be usable verbatim as a path component.
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias != "."
        && alias != ".."
        && alias.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::valid_alias;

    #[test]
    fn alias_validation() {
        for good in ["ET_2025_05", "et-2025.05", "mybuild", "a"] {
            assert!(valid_alias(good), "{good} should be valid");
        }
        for bad in ["", ".", "..", "has space", "slash/y", "tilde~", "a\tb"] {
            assert!(!valid_alias(bad), "{bad:?} should be invalid");
        }
    }
}
