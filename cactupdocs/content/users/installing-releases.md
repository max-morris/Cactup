+++
title = "Installing Releases"
description = "Install Einstein Toolkit releases with flexible paths and naming"
+++

# Installing Einstein Toolkit Releases

This guide covers everything you need to know about installing and managing Einstein Toolkit releases with cactup.

## Listing releases

View the most recent releases:

```sh
cactup releases
```

See all available versions:

```sh
cactup releases --all
```

## Installing a release

### Simple installation

Install the latest release with interactive prompts:

```sh
cactup install
```

cactup will ask for:
- **Installation name (alias)** — how you'll refer to this release (e.g., `et-2025`, `latest`)
- **Install prefix** — where to install the Cactus source and build (default: `~/.cactup/cacti`)
- **Symlink location** — optional: create a symlink to this installation

### Installing a specific release

Install a particular version without prompts:

```sh
cactup install ET_2025_05 --alias myrelease --silent
```

### Custom paths

Control exactly where cactup installs:

```sh
cactup install ET_2025_05 \
  --alias latest \
  --install-prefix /opt/cactus/ET_2025_05 \
  --symlink-prefix ~/bin \
  --symlink-name cactus
```

This creates:
- Installation at `/opt/cactus/ET_2025_05`
- Symlink at `~/bin/cactus` pointing to the active installation

Use `--no-symlink` to skip the symlink entirely:

```sh
cactup install ET_2025_05 --no-symlink
```

## Listing your installations

See all installed releases:

```sh
cactup list
```

Output shows the alias, release version, installation path, and active marker.

## Viewing installation details

Show the active installation:

```sh
cactup show
```

Show a specific installation:

```sh
cactup show myrelease
```

This displays:
- Release version
- Installation path
- List of built configs
- Active config
- Simulator and test-suite home directories

## Setting the active installation

Make an installation active (the default for `build`, `config`, `sim`, and `test` commands):

```sh
cactup use myrelease
```

You can also override the active installation for a single command:

```sh
cactup --installation myrelease config list
```

## Uninstalling

Remove an installation:

```sh
cactup uninstall myrelease
```

cactup will ask for confirmation. Skip the prompt:

```sh
cactup uninstall myrelease --force
```

This removes the installation directory and any symlinks, but preserves simulation data under `simulation-home`.

## Global flags for installation commands

{{cactup:cli command="install"}}

{{cactup:cli command="list"}}

{{cactup:cli command="show"}}

{{cactup:cli command="use"}}

{{cactup:cli command="uninstall"}}

## Installation locations

By default, cactup uses:

- **Install prefix**: `~/.cactup/cacti/` (can be overridden with `--install-prefix`)
- **Symlink**: `~/Cactus` — i.e. prefix `~` (home) and name `Cactus` by default (customize with `--symlink-prefix` and `--symlink-name`, or skip with `--no-symlink`)
- **Simulation home**: `~/.cactup/simulations/` (can be configured in your machine definition)
- **Test-suite home**: `~/.cactup/tests/` (can be configured in your machine definition)

See your machine's configuration:

```sh
cactup machine show
```

## Next steps

Once you've installed a release, proceed to:
- [Building Configs](building-configs.html) — compile Cactus for your machine
- [Running Simulations](running-simulations.html) — create and run a simulation
- [Test Suites](test-suites.html) — validate your build
