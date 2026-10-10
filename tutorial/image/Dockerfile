# syntax=docker/dockerfile:1.7
#
# The Cactup tutorial image. Build it with tutorial/image/build.sh, which
# computes the cactup build stamps from git and passes them in; the build
# context is the repository root.
#
# Stages:
#   labext    the prebuilt JupyterLab extension
#   cactup    two static distribution builds of cactup from this checkout
#   base      Debian trixie with the tools every later stage needs
#   platform  base + the Einstein Toolkit toolchain, SLURM and munge
#   lab       platform + the git mirrors, JupyterLab, the tutorial package,
#             the local update site cactup is installed from, and the
#             attendees' home skeleton
#
# The mirrors come from the `mirrors` build context (see build.sh).

ARG TARGET=x86_64-unknown-linux-musl

# -- labext ---------------------------------------------------------------------
FROM node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c AS labext
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*
# Every Python package version is pinned (constraints.txt) and every npm one
# (package-lock.json): a rebuild installs exactly what was tested.
COPY tutorial/jupyter/constraints.txt /tmp/constraints.txt
RUN python3 -m venv /venv && /venv/bin/pip install --no-cache-dir -c /tmp/constraints.txt jupyterlab
ENV PATH=/venv/bin:$PATH
WORKDIR /src/labext
COPY tutorial/jupyter/labext/package.json tutorial/jupyter/labext/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
COPY tutorial/jupyter/labext/ ./
RUN npm run build && npm test

# -- cactup ---------------------------------------------------------------------
# Built twice from the same source with two stamps: "previous" is what the
# installer installs, "current" is what the update check offers, so the first
# cactup command after notebook 1's install performs a real auto-update.
FROM rust:1-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS cactup
ARG TARGET
ARG CACTUP_PREVIOUS_ID
ARG CACTUP_PREVIOUS_DATE
ARG CACTUP_CURRENT_ID
ARG CACTUP_CURRENT_DATE
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools cmake \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$TARGET"
WORKDIR /src
# From the pinned commit's snapshot (build.sh's cactup-src context), never
# from the checkout.
COPY --from=cactup-src Cargo.toml Cargo.lock build.rs ./
# The docs generator is a workspace member, so cargo needs its manifest.
COPY --from=cactup-src cactupdocs/Cargo.toml cactupdocs/Cargo.toml
COPY --from=cactup-src cactupdocs/src/ cactupdocs/src/
COPY --from=cactup-src src/ src/
COPY --from=cactup-src resources/ resources/
COPY --from=cactup-src mdb/GENERATION mdb/GENERATION
COPY --from=cactup-src mdb/generic/ mdb/generic/
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
# The base image by digest and the Debian archive by snapshot date: together
# they fix every package, so rebuilding the image months later installs the
# same toolchain and reuses the same bakes (see the README's bake cache keys).
# Move both on purpose, together; an empty DEBIAN_SNAPSHOT uses the live
# archive.
FROM debian:trixie@sha256:9cc080028c43b27d2074d63a5f9caf7166d731494965616c1a6d2827a004585c AS base
ARG DEBIAN_SNAPSHOT=20260929T000000Z
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 LC_ALL=C.UTF-8
RUN set -eu; \
    if [ -n "$DEBIAN_SNAPSHOT" ]; then \
      sed -i \
        -e "s|^URIs: http://deb.debian.org/debian-security\$|URIs: http://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT|" \
        -e "s|^URIs: http://deb.debian.org/debian\$|URIs: http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT|" \
        /etc/apt/sources.list.d/debian.sources; \
      echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot; \
    fi
RUN apt-get update && apt-get install -y --no-install-recommends \
      bash ca-certificates curl git jq less procps python3 python3-venv \
      tini vim-tiny \
    && rm -rf /var/lib/apt/lists/*
# One user for every attendee: baked builds embed absolute paths, so every
# container must put the same files in the same places.
RUN useradd --create-home --uid 1000 --shell /bin/bash cactus

# -- cuda -----------------------------------------------------------------------
# The CUDA toolkit, for baking the GPU config (B3) only: the tutorial image
# has none (see "Bakes" in the README). CUDA 13 from NVIDIA's repository for
# Debian 13, whose nvcc accepts gcc 14 as its host compiler. Its own stage on
# the base image, so the lab image's rebuilds never download it again; the
# bake container mounts the toolkit from a container of this image.
FROM base AS cuda
ARG CUDA_VERSION=13.4
RUN set -eu; \
    cd /tmp; \
    curl -fsSLO https://developer.download.nvidia.com/compute/cuda/repos/debian13/x86_64/cuda-keyring_1.1-1_all.deb; \
    dpkg -i cuda-keyring_1.1-1_all.deb; rm cuda-keyring_1.1-1_all.deb; \
    apt-get update; \
    apt-get install -y --no-install-recommends "cuda-toolkit-$(echo "$CUDA_VERSION" | tr . -)"; \
    rm -rf /var/lib/apt/lists/*; \
    "/usr/local/cuda-$CUDA_VERSION/bin/nvcc" --version
VOLUME /usr/local/cuda-${CUDA_VERSION}

# -- platform -------------------------------------------------------------------
# What a cluster's login node would offer: compilers, MPI and the Debian
# libraries the tutorial's optionlists use (see mdb/cactup-tutorial), and a
# single-node SLURM (started by the entrypoint).
FROM base AS platform
RUN apt-get update && apt-get install -y --no-install-recommends \
      gcc g++ gfortran make cmake pkg-config perl patch bzip2 xz-utils unzip \
      file rsync bc time \
      libopenmpi-dev openmpi-bin libhdf5-openmpi-dev libadios2-mpi-c-dev libadios2-mpi-c++11-dev \
      libfftw3-dev libgsl-dev libhwloc-dev libyaml-cpp-dev zlib1g-dev \
      libsilo-dev libblas-dev liblapack-dev libboost-dev \
      gdb slurm-wlm munge \
    && rm -rf /var/lib/apt/lists/* \
    && rm -f /etc/munge/munge.key
# (The package creates a munge key; every container must make its own, or any
# holder of the image could forge SLURM credentials for another container.)

# -- lab ------------------------------------------------------------------------
FROM platform AS lab
ARG TARGET
ARG CACTUP_PREVIOUS_ID
ARG CACTUP_PREVIOUS_DATE
ARG CACTUP_CURRENT_ID
ARG CACTUP_CURRENT_DATE
RUN python3 -m venv /opt/venv
ENV PATH=/home/cactus/.cactup/bin:/opt/venv/bin:$PATH
COPY tutorial/jupyter/constraints.txt /tmp/constraints.txt
RUN --mount=type=cache,target=/root/.cache/pip \
    pip install --retries 10 --timeout 60 --resume-retries 5 -c /tmp/constraints.txt \
      jupyterlab jupyterhub jupytext ipykernel numpy pandas matplotlib
# Every repository the tutorial installs or refetches, and the rules that send
# git (and cactup's git layer) to them instead of upstream; see the README.
COPY --from=mirrors / /opt/cactup-mirrors/
# Owned by root, read by everyone: git serves a repository owned by another
# user only when it is a declared safe directory.
RUN set -eu; \
    rm -f /opt/cactup-mirrors/mirrors.lock; \
    chmod -R go-w /opt/cactup-mirrors; \
    git config --system include.path /opt/cactup-mirrors/mirrors.gitconfig; \
    git config --system --add safe.directory '/opt/cactup-mirrors/*'
COPY tutorial/jupyter/ /src/cactup-tutorial/
COPY --from=labext /src/cactup_tutorial/labextension/ /src/cactup-tutorial/cactup_tutorial/labextension/
RUN --mount=type=cache,target=/root/.cache/pip \
    pip install --retries 10 --timeout 60 -c /tmp/constraints.txt '/src/cactup-tutorial[lab]' \
    && rm -rf /src/cactup-tutorial /tmp/constraints.txt

# The local update site: one root that offers both builds. The installer reads
# <target>/cactup.sha256, which names "previous"; cactup's update check reads
# latest.json, which offers "current".
COPY tutorial/image/assemble-update-root.sh /opt/cactup-tutorial/tools/
COPY --from=cactup-src cactup-init.sh mdb/GENERATION /opt/cactup-tutorial/tools/
COPY --from=cactup /out/ /tmp/cactup-builds/
RUN set -eu; \
    tools=/opt/cactup-tutorial/tools; \
    root=/opt/cactup-tutorial/update-root; \
    sh "$tools/assemble-update-root.sh" /tmp/update-root-previous "$TARGET" \
       /tmp/cactup-builds/cactup-previous "$CACTUP_PREVIOUS_ID" "$CACTUP_PREVIOUS_DATE"; \
    sh "$tools/assemble-update-root.sh" "$root" "$TARGET" \
       /tmp/cactup-builds/cactup-current "$CACTUP_CURRENT_ID" "$CACTUP_CURRENT_DATE"; \
    cp "/tmp/update-root-previous/$TARGET/cactup-$CACTUP_PREVIOUS_ID" "$root/$TARGET/"; \
    cp "/tmp/update-root-previous/$TARGET/cactup.sha256" "$root/$TARGET/cactup.sha256"; \
    rm -rf /tmp/update-root-previous

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
# The tutorial's thornlists, read-only: configs record their thornlist's path
# and keep using it, so an edit would change every later build's fingerprint.
COPY tutorial/thornlists/*.th /opt/cactup-tutorial/thornlists/
RUN chmod 0755 /opt/cactup-tutorial/thornlists && chmod 0444 /opt/cactup-tutorial/thornlists/*.th

# The pristine home every attendee starts from (and `cactup-tutorial-reset`
# restores). cactup is not in it: notebook 1 installs it. What is: the knobs
# pointing cactup at the local update site and turning off its random tips
# after commands (every cell is a terminal, so they'd appear in one cell in
# eight, and make the notebooks' output differ run to run), and the global
# .cactupignore that
# keeps editor files out of thorn shapes. Set with the current build itself,
# so the database has exactly the format cactup writes.
USER cactus
WORKDIR /home/cactus
RUN set -eu; \
    /tmp/cactup-builds/cactup-current knob update-url http://127.0.0.1:8765 </dev/null; \
    /tmp/cactup-builds/cactup-current knob wisdom-frequency off </dev/null; \
    printf '%s\n' \
      '# Files editors leave next to the ones they edit; they are not part of' \
      '# a thorn (see "cactup build" in the documentation).' \
      '.ipynb_checkpoints/' \
      '.*.sw?' \
      '.~*' > "$HOME/.cactup/cactupignore"; \
    rm -f "$HOME/.cactup/update-check"
USER root
# The marker says a home has been seeded; it is part of the skeleton, so the
# image's own home and every home restored from the skeleton carry it.
RUN install -o cactus -g cactus -m 0644 /dev/null /home/cactus/.cactup-tutorial-seeded \
    && cp -a /home/cactus /opt/cactup-tutorial/skel && rm -rf /tmp/cactup-builds

EXPOSE 8888
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/sbin/cactup-tutorial-entrypoint"]

# -- tutorial -------------------------------------------------------------------
# The lab image plus the precomputed builds the make shim restores (see
# "Bakes" in the README). build.sh bakes them in a container started from the
# lab image and passes the ones this image needs as the `bakes` context.
FROM lab AS tutorial
COPY --from=bakes / /opt/cactup-bakes/

# The notebooks, last, so editing one rebuilds only this layer. Their sources
# are jupytext MyST files; attendees get .ipynb files in ~/notebooks, and
# cactup-tutorial-reset --notebook restores one from the pristine copies.
COPY tutorial/notebooks/*.md /tmp/notebooks/
RUN set -eu; \
    pristine=/opt/cactup-tutorial/notebooks; \
    mkdir -p "$pristine"; \
    for src in /tmp/notebooks/*.md; do \
      jupytext --quiet --to ipynb -o "$pristine/$(basename "$src" .md).ipynb" "$src"; \
    done; \
    chmod 0755 "$pristine"; chmod 0644 "$pristine"/*.ipynb; rm -rf /tmp/notebooks; \
    for home in /home/cactus /opt/cactup-tutorial/skel; do \
      install -d -o cactus -g cactus "$home/notebooks"; \
      install -o cactus -g cactus -m 0644 "$pristine"/*.ipynb "$home/notebooks/"; \
    done
