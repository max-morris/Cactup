# syntax=docker/dockerfile:1.7
#
# The Cactup tutorial image. Build it with tutorial/image/build.sh, which
# computes the cactup build stamps from git and passes them in; the build
# context is the repository root.
#
# Stages:
#   labext   the prebuilt JupyterLab extension
#   cactup   two static distribution builds of cactup from this checkout
#   base     Debian trixie with the tools every later stage needs
#   lab      base + JupyterLab, the tutorial package, cactup preinstalled

ARG TARGET=x86_64-unknown-linux-musl

# -- labext ---------------------------------------------------------------------
FROM node:20-bookworm-slim AS labext
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv /venv && /venv/bin/pip install --no-cache-dir 'jupyterlab>=4.4,<5'
ENV PATH=/venv/bin:$PATH
WORKDIR /src/labext
COPY tutorial/jupyter/labext/package.json ./
RUN --mount=type=cache,target=/root/.npm npm install --no-audit --no-fund
COPY tutorial/jupyter/labext/ ./
RUN npm run build && npm test

# -- cactup ---------------------------------------------------------------------
# Built twice from the same source with two stamps: "previous" is what comes
# preinstalled, "current" is what the tutorial's update server offers, so the
# first cactup command in notebook 1 performs a real auto-update.
FROM rust:1-trixie AS cactup
ARG TARGET
ARG CACTUP_PREVIOUS_ID
ARG CACTUP_PREVIOUS_DATE
ARG CACTUP_CURRENT_ID
ARG CACTUP_CURRENT_DATE
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools cmake \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$TARGET"
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
# The docs generator is a workspace member, so cargo needs its manifest.
COPY cactupdocs/Cargo.toml cactupdocs/Cargo.toml
COPY cactupdocs/src/ cactupdocs/src/
COPY src/ src/
COPY resources/ resources/
COPY mdb/GENERATION mdb/GENERATION
COPY mdb/generic/ mdb/generic/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=cactup-tutorial-target \
    set -eu; \
    export "CC_$(printf '%s' "$TARGET" | tr - _)=musl-gcc" CACTUP_DIST=1; \
    mkdir -p /out; \
    for which in previous current; do \
      if [ "$which" = previous ]; then \
        CACTUP_BUILD_ID=$CACTUP_PREVIOUS_ID CACTUP_BUILD_DATE=$CACTUP_PREVIOUS_DATE; \
      else \
        CACTUP_BUILD_ID=$CACTUP_CURRENT_ID CACTUP_BUILD_DATE=$CACTUP_CURRENT_DATE; \
      fi; \
      export CACTUP_BUILD_ID CACTUP_BUILD_DATE; \
      cargo build --release --locked --target "$TARGET"; \
      cp "target/$TARGET/release/cactup" "/out/cactup-$which"; \
    done; \
    /out/cactup-current --version

# -- base -----------------------------------------------------------------------
FROM debian:trixie AS base
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 LC_ALL=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      bash ca-certificates curl git jq less procps python3 python3-venv \
      tini vim-tiny \
    && rm -rf /var/lib/apt/lists/*
# One user for every attendee: baked builds embed absolute paths, so every
# container must put the same files in the same places.
RUN useradd --create-home --uid 1000 --shell /bin/bash cactus

# -- lab ------------------------------------------------------------------------
FROM base AS lab
ARG TARGET
ARG CACTUP_PREVIOUS_ID
ARG CACTUP_PREVIOUS_DATE
ARG CACTUP_CURRENT_ID
ARG CACTUP_CURRENT_DATE
RUN python3 -m venv /opt/venv
ENV PATH=/home/cactus/.cactup/bin:/opt/venv/bin:$PATH
RUN --mount=type=cache,target=/root/.cache/pip \
    pip install --retries 10 --timeout 60 --resume-retries 5 'jupyterlab>=4.4,<5' 'jupyterhub>=5,<6' jupytext ipykernel \
      numpy pandas matplotlib
COPY tutorial/jupyter/ /src/cactup-tutorial/
COPY --from=labext /src/cactup_tutorial/labextension/ /src/cactup-tutorial/cactup_tutorial/labextension/
RUN --mount=type=cache,target=/root/.cache/pip pip install --retries 10 --timeout 60 /src/cactup-tutorial && rm -rf /src/cactup-tutorial

# The update roots: "init" offers the previous build (what the installer
# fetched when the image was built), and the one the update server serves at
# run time offers the current build.
COPY tutorial/image/assemble-update-root.sh cactup-init.sh mdb/GENERATION /opt/cactup-tutorial/tools/
COPY --from=cactup /out/ /tmp/cactup-builds/
RUN set -eu; \
    tools=/opt/cactup-tutorial/tools; \
    sh "$tools/assemble-update-root.sh" /opt/cactup-tutorial/update-root-init "$TARGET" \
       /tmp/cactup-builds/cactup-previous "$CACTUP_PREVIOUS_ID" "$CACTUP_PREVIOUS_DATE"; \
    sh "$tools/assemble-update-root.sh" /opt/cactup-tutorial/update-root "$TARGET" \
       /tmp/cactup-builds/cactup-current "$CACTUP_CURRENT_ID" "$CACTUP_CURRENT_DATE"; \
    rm -rf /tmp/cactup-builds

# Staged and copied into place so directory modes don't depend on the build
# host's checkout (umask): directories 0755, nothing group- or world-writable.
COPY tutorial/image/rootfs/ /tmp/rootfs/
RUN set -eu; \
    cd /tmp/rootfs; \
    find . -type d -exec chmod 0755 {} +; \
    find . -type f -exec chmod go-w {} +; \
    chmod 0755 usr/local/bin/cactup-tutorial-* usr/local/sbin/cactup-tutorial-entrypoint; \
    cp -a . /; \
    rm -rf /tmp/rootfs

# Install cactup exactly as a user would, with the one-line installer, from
# the "init" update root, then point the update-url knob at the run-time
# update server. The installer only accepts plain http for loopback.
USER cactus
WORKDIR /home/cactus
RUN set -eu; \
    cactup-tutorial-update-server /opt/cactup-tutorial/update-root-init & \
    server=$!; \
    cactup-tutorial-wait-update-server; \
    CACTUP_UPDATE_ROOT=http://127.0.0.1:8765 sh /opt/cactup-tutorial/tools/cactup-init.sh -y --no-modify-path; \
    kill "$server"; \
    cactup knob update-url http://127.0.0.1:8765; \
    rm -f "$HOME/.cactup/update-check"
USER root
# The pristine home every attendee starts from (and `cactup-tutorial-reset`
# restores).
RUN cp -a /home/cactus /opt/cactup-tutorial/skel

EXPOSE 8888
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/sbin/cactup-tutorial-entrypoint"]
