# The tutorial's JupyterHub. Settings come from the environment, which
# compose.yaml fills from .env (see .env.example).
import asyncio
import os
import sys

import docker

from cactup_cyol.cpusets import Assigner, parse_cpu_list, slices

c = get_config()  # noqa: F821

env = os.environ.get

# -- the hub ---------------------------------------------------------------------
c.JupyterHub.bind_url = "http://:8000"
c.JupyterHub.hub_bind_url = "http://:8081"
# What the attendees' containers use to reach the hub: its name on their network.
c.JupyterHub.hub_connect_url = "http://hub:8081"
c.JupyterHub.db_url = "sqlite:////srv/jupyterhub/jupyterhub.sqlite"
c.JupyterHub.cookie_secret_file = "/srv/jupyterhub/jupyterhub_cookie_secret"
# Attendees reach the hub over HTTPS (Caddy's, or the site's proxy's), but the
# hub sees plain HTTP from its own proxy and would leave its cookies without
# Secure; a browser would then send them over a plain http:// request too.
# (Off only for tests/hub, which is plain HTTP end to end.) JupyterHub reads
# cookie_options from tornado_settings only, and hands it on to the
# attendees' servers for their own cookies.
if env("SECURE_COOKIES", "1") == "1":
    c.JupyterHub.tornado_settings = {"cookie_options": {"secure": True}}
# A restarted hub (`docker compose up -d` after changing .env, a new hub
# image) picks the running servers up again instead of stopping them all:
# stopping one loses its kernels and requeues its jobs.
c.JupyterHub.cleanup_servers = False
c.JupyterHub.template_vars = {
    "announcement_login": (
        "New here? Choose a username (lowercase letters, digits and dashes, "
        "starting with a letter) and a password of at least 8 characters, and "
        "type the session token shown in the room. Back again? Your username "
        "and password are enough. \"Invalid username or password\" when signing "
        "up? Check the token, or try another username: yours may be taken. "
        "Forgot your password? Ask an instructor."
    ),
}

# -- logins: choose your own ------------------------------------------------------
c.JupyterHub.authenticator_class = "cactup_cyol.authenticator.CYOLAuthenticator"
c.CYOLAuthenticator.accounts_db = "/srv/jupyterhub/cyol-accounts.sqlite"
c.CYOLAuthenticator.token_state = "/srv/jupyterhub/cyol-token.json"
# Every account was made with the session token (or by make_users), so every
# account may log in.
c.Authenticator.allow_all = True
# Admins' accounts are made on the machine (make_users --name), never by
# signing up with the token.
c.Authenticator.admin_users = {u.strip().lower() for u in env("HUB_ADMINS", "").split(",") if u.strip()}

# -- the attendees' containers ------------------------------------------------------
LABEL = "org.cactup-tutorial.attendee"
c.JupyterHub.spawner_class = "docker"
c.DockerSpawner.image = env("TUTORIAL_IMAGE", "cactup-tutorial:tutorial")
c.DockerSpawner.network_name = env("USER_NETWORK", "cactup-tutorial-users")
c.DockerSpawner.use_internal_ip = True
c.DockerSpawner.name_template = "cactup-{username}"
# A fresh container on every start (a new image takes effect at once); what
# lasts is in the two volumes.
c.DockerSpawner.remove = True
c.DockerSpawner.volumes = {
    # The attendee's home: installs, configs, simulations, notebooks.
    "cactup-home-{username}": "/home/cactus",
    # SLURM's state, so job ids keep counting up across restarts: cactup
    # must never mistake a new job for an old simulation's.
    "cactup-slurm-{username}": "/var/spool/slurmctld",
}
# The image has no CMD; its entrypoint starts SLURM, then this.
c.DockerSpawner.cmd = ["jupyterhub-singleuser"]
c.DockerSpawner.mem_limit = env("MEM_LIMIT", "8G")
extra_host_config = {
    # OpenMPI's shared-memory transport.
    "shm_size": "1g",
    "pids_limit": 4096,
}
if env("GPU", "0") == "1":
    extra_host_config["device_requests"] = [
        docker.types.DeviceRequest(count=-1, capabilities=[["gpu"]]),
    ]
c.DockerSpawner.extra_host_config = extra_host_config
c.Spawner.default_url = "/lab/tree/notebooks/01-getting-started.ipynb"
# The entrypoint starts the update site and SLURM before the notebook server.
c.Spawner.http_timeout = 180
c.Spawner.start_timeout = 300

# Each container gets CPUS_PER_ATTENDEE cores of its own (a cpuset): `nproc`,
# OpenMP and MPI then see exactly those, where a CPU quota would show them
# every core on the machine. See cactup_cyol/cpusets.py.
CPUSET = env("CPUSET", "1") == "1"
with open("/sys/devices/system/cpu/online") as f:
    assigner = Assigner(slices(parse_cpu_list(f.read()), int(env("CPUS_PER_ATTENDEE", "4"))))


def running_cpusets() -> dict[str, str]:
    client = docker.from_env()
    try:
        return {ct.name: ct.attrs.get("HostConfig", {}).get("CpusetCpus", "")
                for ct in client.containers.list(filters={"label": LABEL})}
    finally:
        client.close()


async def pre_spawn(spawner):
    # A label to find the attendees' containers by (the hub's and Caddy's
    # names start with cactup- too).
    spawner.extra_create_kwargs = {"hostname": "cactup-tutorial", "labels": {LABEL: spawner.user.name}}
    if not CPUSET:
        return
    running = await asyncio.get_running_loop().run_in_executor(None, running_cpusets)
    chosen = assigner.assign(spawner.object_name, running)
    spawner.extra_host_config = {**extra_host_config, "cpuset_cpus": chosen}
    spawner.log.info("%s gets CPUs %s", spawner.user.name, chosen)


def post_stop(spawner):
    assigner.release(spawner.object_name)


c.Spawner.pre_spawn_hook = pre_spawn
c.Spawner.post_stop_hook = post_stop

# -- services ------------------------------------------------------------------------
# The idle culler judges by notebook activity only, and stops a server whose
# jobs may still be running: SLURM requeues them when the container starts
# again, but the timeout should outlast the longest queue (4 hours). It looks
# every 5 minutes.
CULL_TIMEOUT = int(env("CULL_TIMEOUT", str(5 * 3600)))
c.JupyterHub.services = [
    {
        "name": "idle-culler",
        "command": [sys.executable, "-m", "jupyterhub_idle_culler", f"--timeout={CULL_TIMEOUT}", "--cull-every=300"],
    },
    {
        "name": "cyol-rotator",
        "command": [sys.executable, "-m", "cactup_cyol.rotator"],
        "environment": {
            "PYTHONPATH": "/srv/cactup_cyol",
            "CYOL_STATE": "/srv/jupyterhub/cyol-token.json",
            "CYOL_SHARED": "/srv/cactup-tutorial",
            "CYOL_SHARED_GID": env("SHARED_GID", ""),
            "CYOL_TOKEN_ROTATE": env("TOKEN_ROTATE_MINUTES", "60"),
            "CYOL_TOKEN_GRACE": env("TOKEN_GRACE_MINUTES", "5"),
        },
    },
]
c.JupyterHub.load_roles = [
    {
        "name": "idle-culler",
        "scopes": ["list:users", "read:users:activity", "read:servers", "delete:servers"],
        "services": ["idle-culler"],
    },
]
