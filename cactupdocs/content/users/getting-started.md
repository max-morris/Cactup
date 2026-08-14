+++
title = "Getting Started"
description = "Your first steps with cactup: list releases, install one, and build a config"
+++

# Getting Started with cactup

This guide walks you through your first cactup workflow: listing available releases, installing one, and building a Cactus config.

## Step 1: List available releases

See what Einstein Toolkit releases are available:

```sh
cactup releases
```

This shows the most recent releases. To see all available versions:

```sh
cactup releases --all
```

## Step 2: Install a release

Install the latest release (cactup will prompt for an installation name and location):

```sh
cactup install
```

Or install a specific release with minimal prompts:

```sh
cactup install ET_2025_05 --silent
```

You can also customize the install location and symlink:

```sh
cactup install ET_2025_05 --alias myrelease \
  --install-prefix /opt/cactus \
  --symlink-prefix ~/bin --symlink-name cactus
```

After installation, you'll see:

```sh
cactup list          # List all your installations
cactup show          # Show cactup's overall state: installation, config, machine
cactup inst show myrelease  # Show one installation in detail
cactup use myrelease   # Set the active installation
```

## Step 3: Build a configuration

The next step is to build a Cactus configuration. You'll need:

1. **A parfile** — the configuration file defining which thorns to include
2. **An optionlist** — compiler settings for your machine (cactup manages these)

Build a config with:

```sh
cactup build myconfig
```

This creates a Cactus config named `myconfig` with default settings. To customize the build:

```sh
cactup build myconfig \
  --optimize           # Optimized (-O2) build
  --variant cuda       # Use the CUDA variant (if available on your machine)
  -j 8                 # Parallel make with 8 jobs
```

Other build options:

```sh
cactup build myconfig --debug       # DEBUG=yes (debug build)
cactup build myconfig --profile     # PROFILE=yes (profiling support)
cactup build myconfig --unsafe      # UNSAFE=yes (unsafe optimizations; use with care)
cactup build myconfig --reconfig    # Reconfigure before building
cactup build myconfig --clean       # Clean before building
cactup build myconfig --force       # Rebuild even if already built
```

(Optimization is already on by default, so `--optimize` is rarely needed.)

See [Building Configs](building-configs.html) for more details on variants, universes, and advanced options.

## Step 4: Run a simulation

Once you have a built config, create and run a simulation:

```sh
cactup sim create mysim mysim.par --config myconfig
```

Then run it:

```sh
cactup sim run mysim
```

Or submit it to a batch queue:

```sh
cactup sim submit mysim -n 4 -w 1:00:00
```

This submits to 4 nodes with a 1-hour time limit. See [Running Simulations](running-simulations.html) for more options.

## Step 5: Monitor progress

List your simulations:

```sh
cactup sim list       # Short summary
cactup sim list --long  # Extended details
```

Check a specific simulation:

```sh
cactup sim show mysim        # Summary
cactup sim show mysim --long # Extended details
```

Stream the output:

```sh
cactup sim log mysim --follow
```

See [Monitoring & Logs](monitoring.html) for more details.

## What's next?

- **Explore build variants**: Some machines offer multiple optionlists (e.g., CPU vs GPU). See [Building Configs](building-configs.html).
- **Learn about universes**: Advanced build environments (Singularity, module-loaded shells). See [Building Configs](building-configs.html).
- **Run test suites**: Validate your config. See [Test Suites](test-suites.html).
- **Set machine defaults**: Use the `knob` command to set allocation, queue, and other defaults for your machine.
- **Cluster-specific setup**: If you're on an HPC system, check the [Cluster Authors](../authors/mdb-overview.html) guide to understand your machine's configuration.

## Troubleshooting

**"No installation found"**: Run `cactup list` to see what's installed, then `cactup use <alias>` to activate one.

**"Machine not recognized"**: cactup discovers your machine automatically. If it fails, use `cactup --machine generic` or create a local machine entry — see [Cluster Authors](../authors/mdb-overview.html).

**Build fails**: Check the build log in your installation directory. Run `cactup show` to see where things are installed.

**Simulation won't start**: Verify the parfile exists and the config is built. Use `cactup config list` and `cactup config show`.

**"N repo(s) skipped (local state preserved)"**: `cactup installation refetch` (or `inst refetch`) leaves alone any repo that has local modifications, local commits, a switched branch, a detached HEAD, or an in-progress rebase/merge, and reports which thorns are affected. Re-run with `--overwrite-modified` (or `-f`, which also implies it) to fetch over them anyway — the affected files are backed up first, to `~/.cactup/refetch-backups/<alias>/<timestamp>/`. Add `-s/--silent` to hide the detailed skip list and keep just the one-line count. If the refetch had an explicit source (`--release TAG` or a thornlist file), the new thornlist is adopted anyway, and the skipped repos are recorded so `cactup inst show` keeps reporting `conformance: PARTIAL` until they're fetched too. See [Installing Releases](installing-releases.html) for the full refetch contract.
