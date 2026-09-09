+++
title = "Machine Discovery"
description = "How cactup decides which machine it is on, and how to make your machine auto-detect"
+++

# Machine Discovery

**Machine discovery** is how cactup figures out which machine definition to use when you run a command. Each machine may ship one or two optional **matcher files** that say which hosts it claims:

- `hostname.regexp` — a single regular expression, matched against the hostname. Fast (evaluated inside cactup, no Python), and enough for almost every cluster.
- `discover.py` — a Python 3 script with an `is_machine(hostname)` function, for the rare site where the hostname alone cannot tell.

If both exist, the regexp is tried first and `discover.py` runs only when the regexp does not match. A machine with neither file is never auto-detected; select it with `--machine`.

## Discovery process

When you run a cactup command:

1. If you specify `--machine myclu`, use that machine (skip discovery and the cache entirely)
2. If a machine is cached **and** the cache was verified on this hostname in this login shell, use it
3. Otherwise **re-verify** the cached machine: run only its own matcher against the current hostname. If it still claims the host, keep it and stamp the cache for this host and shell
4. Otherwise run every machine's matchers (both MDB layers) against the hostname. Exactly one match: use it (and cache it). Several: cactup asks you to choose. None: keep the previously cached machine if there was one (see below), else fall back to `generic` (not cached)

Step 2 is what every command after the first in a shell hits: no matcher runs at all. Step 3 is what a new login shell, or the first command on a different node, costs: one regexp, or one `python3` if the machine only has a `discover.py`.

### Why the cache is re-verified

The cache lives in cactup's database under `~/.cactup`. If your home directory is shared between two clusters, the machine cached on cluster A used to be reused on cluster B silently, and everything downstream (queues, submit scripts, optionlists) was wrong. Re-verification catches that on the first command you run on B: A's matcher does not claim B's host, discovery runs, and cactup tells you `Detected machine changed from a to b`.

### Why "nobody claims this host" keeps the cached machine

An interactive allocation (`salloc`, `srun --pty`, …) puts you on a compute node whose hostname a login-node pattern like `^login[1-4]\.frontera\.tacc\.utexas\.edu$` never claims. That is not a different machine, so cactup keeps the cached one, prints a one-line note, and stays quiet for the rest of that shell. If it really is a different machine, tell cactup so:

```sh
cactup machine forget      # clear the cache; the next command re-discovers
cactup --machine other …   # or just say which one
```

## Writing hostname.regexp

The whole file, with surrounding whitespace trimmed, is one pattern in the [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax). There is no comment syntax: a `#` is part of the pattern. The pattern is tested against the hostname as cactup resolved it **and** against its short form (the part before the first `.`), so a pattern for bare node names matches fully qualified names too:

```
^ln[1-4]$
```

claims both `ln1` and `ln1.cosma.dur.ac.uk`. Anchor your patterns (`^…$`) unless you really mean a substring match. A file that is empty or does not compile is reported as a warning and treated as "does not match".

Examples:

```
^mike\d+(\.hpc\.lsu\.edu)?$
^(login0[0-7]|a[0-7]{3})\.anvil\.rcac\.purdue\.edu$
^(gra-login\d+|gra\d+)
```

Every machine shipped in the system MDB uses a `hostname.regexp`; they are direct ports of the old simfactory `aliaspattern` regexes.

## Writing discover.py

Only when the hostname is not enough. `discover.py` is a Python 3 script with a single function:

```python
def is_machine(hostname):
    """Return True if this is the machine, False otherwise."""
    return hostname == "example.hpc.edu"
```

The function receives the hostname cactup resolved and returns a boolean. It may ignore the argument and probe the system itself:

```python
import os

def is_machine(hostname):
    # Two clusters share a hostname scheme; the module system tells them apart.
    return "our-cluster-modules" in os.environ.get("LOADEDMODULES", "")
```

```python
import os

def is_machine(hostname):
    # A marker file identifies the cluster.
    return os.path.exists("/etc/cluster-id") and \
           open("/etc/cluster-id").read().strip() == "our-cluster"
```

Things to know:

- All `discover.py` scripts that need to run are evaluated in **one** `python3` process, each in its own namespace. Anything the script prints to stdout is redirected to stderr; it cannot confuse cactup.
- A script that raises (or calls `sys.exit`) counts as "does not match". Run with `-v` to see the traceback.
- Keep it fast and side-effect free; it runs whenever cactup has to re-verify or re-discover.
- Pair it with a `hostname.regexp` if the hostname usually suffices: the regexp answers first and the script is only consulted when it does not match.

## Discovery order and priority

Discovery is a single sweep over all machines **sorted alphabetically by name**,
across both MDB layers together — not "system layer first, then user layer".
Name-shadowing is applied before the sweep: a user machine that shares a system
machine's **name** completely replaces it, so only one machine's matchers run for
that name (the user one's).

Because cactup collects *all* matches rather than stopping at the first, having
two differently-named machines match the same host is not a silent
first-wins — cactup prompts you to pick. Keep each pattern specific enough
that only one machine claims a given host.

To override a system machine: define your machine in the user MDB with the **same
name**. It shadows the system machine entirely.

## Workstations

The `generic` machine ships no matcher file, which is what makes it the fallback. `cactup machine create` writes a `hostname.regexp` claiming your host's exact name (and its short form), so the machine it creates auto-detects from then on; `--no-discover` writes no matcher, for a machine you only ever select with `--machine`.

## Debugging discovery

To see which machine was detected, and for which host:

```sh
cactup show            # machine section: name, plus "detected for host: …"
cactup machine show    # resolves (re-verifying or re-discovering as needed)
```

To test a pattern against a hostname without being there:

```sh
cactup --hostname login2.myclu.edu machine show
```

`-v` prints why a machine was skipped (a failing `discover.py`, a regexp that did not compile), and `--trace` shows the `python3` command line when one is spawned. To skip discovery and pick a machine explicitly:

```sh
cactup --machine mylab config list     # Use mylab regardless of detection
```

To clear the cache and re-run discovery:

```sh
cactup machine forget
cactup machine show   # Forces re-discovery
```

## Caching behavior

Once a machine is detected, cactup records it in its database under `~/.cactup`, together with the hostname it was verified on and an identifier for the login session that verified it. The record is a **single** one for this `~/.cactup`, not one per host: a `~/.cactup` lives on one machine, and the login and compute nodes of a cluster share it.

The name in that record is trusted without running anything while the hostname and login shell are the ones that stamped it; any other hostname or shell re-verifies it first (see above). It never expires on its own; `cactup machine forget` clears it, and `cactup machine delete` clears it when it named the deleted machine. A run on a system without `/proc` cannot identify its session and re-verifies every time, which still costs only one regexp.

## Configs remember their machine

A build is not portable across machines, so every config records the machine it was built for, and every simulation records the machine it was created on. `cactup build` of an existing config, `sim create`/`submit`/`run`, and `test run`/`submit` refuse to use one that was built for another machine:

```
error: config "sim" was built for machine "mike" but this is machine "qbd"; pass --ignore-machine (or -f) to use it anyway, at your own risk
```

That is the message you will see if the cache was stale and re-verification just corrected it. Pass `--ignore-machine` (or the command's `-f`, which implies it) to proceed regardless; for `cactup build` that forces a full rebuild, after which the config names the current machine.

## Next steps

- [meta.toml Reference](meta-toml.html) — define the machine's configuration
- [Porting a Cluster](porting-a-cluster.html) — complete end-to-end example
