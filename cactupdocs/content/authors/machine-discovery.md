+++
title = "Machine Discovery"
description = "Write discover.py scripts to auto-detect your machine"
+++

# Machine Discovery

**Machine discovery** is how cactup figures out which machine definition to use when you run a command. Each machine has an optional `discover.py` script that tests whether the current host matches that machine.

## Discovery process

When you run a cactup command:

1. If you specify `--machine myclu`, use that machine (skip discovery)
2. If you've cached a detected machine (run recently), use the cache
3. Run all `discover.py` scripts (system MDB + user MDB) in order
4. Use the first script that returns `True`
5. If no script returns `True`, use `generic` (the fallback)

The discovered machine is cached in cactup's database under `~/.cactup`. It persists until you clear it — there is no time-based expiry. Clear the cache to force re-discovery:

```sh
cactup machine forget
```

## Writing discover.py

Each machine's `discover.py` is a Python 3 script with a single function:

```python
def is_machine(hostname):
    """Return True if this is the machine, False otherwise."""
    return hostname == "example.hpc.edu"
```

The `is_machine()` function receives the hostname and returns a boolean. cactup calls it and uses the result to determine if the machine matches.

### Simple examples

**Exact hostname match:**

```python
def is_machine(hostname):
    return hostname == "mike2.hpc.lsu.edu"
```

**Prefix match (domain):**

```python
def is_machine(hostname):
    return hostname.startswith("node") and hostname.endswith(".example.edu")
```

**Regex match:**

```python
import re

def is_machine(hostname):
    return bool(re.match(r"^comp\d+\.cluster\.local$", hostname))
```

**Multiple criteria:**

```python
import os

def is_machine(hostname):
    # Match the hostname AND check for a cluster-specific file
    if not hostname.startswith("hpc"):
        return False
    # Check for cluster marker file
    return os.path.exists("/etc/cluster-id") and \
           open("/etc/cluster-id").read().strip() == "our-cluster"
```

## Available context

The `is_machine()` function receives only the hostname. For more complex detection, you can:

1. Read files on the system (e.g., `/etc/hostname`, `/etc/os-release`)
2. Run commands (e.g., `hostname -f`, module system queries)
3. Check environment variables (`os.environ`)

```python
import os
import subprocess

def is_machine(hostname):
    # Check hostname
    if not hostname.startswith("compute"):
        return False
    
    # Check for a module system (loaded modules indicate a specific cluster)
    try:
        modules = os.environ.get("LOADEDMODULES", "")
        return "our-cluster-modules" in modules
    except:
        return False
```

## Discovery for HPC clusters

### SLURM cluster with specific hostname pattern

```python
import re

def is_machine(hostname):
    # Match SLURM node hostnames: node001.example.edu, etc.
    return bool(re.match(r"^node\d{3}\.example\.edu$", hostname))
```

### LSU Mike cluster (real example)

```python
def is_machine(hostname):
    return hostname == "mike2.hpc.lsu.edu" or hostname.startswith("node") and ".lsu.edu" in hostname
```

### Multiple clusters with different modules

```python
import os

def is_machine(hostname):
    # Check loaded modules to distinguish clusters
    modules = os.environ.get("LOADEDMODULES", "")
    return "cluster-a-modules" in modules
```

## Discovery for workstations

### Local workstation (generic fallback sufficient)

The `generic` machine has a `discover.py` that returns `False`, making it the fallback. To create a specific workstation machine, you don't need a `discover.py` — just use `cactup --machine mylab` to select it explicitly.

### Auto-detect local machine by hostname

If you want a workstation to auto-detect by name:

```python
import socket

def is_machine(hostname):
    return hostname == "mylab.local" or socket.gethostname() == "mylab"
```

## Discovery order and priority

Discovery runs in this order:

1. System MDB machines (in alphabetical order)
2. User MDB machines (in alphabetical order)

The first to return `True` is used. So if you have both a system and user machine with the same `discover.py`, the system version runs first.

To override: define your machine in the user MDB with the same name. It will shadow the system machine.

## Debugging discovery

To see which machine was detected:

```sh
cactup machine show   # Shows the detected machine
```

To see the hostname cactup is using:

```sh
cactup --hostname myhost machine show   # Override hostname for testing
```

To skip discovery and pick a machine explicitly:

```sh
cactup --machine mylab config list     # Use mylab regardless of detection
```

To clear the cache and re-run discovery:

```sh
cactup machine forget
cactup machine show   # Forces re-discovery
```

## Testing your discover.py

Write a test script to verify your logic:

```bash
#!/bin/bash

python3 << 'EOF'
import sys
sys.path.insert(0, "/path/to/mdb/machines/mylab")
from discover import is_machine

test_hosts = [
    "mylab.hpc.edu",
    "node001.hpc.edu",
    "other.machine.edu",
    "localhost",
]

for host in test_hosts:
    result = is_machine(host)
    print(f"{host}: {result}")
EOF
```

## Common patterns

### Regex-based discovery

```python
import re

def is_machine(hostname):
    # Match: compute01, compute02, ... on any domain
    return bool(re.match(r"^compute\d+", hostname))
```

### Environment-based discovery

```python
import os

def is_machine(hostname):
    # Detect based on environment variable or loaded module
    return os.environ.get("CLUSTER_NAME") == "our-cluster"
```

### Filesystem marker

```python
import os

def is_machine(hostname):
    # Cluster has a specific file in /etc
    return os.path.exists("/etc/.our-cluster-marker")
```

### Reverse DNS check

```python
import socket

def is_machine(hostname):
    try:
        # Verify the hostname resolves to expected subnet
        ip = socket.gethostbyname(hostname)
        return ip.startswith("192.168.1.")
    except:
        return False
```

## Caching behavior

Once a machine is detected, cactup records it in its database under `~/.cactup` and reuses it on subsequent commands, avoiding re-running discovery scripts. The cached choice persists (no time-based expiry) until you clear it.

To see the currently-detected machine:

```sh
cactup machine show
```

To clear it:

```sh
cactup machine forget
```

The cache is per-machine (per entry in the MDB). If you port your machine definition, the cache is preserved until expiration or manual clearing.

## Performance considerations

Discovery scripts run at the start of every cactup command (unless cached). Keep them fast:

- Avoid heavy system calls
- Minimize regex complexity
- Cache results where possible (environment variables, filesystem checks)

For expensive checks (e.g., querying a service), consider environment-based detection instead:

```python
import os

def is_machine(hostname):
    # Use a pre-set environment variable instead of running commands
    return os.environ.get("CACTUP_MACHINE") == "mylab"
```

Users can then set `export CACTUP_MACHINE=mylab` in their shell profile to speed up discovery.

## Next steps

- [meta.toml Reference](meta-toml.html) — define the machine's configuration
- [Porting a Cluster](porting-a-cluster.html) — complete end-to-end example
