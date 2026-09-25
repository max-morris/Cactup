+++
title = "cactup"
description = "Install and manage Einstein Toolkit releases on HPC clusters and workstations"
+++

# Welcome to cactup

**cactup** is the easiest way to install and manage [Einstein Toolkit](http://einsteintoolkit.org/) (Cactus) releases on HPC clusters and workstations. It replaces the older simfactory system with a modern, user-friendly CLI.

## Quick start

<div class="install-box">

To install cactup, run this in your terminal:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://max-morris.github.io/Cactup/cactup-init.sh | sh
```

<details>
<summary>No <code>curl</code>? Use <code>wget</code> instead</summary>

```sh
wget -qO- https://max-morris.github.io/Cactup/cactup-init.sh | sh
```

</details>

<p class="install-note">Linux x86_64 or aarch64, any distribution; installs to <code>~/.cactup/bin</code>.</p>

</div>

cactup keeps itself and its machine database up to date; see
[Updating cactup](users/updating.html). Then:

```sh
cactup releases      # See available Einstein Toolkit releases
cactup install       # Install the latest release (or choose a version)
cactup list          # List the releases you've installed
```

Once installed, explore the user guide to build configs and run simulations.
To refresh an existing installation — pick up a thornlist edit or move it to
a newer release — see [Installing Releases](users/installing-releases.html)
for `cactup installation refetch`.

## What is cactup?

cactup handles:

- **Installing releases** — download and install Einstein Toolkit versions with a single command
- **Refreshing installations** — refetch an existing installation to pick up thornlist edits or move to a new release
- **Managing configurations** — build Cactus configs with custom thornlists and compiler options
- **Running simulations** — create, submit, and monitor simulations on batch systems or interactively
- **Running test suites** — validate built configs against the thorn test suite
- **Cluster support** — manage multiple machines, from single-node workstations to large HPC clusters

## Two audiences

**Users**: Install releases, build configs, and run simulations on your machine. Start with [Getting Started](users/getting-started.html).

**Cluster authors**: Add your HPC cluster to cactup by writing a machine database (MDB) entry. Start with [MDB Overview](authors/mdb-overview.html).

## For cluster authors

If you manage an HPC cluster and want to add it to cactup, see [Porting a Cluster](authors/porting-a-cluster.html). You'll create a machine definition (meta.toml) and submit/run scripts; cactup handles the rest.

## Get help

- **User questions**: Check the [User Guide](users/getting-started.html)
- **CLI reference**: See the full [CLI documentation](reference/cli.html)
- **Cluster setup**: Start with [MDB Overview](authors/mdb-overview.html)
