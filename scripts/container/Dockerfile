# Capture-cell image: a thin client-installer plus the runtime deps the
# shipped drivers invoke. It carries NO routectl binary and NO Rust
# toolchain -- the wrapper bind-mounts the host-built `target/release/routectl`
# read-only via ROUTECTL_BIN, and bind-mounts the repo read-only too. Nothing
# in the capture corpus is baked into a layer, and no credential, seat file,
# or host identity ever enters one: the upstream token arrives at run time as
# an environment variable.
#
# Base pinned BY DIGEST, not by the floating `ubuntu:25.04` tag: a floating
# tag lets two contributors resolve different glibc builds, and the divergence
# surfaces only as an unreproducible fixture. The comment records the
# human-readable version the digest stood for when it was pinned.
#
# glibc FLOOR is 2.39, set by `target/release/routectl` (it needs up to
# GLIBC_2.39); the client needs only 2.26, so routectl sets the floor. A musl
# base (Alpine) cannot load the binary at all. ubuntu:25.04 ships glibc 2.41,
# which clears the floor with headroom.
FROM ubuntu@sha256:27771fb7b40a58237c98e8d3e6b9ecdd9289cec69a857fccfb85ff36294dac20
# ubuntu:25.04

# Runtime deps, each justified by a shipped driver or the rig:
#   bash            - drivers use `set -eu`, arrays, bash-isms throughout
#   python3         - the scrub gate hard-fails without it by design
#   util-linux      - `script(1)` for `-q -e -c`, which allocates a REAL pty
#                     (the interactive driver's wire shape depends on it)
#   git             - the driver shells out to git for a synthetic identity
#   curl            - health poll and the client installer
#   iproute2        - `ss` for the isolation probe
#   coreutils       - sha256sum, mktemp -d
#   gawk sed grep   - JSON is hand-emitted with these; grep -E throughout
#   ca-certificates - TLS trust for the installer and health poll
#
# Deliberately ABSENT: a command-line JSON query tool. Verified zero uses in
# either capture script. Both hand-emit JSON specifically to avoid that
# dependency; installing it invites the next author to create a drift surface
# between the host and container capture paths. The tool's own name is kept
# out of this file so the absence can be asserted by a plain grep.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        python3 \
        util-linux \
        git \
        curl \
        iproute2 \
        coreutils \
        gawk \
        sed \
        grep \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Client version is a build ARG so two tags can be built from one Dockerfile
# (that is how the cross-cell version pin is satisfied later). The committed
# default is CONVENIENCE ONLY: the sole record of what a fixture was captured
# with is `meta.client.version` read off the RUNNING binary, and a mismatch
# between this pin and the observed version is a captured fact, not an error.
ARG CLAUDE_VERSION=2.1.246

# The official installer takes the version as its first argument and lands the
# client under $HOME/.local/bin. HOME is pinned for the duration of THIS layer
# only, never with ENV: the container runs as `--user $(id -u):$(id -g)` with
# no matching /etc/passwd entry, and an inherited HOME pointing at a root-owned
# directory is worse than an unset one. The runner sets a fresh HOME per run
# regardless, so the client must be reachable at an absolute path that does not
# depend on HOME -- hence the symlink into /usr/local/bin, which is on PATH
# whatever HOME turns out to be.
#
# 0755 on the install root so the unprivileged run-time uid can traverse it and
# execute the binary the symlink resolves to.
RUN mkdir -p /opt/claude \
    && HOME=/opt/claude sh -c 'curl -fsSL https://claude.ai/install.sh | bash -s -- "$1"' _ "$CLAUDE_VERSION" \
    && ln -s /opt/claude/.local/bin/claude /usr/local/bin/claude \
    && chmod -R a+rX /opt/claude \
    && claude --version
