# The tutorial's JupyterHub: the official hub image, the version the
# attendees' image has (jupyterhub-singleuser must match it), plus
# DockerSpawner, the idle culler and choose-your-own login.
FROM quay.io/jupyterhub/jupyterhub:5.5.2@sha256:1c38e6f0178c49b7b5fde7f119596a6fc8aecd66dc304403f5c1f25cfd843b5a

COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt && rm /tmp/requirements.txt

COPY cactup_cyol /srv/cactup_cyol/cactup_cyol
ENV PYTHONPATH=/srv/cactup_cyol
COPY jupyterhub_config.py /srv/jupyterhub_config.py

WORKDIR /srv/jupyterhub
CMD ["jupyterhub", "-f", "/srv/jupyterhub_config.py"]
