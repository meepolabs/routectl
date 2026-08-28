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
# base (Alpine) cannot load the binary at all. ubuntu:26.04 ships glibc 2.43,
# which clears the floor.
#
# AN LTS, DELIBERATELY. The digest pin freezes the base layer but NOT the
# `apt-get update` layer below, which resolves against a live archive: once a
# release leaves support its archive is rotated to old-releases and that layer
# starts failing 404, so the committed Dockerfile stops building for the
# contributor who has no host artifact. An interim release carries a ~9-month
# clock and would hand this image a build deadline. 26.04 is supported to
# 2031-05-29.
FROM ubuntu@sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b
# ubuntu:26.04 LTS

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
# Versions are deliberately unpinned. Pinning would freeze this layer against
# an archive that still moves for security updates on an LTS, so a pin goes
# stale and starts failing to resolve -- the same unbuildable-for-a-contributor
# outcome the LTS base above exists to avoid. The base digest pins the
# toolchain-relevant surface; these are runtime utilities the drivers invoke by
# name, not by version, and the self-tests assert their PRESENCE.
# hadolint ignore=DL3008
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
# The installer writes per-build machine-identity state (`machineID`, `userID`,
# a `firstStartTime`, and a timestamped backup). It is randomly regenerated per
# build and never read at run time, so it is not a credential and not host
# identity -- but it makes two builds of the same committed Dockerfile produce
# byte-different layers, which is the divergence the digest pin exists to
# prevent. Removed in the same layer that creates it.
#
# The smoke test runs as the UNPRIVILEGED uid, not as root. `chmod -R a+rX`
# above is what lets the run-time uid traverse the install root and execute the
# binary; root ignores mode bits, so a root-only `claude --version` passes even
# when those bits are wrong and the failure surfaces later, inside a paid
# capture run. `nobody` has no passwd HOME, which is also the run-time shape.
RUN mkdir -p /opt/claude \
    && HOME=/opt/claude sh -c 'curl -fsSL https://claude.ai/install.sh | bash -s -- "$1"' _ "$CLAUDE_VERSION" \
    && ln -s /opt/claude/.local/bin/claude /usr/local/bin/claude \
    && rm -rf /opt/claude/.claude.json /opt/claude/.claude /opt/claude/.cache \
    && chmod -R a+rX /opt/claude \
    && su -s /bin/sh -c 'claude --version' nobody
