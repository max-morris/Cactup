+++
title = "The .sim Format"
description = "Every table and field of a .sim file, formats 1 to 4"
+++

# The .sim format

A `.sim` file is TOML. It holds a simulation's parfile and thornlist
verbatim, the exact source each repository was at, the inputs the parfile
needs from outside the source tree, and a little description. `cactup sim
export` and `cactup sim pack` write one; `cactup import` reproduces it; the
Cactarium publishes it. This page lists everything a `.sim` may contain.

Unknown keys are an error in every table: a file that uses something a
reader does not know is refused, never half understood.

## `format`

```toml
format = 1
```

Required, and the first key. Which generation of this format the file is:

| Format | Adds |
|---|---|
| 1 | Everything not listed below. |
| 2 | Archive inputs: `unpack`, `path` and `[[input.fixup]]`. |
| 3 | Inputs that are a file of the simulation's own repository: `repo-file`. |
| 4 | A `[sim] cactarium` stamp naming one sim of an entry that holds several: `owner/name/sim@version`. |

cactup writes the **oldest** format that can hold a file's content, so a file
that uses nothing newer stays readable by older readers. A reader refuses a
format newer than it knows ("upgrade to read it"), and refuses a file whose
content needs a newer format than it claims.

## `[sim]`

```toml
[sim]
name = "bhns-gw230529-dense"
summary = "BH-NS merger with dense 2D output"
description = """Markdown, any length."""
authors = ["Max Morris"]
license = "CC-BY-4.0"
contact = "https://github.com/max-morris/pretty-bhns/issues"
root = "Cactus"
```

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | The simulation's name; not empty. |
| `summary` | no | One line. |
| `description` | no | Markdown. |
| `authors` | no | A list of names. |
| `license` | no | An SPDX license expression. |
| `contact` | no | Where to go for help with it: an `https://` URL or a `mailto:` address. |
| `root` | yes | The thornlist's `!DEFINE ROOT`: the source tree's directory under the installation. |
| `cactarium` | no | Set by the Cactarium when it publishes the file: `owner/name@version`, or `owner/name/sim@version` for one sim of an entry that holds several (format 4). `cactup import` uses it to find a published simulation's repository for `repo-file` inputs. |

## `[thornlist]`

```toml
[thornlist]
filename = "bhns.th"
text = """
!CRL_VERSION = 1.0
...
"""
```

| Key | Required | Meaning |
|---|---|---|
| `filename` | yes | A plain file name: what `cactup sim unpack` writes it as. |
| `text` | yes | The thornlist (GetComponents CRL) exactly as written, with any `!INCLUDE` spliced in. Kept byte for byte. |
| `untracked` | no | Thorns in the build that no repository backs. Only in a file exported with an override; a file that has any is not fully specified. |

## `[parfile]`

```toml
[parfile]
filename = "bhns.par"
kind = "par"
vars = ["SIMULATION_NAME"]
text = """
...
"""
```

| Key | Required | Meaning |
|---|---|---|
| `filename` | yes | A plain file name, ending in `.par` or `.py` to match `kind`. |
| `kind` | yes | `par` (a Cactus parameter file) or `py` (a Python script that prints one). |
| `vars` | no | The `@VAR@` tokens the parfile uses. |
| `text` | yes | The parfile, verbatim and not substituted. |

## `[[input]]`

Something the parfile needs from outside the source tree, reached through a
cactup knob: `@KNOB(name)@` in the parfile. One table per input.

```toml
[[input]]
knob = "kadath-initial-data"
description = "FUKA initial data for the GW230529-like BH-NS binary"
url = "https://einsteintoolkit.org/gallery/bhns/bhns_gw230529_ID.tar.gz"
sha256 = "..."
unpack = true
path = "bhns_gw230529_ID/BHNS_ECC_RED.gam2.30.0.0.5.q0.388889.0.0.13.info"

[[input.fixup]]
file = "bhns_gw230529_ID/BHNS_ECC_RED.gam2.30.0.0.5.q0.388889.0.0.13.info"
replace = "eos = /path/to/gam2.polytrope"
with = "eos = {dir}/bhns_gw230529_ID/gam2.polytrope"
```

| Key | Format | Meaning |
|---|---|---|
| `knob` | 1 | The knob the parfile reads. Each knob at most once. |
| `description` | 1 | What it is, for whoever imports the simulation. |
| `url` | 1 | Where its file can be downloaded. |
| `sha256` | 1 | The SHA-256 of that file (the archive itself, when unpacked): 64 lowercase hex digits. A download that does not match is refused. |
| `unpack` | 2 | The file is an archive to unpack: `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz` or `.zip`, told by the extension. Needs a `sha256` (or a `repo-file`): an archive is never unpacked unverified. |
| `path` | 2 | With `unpack`: the file (or directory) in the unpacked tree the knob points at, relative to its root. Without it, the knob gets the unpacked directory itself. |
| `repo-file` | 3 | A file of the simulation's own repository, at the published version's commit, instead of `url` and `sha256` (the commit pins its content). Only a simulation the Cactarium publishes from its repository can have one. May be unpacked like a download. |

Paths inside an archive or a repository (`path`, `repo-file`, a fixup's
`file`) are relative, `/`-separated, and have no empty, `.` or `..` part and
no backslash.

On import, an input is settled before anything long runs: from `--input
KNOB=VALUE`, else by fetching it (into the `inputs-home` directory, shared by
every simulation that needs the same file), else by asking. An unpacked
archive is kept as a tree named for its content and its fixups, and reused.

### `[[input.fixup]]`

A text edit made to a file of an unpacked archive, once, right after
unpacking, in order. Typically a path the archive's author left as a
placeholder.

| Key | Meaning |
|---|---|
| `file` | The file to edit, relative to the unpacked tree's root. Must be a regular file reached through no link. |
| `replace` | Literal text; every occurrence is replaced, and there must be at least one. Not empty. |
| `with` | Its replacement. `{dir}` stands for the absolute directory the archive was unpacked into, the only placeholder. |

## `[[repo]]`

One table per source repository the thornlist names (per repository, not per
section): the exact source the simulation was built from.

```toml
[[repo]]
name = "carpetx"
url = "https://github.com/EinsteinToolkit/CarpetX.git"
ref = "main"
commit = "3f2c9e1a00112233445566778899aabbccddeeff"
```

| Key | Meaning |
|---|---|
| `name` | The repository's name as the thornlist derives it (`!NAME`, else the URL's last part). |
| `url` | Where it is fetched from. |
| `ref` | The branch or tag the thornlist names. A label only: `commit` is what is fetched. |
| `commit` | The full commit id (40 or 64 hex digits). |
| `sha256` | For a downloaded file instead of a git repository: its SHA-256. At most one of `commit` and `sha256`. |
| `cactarium` | The Cactarium package version this commit belongs to, if any. |
| `dirty` | `true` when the source had uncommitted changes at export (only with `--allow-dirty`). |

A file is **fully specified** when every repository has a `commit` or a
`sha256`, none is dirty, and `[thornlist] untracked` is empty. Only a fully
specified file can be published.

## `[run]`

Suggested run shape, defaults for `cactup import --submit`; machine-shaped
settings (nodes, queue) follow from the machine it runs on instead.

| Key | Meaning |
|---|---|
| `tasks` | MPI tasks. |
| `cpus-per-task` | CPUs per task. |
| `gpus-per-task` | GPUs per task. |
| `walltime` | Total wall time wanted, `HH:MM:SS`; cactup chains restarts against the queue's limit. |

## `[provenance]`

Where the file came from. Informational: `import` needs none of it.

| Key | Meaning |
|---|---|
| `exported-by` | The cactup that wrote it. |
| `exported-at` | When. |
| `machine` | The machine the simulation ran on. |
| `flavor` / `optionlist` | How its configuration was built: a machine flavor, or an optionlist file's name. At most one. |
| `universe` | The build universe (a container image, say), when not the host. |
| `flags` | Build flags. |
| `machine-disabled` | Thorns that machine's configuration turned off. |
| `base` | The Cactarium stack the installation was based on, if any. |
| `extra` | Anything else, as strings. |
