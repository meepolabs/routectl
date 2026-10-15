# Capture-cell image: a thin client-installer plus the runtime deps the
# shipped drivers invoke. It carries NO routectl binary and NO Rust
# toolchain -- the wrapper bind-mounts the host-built `target/release/routectl`
# read-only via ROUTECTL_BIN, and bind-mounts the repo read-only too. The only
# files copied in are pi's committed package manifest and lock. Nothing in the
# capture corpus is baked into a layer, and no credential, seat file, or host
# identity ever enters one: the upstream token arrives at run time as an
# environment variable.
#
# Base pinned BY DIGEST, not by the floating `ubuntu:26.04` tag: a floating
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

# Codex ships a single static-musl binary with no install script, so there is
# no per-build machine-identity state to remove, unlike the Claude installer
# above. Ownership still stays root -- this RUN executes as root like every
# other layer -- and what makes the tree usable by the unprivileged runtime
# uid is the mode bits (`chmod -R a+rX`), verified by the smoke test below.
#
# Same shape as CLAUDE_VERSION otherwise: build ARG for convenience only (the
# fixture record is `meta.client.version` off the running binary), 0755 on
# the install root so the unprivileged run-time uid can traverse and execute,
# and an absolute symlink into /usr/local/bin so the binary is reachable
# regardless of HOME.
ARG CODEX_VERSION=0.151.0

# The release asset name encodes the arch, matching this Dockerfile's
# existing single-arch (x86_64) assumption -- no TARGETARCH here either.
# tar/gunzip are already present in the base image; no new apt dependency.
#
# The smoke test runs as the UNPRIVILEGED uid, same rationale as claude's:
# root ignores mode bits, so a root-only check would pass even if the mode
# bits were wrong. `nobody` has no writable passwd HOME, so codex's own
# best-effort PATH-alias write on startup fails harmlessly (a warning on
# stderr, not a build failure) rather than leaving state in the layer.
RUN mkdir -p /opt/codex \
    && curl -fsSL "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-x86_64-unknown-linux-musl.tar.gz" \
        -o /tmp/codex.tar.gz \
    && tar -xzf /tmp/codex.tar.gz -C /opt/codex \
    && rm -f /tmp/codex.tar.gz \
    && ln -s /opt/codex/codex-x86_64-unknown-linux-musl /usr/local/bin/codex \
    && chmod -R a+rX /opt/codex \
    && su -s /bin/sh -c 'codex --version' nobody

# Node is the runtime pi needs (its package declares node >= 22.19). The
# official linux-x64 tarball, not the distro package: the archive's node
# moves with the archive, which is the unpinned-apt-layer drift the comment
# on the apt layer accepts for runtime utilities but not for an interpreter a
# client runs on. The checksum is the one nodejs.org publishes in the signed
# SHASUMS256.txt for this exact version; a download that does not match it
# fails the build here rather than landing an unverified interpreter.
#
# .tar.gz rather than .tar.xz: the base image has gzip and no xz, and adding
# an apt dependency to save a few MB of download is the wrong trade.
# --no-same-owner because the tarball records its builder's uid.
ARG NODE_VERSION=24.21.0
ARG NODE_SHA256=6e1db87ef58b8819e5d5402eff1536491b18edd8eb7bee5ef7897876e88dc5ff

RUN mkdir -p /opt/node \
    && curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.gz" \
        -o /tmp/node.tar.gz \
    && printf '%s  %s\n' "$NODE_SHA256" /tmp/node.tar.gz >/tmp/node.sha256 \
    && sha256sum -c /tmp/node.sha256 \
    && rm -f /tmp/node.sha256 \
    && tar -xzf /tmp/node.tar.gz -C /opt/node --strip-components=1 --no-same-owner \
    && rm -f /tmp/node.tar.gz \
    && ln -s /opt/node/bin/node /usr/local/bin/node \
    && ln -s /opt/node/bin/npm /usr/local/bin/npm \
    && ln -s /opt/node/bin/npx /usr/local/bin/npx \
    && chmod -R a+rX /opt/node \
    && su -s /bin/sh -c 'node --version' nobody

# pi is installed from its published npm package at a pinned version; no
# source build. The whole dependency tree is fixed by the committed
# pi/package-lock.json (generated with `npm install --package-lock-only
# --ignore-scripts` on this image's node), and `npm ci` installs exactly that
# tree, verifying every tarball against the lock's integrity record. A
# registry install of the bare package would resolve its ranged transitive
# dependencies live, so two builds of one pin could carry different code.
# build.sh reads PI_VERSION out of that lock and passes it here only so the
# smoke test below can check what the lock installed; it selects nothing. The
# fixture record is still `meta.client.version` off the running binary.
#
# The npm cache and HOME are pointed at throwaway paths for this layer only
# and removed in it: npm writes its cache, debug logs, and update-notifier
# state under them, which is per-build state that would otherwise ride the
# layer. --ignore-scripts: two locked packages (@google/genai, protobufjs)
# declare install scripts, and neither is needed for the CLI to run (the
# smoke test below proves it), so none runs as root at build time.
#
# The smoke test runs as the unprivileged uid and requires `pi --version` to
# print exactly PI_VERSION, so a lock at a different version fails here.
# node writes a compile cache under /tmp on that first run; it is per-build
# state like the npm cache and goes with it.
ARG PI_VERSION=1.1.0

COPY pi/package.json pi/package-lock.json /opt/pi/

WORKDIR /opt/pi
RUN HOME=/tmp/pi-build-home npm_config_cache=/tmp/pi-npm-cache \
        npm ci --ignore-scripts \
        --no-audit --no-fund --no-update-notifier \
    && rm -rf /tmp/pi-build-home /tmp/pi-npm-cache \
    && ln -s /opt/pi/node_modules/.bin/pi /usr/local/bin/pi \
    && chmod -R a+rX /opt/pi \
    && su -s /bin/sh -c 'pi --version' nobody >/tmp/pi-version \
    && cat /tmp/pi-version \
    && test "$(cat /tmp/pi-version)" = "$PI_VERSION" \
    && rm -rf /tmp/pi-version /tmp/node-compile-cache
WORKDIR /

# hermes is installed from the UPSTREAM repository at a release tag, and the
# tag is checked against a pinned commit: a tag can be moved, a commit
# cannot, so a re-pointed tag fails the build instead of changing what the
# image carries under the same pin. Both are build ARGs; overriding one
# without the other fails the check below by design.
#
# Dependencies come from the project's own uv.lock (hash-verified, --frozen
# so nothing is re-resolved at build time), installed with the uv release
# that same commit pins in pm/lock.json and verified against the sha256 it
# records there. The venv uses the base image's python3, which satisfies the
# project's requires-python; uv is told never to download an interpreter.
# The `anthropic` extra is the SDK hermes needs for its anthropic_messages
# mode; the openai SDK for the other two modes is a core dependency.
#
# The .git directory is removed after the check, which turns off upstream's
# self-update probe (it needs a checkout) and keeps clone metadata out of the
# layer. The other trees removed with it are ones no CLI code path reads:
# test suites, the docs site, the desktop app, and packaging metadata (all
# outside upstream's own image build context, per its .dockerignore at the
# pinned commit), plus the eval scripts and the `.env.example` template. At
# the pinned commit the evals are named only in comments and in upstream's
# own exclusion lists, and `.env.example` is read only by the installers that
# seed a fresh `.env` from it, which this image never runs. Their fixtures and
# placeholders are credential-shaped by design, which is noise for the layer
# scan rather than coverage. With no checkout, `hermes --version` reads its
# identity from an install stamp, so one is written here from the pins --
# without the timestamp upstream's own stamp writer adds, which would make two
# builds of one pin byte-different. `external` names this image, not hermes, as the
# thing that replaces the tree on an update.
#
# The uv binary, its cache, and the lock files uv leaves in /tmp and in the
# venv are removed in the same layer; the smoke test runs as the unprivileged
# uid and requires the pinned version in the output.
ARG HERMES_VERSION=v0.21.7
ARG HERMES_COMMIT=71d34bad1045610252eda02ee79549436896e784

RUN set -eu \
    && git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$HERMES_VERSION" \
        https://github.com/NousResearch/hermes-agent.git /opt/hermes/src \
    && test "$(git -C /opt/hermes/src rev-parse HEAD)" = "$HERMES_COMMIT" \
    && rm -rf /opt/hermes/src/.git /opt/hermes/src/.github /opt/hermes/src/tests \
        /opt/hermes/src/website /opt/hermes/src/nix /opt/hermes/src/flake.nix /opt/hermes/src/flake.lock \
        /opt/hermes/src/evals /opt/hermes/src/.env.example \
    && find /opt/hermes/src/apps -mindepth 1 -maxdepth 1 ! -name shared -exec rm -rf {} + \
    && python3 -I -c 'import json, sys; a = json.load(open(sys.argv[1]))["packages"]["uv"]["artifacts"]["linux-x64"]; print(a["url"], file=open(sys.argv[2], "w")); print(a["sha256"] + "  /tmp/hermes-uv.tar.gz", file=open(sys.argv[3], "w"))' \
        /opt/hermes/src/pm/lock.json /tmp/hermes-uv.url /tmp/hermes-uv.sha256 \
    && mkdir -p /tmp/hermes-uv \
    && curl -fsSL "$(cat /tmp/hermes-uv.url)" -o /tmp/hermes-uv.tar.gz \
    && sha256sum -c /tmp/hermes-uv.sha256 \
    && tar -xzf /tmp/hermes-uv.tar.gz -C /tmp/hermes-uv --strip-components=1 --no-same-owner \
    && UV_CACHE_DIR=/tmp/hermes-uv-cache UV_PYTHON_DOWNLOADS=never UV_NO_CONFIG=1 \
        UV_PROJECT_ENVIRONMENT=/opt/hermes/venv \
        /tmp/hermes-uv/uv sync --project /opt/hermes/src --frozen --no-dev \
        --extra anthropic --python /usr/bin/python3 --quiet \
    && rm -rf /tmp/hermes-uv /tmp/hermes-uv.tar.gz /tmp/hermes-uv-cache /tmp/hermes-uv.url /tmp/hermes-uv.sha256 \
        /tmp/uv-*.lock /opt/hermes/venv/.lock \
    && python3 -I -c 'import json, sys; v = sys.argv[1].removeprefix("v"); json.dump({"schemaVersion": 2, "commit": sys.argv[2], "baseVersion": v, "displayVersion": v, "distance": 0, "dirty": False, "updateMechanism": "external"}, open(sys.argv[3], "w")); print(file=open(sys.argv[3], "a"))' \
        "$HERMES_VERSION" "$HERMES_COMMIT" /opt/hermes/src/install-stamp.json \
    && ln -s /opt/hermes/venv/bin/hermes /usr/local/bin/hermes \
    && chmod -R a+rX /opt/hermes \
    && su -s /bin/sh -c 'hermes --version' nobody >/tmp/hermes-version \
    && cat /tmp/hermes-version \
    && grep -qF "v${HERMES_VERSION#v}" /tmp/hermes-version \
    && rm -f /tmp/hermes-version
