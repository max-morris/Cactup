+++
title = "Updating cactup"
description = "How cactup keeps itself and its machine database current, and how to control it"
+++

# Updating cactup

cactup is one self-contained program plus a copy of the machine database
(MDB) it downloads for you. Both stay current on their own: cactup checks
for a newer build once a day and, by default, installs it; it refreshes the
machine database every few hours. This page explains what happens, how to
change it, and what to do on hosts with no network access.

## How cactup checks for a new build

Every release is published at `https://max-morris.github.io/Cactup/`, where a
small file, `latest.json`, names the newest build, its date, the machine
database generation it uses (see [below](#machine-database-generations)), and
a download with its SHA-256 checksum for each supported platform.

At most **once every 24 hours**, at the start of a command you type, cactup
fetches `latest.json` and compares it with its own build. The check:

- only happens in an **interactive** session (when cactup's error output is a
  terminal), never when output is piped into a script;
- **never happens in a batch job**: the commands a submit script runs on a
  compute node (`sim run --sim-dir …`, `test run --test-dir …`, `build run
  --config-dir …`) neither check for updates nor touch the network;
- is not made before `cactup knob` (you are configuring cactup, so nothing
  is installed underneath you), nor before `cactup update`, which does its
  own;
- gives up after a few seconds if the server does not answer, silently, and
  waits for the next 24-hour window before trying again.

The time of the last check is kept in `~/.cactup/update-check`; deleting that
file makes the next command check again.

What happens when a newer build exists depends on the `autoupdate` knob.

## The `autoupdate` knob

| Value | What cactup does when a newer build is published |
|---|---|
| `auto` (default) | Downloads it, verifies its size and checksum, confirms it runs, installs it, and re-runs the command you typed with the new build |
| `notify` | Prints one line, ``cactup <new> is available (you have <old>); run `cactup update` `` and carries on |
| `off` | Nothing; cactup never checks on its own (`cactup update` still works) |

```sh
cactup knob autoupdate notify         # only say when a newer build exists
cactup knob autoupdate off            # never check on your own
cactup knob delete autoupdate         # back to the default, auto
cactup -K autoupdate=off install      # skip the check for this one command
```

An automatic update only replaces a cactup that the installer put in
`~/.cactup/bin`. If you run a copy from somewhere else, or `~/.cactup/bin` is
read-only, cactup tells you why it cannot update and keeps going with the
build you have.

A download is checked against the published size and SHA-256 **before**
anything runs it. If the file is not there yet or the checksum does not match,
a new release is usually still spreading through the web server's cache
(about ten minutes); cactup quietly retries on the next command until the
release has propagated.

## `cactup update`

```sh
cactup update --check     # compare with the newest release; change nothing
cactup update             # install the newest build and refresh the machine database now
```

`--check` prints your build and its date, the newest published build, the
machine database generation of each, and whether an update would happen. It
modifies nothing.

Plain `cactup update` works regardless of the `autoupdate` knob and of the
24-hour and 6-hour waits:

1. If a newer build exists, it is installed, and the rest of the update runs
   as the new build.
2. The machine database is refreshed immediately (skipped when you pass
   `--mdb-path`).

If the binary cannot be replaced (a copy outside `~/.cactup/bin`, or a
read-only directory), the machine database is still refreshed and then the
command fails with the reason. If it reports that a release is still
propagating, wait a few minutes and run it again.

{{cactup:cli command="update"}}

## Where builds live, and why jobs are safe

Each build has an id: the short hash of the source commit it was built from.
`cactup --version` shows it with the build date and the machine database
generation, for example `cactup 0.1.0 (a1b2c3d 2026-09-24, mdb generation 1)`.

Every build is kept under its own name, and `cactup` on your `PATH` is a
symbolic link to the current one:

```
~/.cactup/bin/
  cactup -> cactup-a1b2c3d     # what your PATH finds
  cactup-a1b2c3d               # the current build
  cactup-9f8e7d6               # the previous build
  cactup-9f8e7d6.retired       # when it stopped being current
```

When you submit a simulation, test run, or queued build, the job script calls
cactup by that versioned path (it is what the `@CACTUP@` template variable
expands to), so **a job always runs the exact build it was submitted with**,
even if cactup updates itself while the job waits in the queue. Installing an
update swaps the link in one step and never modifies a file that a running
cactup is using.

Old builds are **never deleted automatically**: a restart chain can keep
re-submitting itself under the build it started with for months, and every
submit script it writes names that file. Each build costs about 11 MB. When
you know nothing is still running under old builds, `cactup update --prune`
removes every build that was retired more than 30 days ago (never the
current one, never the one that is running) and prints what it removed. A
job whose build has been pruned cannot start; submit it again, and it will
use the current build.

## The machine database

cactup keeps its own git clone of the machine database under `~/.cactup/mdb`:

```
~/.cactup/mdb/
  repo/                   # the git repository: the project's `mdb` branch
  <commit>/               # one exported revision of the machine database
  gen-1 -> <commit>       # the revision this cactup uses
```

- It is refreshed **at most every 6 hours**, when a command needs machine
  information. A progress line (`machine database`) appears only while the
  refresh runs, and a line such as `updated to generation 1 (4e5f6a7)` is
  left behind only when something actually changed.
- Each revision is exported to its own directory and switched in with one
  link change, so a command that is already running never sees a
  half-updated database. Superseded revisions are removed a day later.
- **Never edit anything under `~/.cactup/mdb`**: the next refresh replaces it.
  To customize a machine, copy it into your own overlay with `cactup machine
  create --from-existing` and edit the copy under `~/.cactup/machines/`. A
  refresh never touches the overlay. See the
  [MDB Overview](../authors/mdb-overview.html).

### Machine database generations

The machine database format evolves with cactup. Each cactup build
understands exactly one **generation** of it (the number `--version` shows),
and uses the newest revision of the database published for that generation.
When the database moves to a newer generation that your build cannot read,
your build keeps using the last revision of its own generation and says so,
loudly, on every command:

```
the machine database has moved to generation 2; this cactup (generation 1)
keeps using the last generation-1 revision. Run `cactup update` (if that
reports up to date, a release is still propagating; retry later)
```

Nothing breaks while you see this; your machines simply stop receiving
fixes. `cactup update` installs a build of the new generation. With
`autoupdate = auto` you will rarely see the message at all.

Machines in your overlay record the generation they were written for, as
`mdb-generation` under `[cactup]` in their `meta.toml`
(`cactup machine create` fills it in):

- a machine **without** the key loads with a one-time warning that cactup is
  assuming its own generation;
- a machine written for an **older** generation is refused, with a pointer to
  the list of changes you need to apply; `cactup machine list` still lists it,
  marked in yellow;
- a machine written for a **newer** generation asks you to run `cactup update`.

How to bring an overlay machine forward is described in
[MDB Generations](../authors/mdb-generations.html#updating-an-overlay-machine).

## Offline hosts and firewalls

- **Update checks** that cannot reach the server fail silently and are not
  retried for 24 hours. Set `autoupdate` to `off` to skip them entirely.
- **Machine database refreshes** give up after about five seconds without a
  connection (longer when the connection goes through a proxy, whether set in
  the environment or in git config), and after 30 seconds in which the server
  sends nothing (five minutes at most in all). cactup then uses the copy it
  already has, with one line saying it `could not reach` the server and how
  old that copy is, and does not retry for 6 hours. Ctrl-C abandons a refresh
  within a second, however it is going.
- With **no copy at all and no network**, commands that need machine
  information fail with an explanation. Either run `cactup update` once on a
  host that shares your home directory and does have network access (a login
  node, typically), or clone the database yourself and point cactup at it:

  ```sh
  git clone --branch mdb --single-branch https://github.com/max-morris/Cactup.git ~/cactup-mdb
  cactup --mdb-path ~/cactup-mdb machine list
  ```

- **Compute nodes never need network access.** Jobs neither check for
  updates nor refresh the machine database.

## Mirrors and forks: `update-url` and `mdb-url`

Two more knobs say where updates come from:

| Knob | Default | What it points at |
|---|---|---|
| `update-url` | `https://max-morris.github.io/Cactup` | The site serving `latest.json` and the builds (https; plain http only for a test server on `127.0.0.1`, `localhost` or `[::1]`) |
| `mdb-url` | `https://github.com/max-morris/Cactup.git` | The git repository whose `mdb` branch is the machine database |

Point them at a mirror inside a firewall, or at your own fork (a fork that
runs the project's CI publishes to `https://<owner>.github.io/<repo>`):

```sh
cactup knob update-url https://mirror.example.org/cactup
cactup knob mdb-url https://mirror.example.org/Cactup.git
```

`autoupdate`, `update-url` and `mdb-url` describe your cactup installation,
not a simulation: unlike the other knobs, they are not recorded with the
simulations, builds and test runs you submit.

The installer takes the same site from its environment:
`CACTUP_UPDATE_ROOT=https://mirror.example.org/cactup`.

## Moving cactup's home: `CACTUP_HOME`

Everything described here lives under `~/.cactup` unless the environment
variable `CACTUP_HOME` names another directory (an absolute path). The
installer honors it too:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://max-morris.github.io/Cactup/cactup-init.sh | CACTUP_HOME=/work/me/cactup sh
```

Set it in your shell profile, next to the `PATH` line the installer added,
so that every cactup you run agrees on where its home is.

## Building cactup from source

A cactup you build yourself with `cargo build` (debug or release) is a
**development build**: `--version` says `dev build`, it never checks for
updates or updates itself, it reads the machine database straight from the
`mdb/` directory of your source tree, and `cactup update` refuses with a
reminder to use `git pull` and `cargo build`. Jobs you submit with it run
that binary from where it was built.

## Uninstalling

1. Optionally remove the Einstein Toolkit installations first with `cactup
   list` and `cactup uninstall <alias>`, so nothing is left behind outside
   `~/.cactup` (installation, simulation and test directories can live
   elsewhere, as set by your machine definition).
2. Remove cactup's home. **This also removes anything else under it**: your
   overlay machines (`machines/`), knobs and installation list
   (`database.json`), and any installations or simulations that live under
   the default `~/.cactup/cacti/` or `~/.cactup/simulations/`:

   ```sh
   rm -rf ~/.cactup          # or "$CACTUP_HOME"
   ```

   To remove only the program and keep your data, delete `~/.cactup/bin`
   and `~/.cactup/mdb` instead.
3. Remove the two lines the installer appended to each of `~/.profile`,
   `~/.bashrc`, `~/.bash_profile` and `~/.zshrc` that existed at the time:

   ```sh
   # added by cactup-init
   export PATH="/home/you/.cactup/bin:$PATH"
   ```
