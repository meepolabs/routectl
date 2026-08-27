#!/usr/bin/env bash
# Self-test for scrub-fixture.sh. Exits 0 when all assertions pass,
# non-zero on the first failure.
#
# Every case drives the REAL script inside a throwaway git repo whose
# environment IS the deny set: `$HOME` is overridden, `user.name` /
# `user.email` are set in the repo's own config, and `hostname` is a stub
# on `PATH`. That is what makes the environment-derived deny set testable
# at all -- the assertions never restate a pattern the script hardcodes,
# because the script hardcodes none.
#
# Both directions are pinned per class. The catching direction alone would
# pass against a gate that flags everything, and a gate with false
# positives is one whoever it blocks switches off -- so each class also
# gets realistic content that RESEMBLES it and must pass, drawn from this
# repo's own paths and identifiers.
#
# Requires python3 (the script does, for header JSON).
#
# Run it from anywhere:
#   bash scripts/scrub-fixture.test.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRUB="$HERE/scrub-fixture.sh"

# The seat store the script derives its `seat-session-id` deny values from
# lives at `$XDG_CONFIG_HOME/routectl/credentials.json`. Every invocation
# below therefore runs under a throwaway XDG: without this the suite would
# read whoever runs it, and a real session id would decide whether a case
# passes. Cases that need a seat plant their own store under their own work
# dir (see SEAT_STORE_JSON); this default is the "no seat at all" state.
TEST_XDG="$(mktemp -d)"
export XDG_CONFIG_HOME="$TEST_XDG"
trap 'rm -rf "$TEST_XDG"' EXIT

fails=0

# The identities the throwaway environment reports, and which the deny set
# must therefore derive. Deliberately not the operator's real ones.
FAKE_HOME_NAME="acontributor"
FAKE_GIT_NAME="Ada Contributor"
FAKE_GIT_EMAIL="ada@example.invalid"
FAKE_HOSTNAME="devbox-17"

if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL: python3 not found; this self-test cannot exercise header redaction"
    exit 1
fi

# The fake HOME lives under a mktemp dir, so its literal value is only
# known per case. Content therefore carries two tokens that every runner
# below expands against the case's actual fake home:
#
#   @HOME@     the literal $HOME the script will derive
#   @HOMEENC@  that same value with `/` replaced by `-`
#
# Substituting rather than hardcoding is what keeps the assertions honest:
# they assert against whatever the environment reports, exactly as the
# script does.
expand_home_tokens() {
    local content="$1" home="$2"
    content="${content//@HOMEENC@/${home//\//-}}"
    printf '%s' "${content//@HOME@/$home}"
}

# Build a throwaway repo, write `$2` as the fixture file named `$1` inside
# it, and run the scrub script over the fixture directory in the mode given
# by `$3` (`--check` or `--write`). Echoes the work directory so a caller
# can inspect the result; the caller removes it.
#
# The environment is the point: HOME points at a fake home under the work
# dir, the git identity is set in the repo's own config, `hostname`
# resolves to a stub, and XDG_CONFIG_HOME points at a throwaway seat-store
# root. `PATH` keeps the real tools the script needs.
#
# `$4` is the seat store to plant, as one of the SEAT_* spellings below.
# Empty (the default) plants nothing, which is the un-interrogable state.
run_scrub() {
    local filename="$1" content="$2" mode="$3" seat="${4:-}"
    local work
    work="$(mktemp -d)"
    local fake_home="$work/home/$FAKE_HOME_NAME"
    mkdir -p "$fake_home" "$work/repo/fixture" "$work/stubbin" "$work/xdg/routectl"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$FAKE_HOSTNAME" >"$work/stubbin/hostname"
    chmod +x "$work/stubbin/hostname"
    [ -z "$seat" ] || printf '%s' "$seat" >"$work/xdg/routectl/credentials.json"
    expand_home_tokens "$content" "$fake_home" >"$work/repo/fixture/$filename"
    (
        cd "$work/repo" || exit 2
        git init -q .
        git config user.name "$FAKE_GIT_NAME"
        git config user.email "$FAKE_GIT_EMAIL"
        HOME="$fake_home" XDG_CONFIG_HOME="$work/xdg" PATH="$work/stubbin:$PATH" \
            bash "$SCRUB" "$mode" fixture
    ) >"$work/scrub.log" 2>&1
    printf '%s\t%s\n' "$?" "$work"
}

# `$1` description, `$2` fixture filename, `$3` content, `$4` expected
# class, `$5` a seat store to plant (see run_scrub).
assert_caught() {
    local desc="$1" filename="$2" content="$3" expect_class="${4:-}" seat="${5:-}"
    local out rc work
    out="$(run_scrub "$filename" "$content" --check "$seat")"
    rc="${out%%$'\t'*}"
    work="${out#*$'\t'}"
    if [ "$rc" = "0" ]; then
        echo "FAIL: expected CAUGHT but --check passed -- $desc"
        fails=$((fails + 1))
    elif [ "$rc" != "1" ]; then
        echo "FAIL: expected exit 1 but got $rc -- $desc"
        cat "$work/scrub.log"
        fails=$((fails + 1))
    elif [ -n "$expect_class" ] && ! grep -q "  $expect_class\$" "$work/scrub.log"; then
        echo "FAIL: caught but did not name class '$expect_class' -- $desc"
        cat "$work/scrub.log"
        fails=$((fails + 1))
    else
        echo "PASS: caught -- $desc"
    fi
    rm -rf "$work"
}

assert_clean() {
    local desc="$1" filename="$2" content="$3" seat="${4:-}"
    local out rc work
    out="$(run_scrub "$filename" "$content" --check "$seat")"
    rc="${out%%$'\t'*}"
    work="${out#*$'\t'}"
    if [ "$rc" = "0" ]; then
        echo "PASS: clean -- $desc"
    else
        echo "FAIL: expected CLEAN but --check exited $rc -- $desc"
        cat "$work/scrub.log"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}

# Run --write over a fixture, then hand the resulting file's content to the
# assertion callback `$4` so a case can pin what survived and what did not.
# The callback sees `$WRITTEN` (the file's new content) and `$FAKEHOME` (the
# home path the script derived), so it can assert on the derived value
# without restating it.
assert_write() {
    local desc="$1" filename="$2" content="$3" predicate="$4"
    local out rc work written
    out="$(run_scrub "$filename" "$content" --write)"
    rc="${out%%$'\t'*}"
    work="${out#*$'\t'}"
    if [ "$rc" != "0" ]; then
        echo "FAIL: --write exited $rc -- $desc"
        cat "$work/scrub.log"
        fails=$((fails + 1))
        rm -rf "$work"
        return
    fi
    written="$(cat "$work/repo/fixture/$filename")"
    local fake_home="$work/home/$FAKE_HOME_NAME"
    if WRITTEN="$written" FAKEHOME="$fake_home" \
        FAKEHOMEENC="${fake_home//\//-}" eval "$predicate"; then
        echo "PASS: write -- $desc"
    else
        echo "FAIL: write predicate failed -- $desc (got: <withheld>)"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}

# A realistic captured ingress body: the shape a real fixture carries,
# with the personal token spliced in per case.
body_with() {
    printf '{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"%s"}]}' "$1"
}

# --- home-path -------------------------------------------------------
assert_caught "a literal home path in a captured body" \
    ingress_request.json \
    "$(body_with "cat @HOME@/.config/routectl/config.toml")" \
    home-path

assert_caught "the dash-encoded home path form from a .claude/projects dir name" \
    ingress_request.json \
    "$(body_with "session at @HOMEENC@-Desktop-build-routectl")" \
    home-path-encoded

# The placeholder the scrub writes must never re-trip the gate it feeds --
# otherwise a scrubbed fixture is permanently unpromotable.
assert_clean "the neutral placeholder home path is accepted" \
    ingress_request.json \
    "$(body_with "cat /home/user/.config/routectl/config.toml")"

assert_clean "the neutral dash-encoded placeholder is accepted" \
    ingress_request.json \
    "$(body_with "session at -home-user-Desktop-build-routectl")"

# --- home-prefix -----------------------------------------------------
assert_caught "a third party's home prefix is refused even when it is not this operator's" \
    ingress_request.json \
    "$(body_with "ls /home/someoneelse/Desktop")" \
    home-prefix

# The dash-encoded twin. The plain form above was refused for a third
# party while this one passed, so a body echoing someone else's
# `.claude/projects/` dir name landed clean.
assert_caught "a third party's dash-encoded home prefix is refused" \
    ingress_request.json \
    "$(body_with "session at -home-someoneelse-Desktop-build")" \
    home-prefix-encoded

assert_clean "a repo-relative path with no home prefix is accepted" \
    ingress_request.json \
    "$(body_with "open crates/routectl-cli/src/commands/serve.rs")"

# A body that legitimately discusses the word `home` must not trip the
# `/home/` rule: this is the false positive that gets a gate disabled.
assert_clean "the word home in prose is accepted" \
    ingress_request.json \
    "$(body_with "set the home directory for the routectl config, then go home")"

assert_clean "a homebrew-style path is not a home prefix" \
    ingress_request.json \
    "$(body_with "which routectl -> /opt/homebrew/bin/routectl")"

# The encoded rule requires a trailing `-` after the name, so an ordinary
# hyphenated phrase ending in the prefix word is not a dir name.
assert_clean "a hyphenated phrase containing home is not an encoded prefix" \
    ingress_request.json \
    "$(body_with "the flag is --set-home-dir and it takes a path")"

# --- git-author-name / git-author-email ------------------------------
# The named gap docs/DEVELOPMENT.md asks a contributor to eyeball: a body
# that captured `git log` output.
assert_caught "a git author name echoed by a captured git log body" \
    ingress_request.json \
    "$(body_with "commit 9ba78a0e\\nAuthor: $FAKE_GIT_NAME <someone@example.invalid>")" \
    git-author-name

assert_caught "a git author email echoed by a captured git log body" \
    ingress_request.json \
    "$(body_with "commit 9ba78a0e\\nAuthor: Someone Else <$FAKE_GIT_EMAIL>")" \
    git-author-email

assert_clean "a git log body with no configured identity in it is accepted" \
    ingress_request.json \
    "$(body_with "commit 9ba78a0e\\nAuthor: routectl maintainer <maintainer@example.com>")"

# --- hostname --------------------------------------------------------
assert_caught "the machine hostname echoed into a captured body" \
    ingress_request.json \
    "$(body_with "ssh into $FAKE_HOSTNAME and restart the daemon")" \
    hostname

assert_clean "a loopback base URL is not a hostname hit" \
    ingress_request.json \
    "$(body_with "base_url = \\\"http://localhost:11435/v1\\\"")"

# --- seat-session-id -------------------------------------------------
# routectl resolves a seat's stored `session_id` on the OAuth-bearer
# surface and puts it on the wire as `x-claude-code-session-id`, which the
# header rules classify VISIBLE -- so `--write` keeps the value and every
# fixture captured on that lane carries the same stable identifier. Both
# gates passed it before this class existed.
#
# The seat store is planted per case under the case's own throwaway XDG.
# Its shape mirrors crates/routectl-auth/src/oauth/types.rs
# (`CredentialsFile` -> `providers` -> `TokenRecord.session_id`). The token
# fields carry inert placeholders rather than key-shaped strings: the gate
# reads `session_id` and nothing else out of this document, so a realistic
# token here would buy no coverage while putting a credential-shaped
# literal in a committed file.
seat_store_with() {
    printf '{"schema_version":1,"providers":{"anthropic":{'
    printf '"access_token":"placeholder","refresh_token":"placeholder",'
    printf '"token_type":"Bearer","expires_at_unix":4102444800,'
    printf '"obtained_at_unix":1756252800,"session_id":"%s"}}}' "$1"
}

# A fresh uuid v4, generated per call. The reject fixture and the accept
# control BOTH use freshly generated values, so neither can pass by
# matching something this file hardcodes.
fresh_uuid_v4() {
    python3 -c 'import uuid; print(uuid.uuid4())'
}

# The value the planted seat stores, and therefore the one deny value the
# class must derive. Generated, never written down: a committed literal
# would be a fixture identifier the gate is welded to.
SEAT_SESSION_ID="$(fresh_uuid_v4)"
SEAT_STORE="$(seat_store_with "$SEAT_SESSION_ID")"

# The wire position D3 identifies: the cloak's metadata `user_id`, which
# carries the session id as its third key.
user_id_body_with() {
    body_with "{\\\"device_id\\\":\\\"abc\\\",\\\"account_uuid\\\":\\\"$(fresh_uuid_v4)\\\",\\\"session_id\\\":\\\"$1\\\"}"
}

assert_caught "the seat's session id in a captured metadata user_id" \
    ingress_request.json \
    "$(user_id_body_with "$SEAT_SESSION_ID")" \
    seat-session-id \
    "$SEAT_STORE"

# The header the router actually emits. Its name carries no credential
# substring and does not end in `-key`, so the header layer classifies it
# VISIBLE and `--write` leaves the value verbatim -- this class is the only
# thing that looks at it.
assert_caught "the seat's session id in the x-claude-code-session-id header" \
    ingress_request.headers.json \
    "[[\"content-type\",\"application/json\"],[\"x-claude-code-session-id\",\"$SEAT_SESSION_ID\"]]" \
    seat-session-id \
    "$SEAT_STORE"

# THE ACCEPT CONTROL, and the whole reason the class derives a value rather
# than matching a shape. A uuid in the metadata `user_id` position is
# ordinary traffic: `account_uuid` is minted fresh per provider instance
# (crates/routectl-providers/src/anthropic_api/cloak.rs) and is
# client-forwarded on this lane. Without this direction the rule degenerates
# into "reject any uuid", which refuses every legitimate id in a body.
assert_clean "a fresh uuid in the same metadata user_id position is accepted" \
    ingress_request.json \
    "$(user_id_body_with "$(fresh_uuid_v4)")" \
    "$SEAT_STORE"

# The same accept direction on the header layer, on the header D3 names as
# the collateral damage a shape rule would cause.
assert_clean "a fresh uuid as an x-client-request-id header value is accepted" \
    ingress_request.headers.json \
    "[[\"content-type\",\"application/json\"],[\"x-client-request-id\",\"$(fresh_uuid_v4)\"]]" \
    "$SEAT_STORE"

# --- ls-owner-column -------------------------------------------------
# The second named gap: an `ls -l` listing whose owner/group columns name
# a real account.
assert_caught "an ls -l owner column naming a real account" \
    ingress_request.json \
    "$(body_with "-rw-r--r-- 1 $FAKE_HOME_NAME staff 4096 Aug 25 10:00 config.toml")" \
    ls-owner-column

assert_caught "an ls -l directory entry with a non-neutral group" \
    ingress_request.json \
    "$(body_with "drwxr-xr-x 3 user devteam 4096 Aug 25 10:00 crates")" \
    ls-owner-column

assert_clean "an ls -l listing owned by the neutral placeholder account is accepted" \
    ingress_request.json \
    "$(body_with "-rw-r--r-- 1 user user 4096 Aug 25 10:00 config.toml")"

assert_clean "an ls -l listing owned by root is accepted" \
    ingress_request.json \
    "$(body_with "-rwxr-xr-x 1 root root 128 Aug 25 10:00 routectl")"

# A permission string in prose carries no owner column and must not match.
assert_clean "a bare mode string in prose is not an ls -l listing" \
    ingress_request.json \
    "$(body_with "chmod the socket to -rw-r--r-- before starting routectl")"

# Two listing variants beyond GNU `ls -l`, both carrying the same owner
# name: macOS `ls -l@` appends an xattr marker to the mode string, and
# `ls -o` omits the group column so a group-position field check reads the
# size instead of an account.
assert_caught "a macOS ls -l@ listing naming a real account" \
    ingress_request.json \
    "$(body_with "-rw-r--r--@ 1 $FAKE_HOME_NAME staff 4096 Aug 25 10:00 config.toml")" \
    ls-owner-column

assert_caught "an ls -o listing has no group column but still names an owner" \
    ingress_request.json \
    "$(body_with "-rw-r--r-- 1 $FAKE_HOME_NAME 4096 Aug 25 10:00 config.toml")" \
    ls-owner-column

assert_clean "a macOS ls -l@ listing owned by the neutral account is accepted" \
    ingress_request.json \
    "$(body_with "-rw-r--r--@ 1 user user 4096 Aug 25 10:00 config.toml")"

assert_clean "an ls -o listing owned by the neutral account is accepted" \
    ingress_request.json \
    "$(body_with "-rw-r--r-- 1 user 4096 Aug 25 10:00 config.toml")"

# --- bearer-token ----------------------------------------------------
assert_caught "an opaque bearer token pasted into a captured body" \
    ingress_request.json \
    "$(body_with "curl -H 'Authorization: Bearer sk-ant-oat01-AAAAAAAAAAAAAAAAAAAAAAAA'")" \
    bearer-token

# The scheme word is matched case-insensitively: a body carrying a pasted
# shell command is the reachable shape, and a caller's paste preserves
# whatever case the source used.
assert_caught "an all-caps BEARER scheme in a captured body" \
    ingress_request.json \
    "$(body_with "curl -H \\\"AUTHORIZATION: BEARER sk-xxxx-AAAABBBBCCCCDDDDEEEE\\\"")" \
    bearer-token

assert_caught "a mixed-case bearer scheme in a captured body" \
    ingress_request.json \
    "$(body_with "header was BeArEr AAAABBBBCCCCDDDDEEEEFFFF")" \
    bearer-token

# A token carrying `:` (vendor-scoped and JWT-adjacent shapes do) and one
# separated by more than a single space.
assert_caught "a colon-bearing token after a wrapped separator" \
    ingress_request.json \
    "$(body_with "Authorization:   Bearer v1:AAAABBBBCCCCDDDDEEEE")" \
    bearer-token

# The accept direction for the case-insensitive widening: the placeholder
# has no 16-char run in the token class, so it must still pass. Asserted
# explicitly rather than assumed, because widening a rule moves the
# boundary in both directions.
assert_clean "the bearer redaction placeholder is accepted" \
    ingress_request.json \
    "$(body_with "curl -H 'Authorization: Bearer [REDACTED]'")"

assert_clean "an all-caps BEARER followed by the placeholder is accepted" \
    ingress_request.json \
    "$(body_with "curl -H 'AUTHORIZATION: BEARER [REDACTED]'")"

# Prose naming the scheme with no token after it is documentation.
assert_clean "prose naming the bearer scheme with no token is accepted" \
    ingress_request.json \
    "$(body_with "set the Authorization header to a Bearer token before calling")"

# --- provider-key ----------------------------------------------------
# A raw vendor credential with NO scheme word: the `cat .env` /
# `.credentials.json` shape, which the bearer rule cannot see at all.
#
# Each fixture is ASSEMBLED from a prefix and an opaque run rather than
# written as one literal. Two of these are shaped closely enough to a live
# key that the repo's secret scanner flags the source line, and suppressing
# a scanner to keep a test fixture is the wrong trade -- the concatenation
# keeps every full key-shaped string out of the file while the body the
# script actually scans is identical.
FAKE_RUN="AbCdEf0123456789ABCDEFGH"
fake_key() { printf '%s%s' "$1" "${2:-$FAKE_RUN}"; }

assert_caught "an anthropic api key in a captured body" \
    ingress_request.json \
    "$(body_with "ANTHROPIC_API_KEY=$(fake_key 'sk-ant-api03-')")" \
    provider-key

assert_caught "an anthropic oauth token in a captured body" \
    ingress_request.json \
    "$(body_with "cat .credentials.json -> $(fake_key 'sk-ant-oat01-')")" \
    provider-key

assert_caught "an openai project key in a captured body" \
    ingress_request.json \
    "$(body_with "OPENAI_API_KEY=$(fake_key 'sk-proj-')")" \
    provider-key

assert_caught "an openrouter key in a captured body" \
    ingress_request.json \
    "$(body_with "OPENROUTER_API_KEY=$(fake_key 'sk-or-v1-')")" \
    provider-key

assert_caught "a github token in a captured body" \
    ingress_request.json \
    "$(body_with "GITHUB_TOKEN=$(fake_key 'ghp_' 'abcdefghijklmnopqrstuvwxyz0123')")" \
    provider-key

assert_caught "an aws access key id in a captured body" \
    ingress_request.json \
    "$(body_with "AWS_ACCESS_KEY_ID=$(fake_key 'AKIA' 'IOSFODNN7EXAMPLEXXXX')")" \
    provider-key

# The accept direction, and the reason the rule requires 16+ chars AFTER
# the prefix: a fixture legitimately discussing the prefix as
# documentation carries no key and must pass. routectl's own docs and
# config-error messages name these prefixes.
assert_clean "prose naming a vendor key prefix with no key after it is accepted" \
    ingress_request.json \
    "$(body_with "anthropic api keys start with sk-ant-api03- and oauth with sk-ant-oat01-")"

assert_clean "a short prefix-shaped token below the opaque-run floor is accepted" \
    ingress_request.json \
    "$(body_with "the placeholder in the docs is sk-proj-XXXX")"

assert_clean "a model id resembling no vendor key prefix is accepted" \
    ingress_request.json \
    "$(body_with "model = claude-sonnet-4-5-20250929, provider = anthropic")"

# --- the five vendor shapes with their own classes --------------------
# Each of these passed --check at exit 0 before the rules existed, so a real
# credential of the shape would NOT have been caught before promotion. Every fixture
# below is ASSEMBLED via fake_key for the same reason the provider-key
# block is: two of these shapes (`AIza`, JWT) are in the repo secret
# scanner's default ruleset, and suppressing a scanner to keep a test
# fixture is the wrong trade.
#
# A 35-char opaque run, the exact width the AIza rule requires.
FAKE_RUN_35="0123456789abcdefghijABCDEFGHIJ_-xyz"
# Hoisted: the withheld-value needle must stay byte-identical to its fixture,
# and four inline copies is how that coupling breaks silently.
FAKE_JWT_PAYLOAD="hbGciOiJSUzI1NiJ9.AAAABBBBCCCC.DDDDEEEEFFFF"

assert_caught "a google oauth access token in a captured body" \
    ingress_request.json \
    "$(body_with "GEMINI_OAUTH=$(fake_key 'ya29.')")" \
    google-oauth-token

assert_caught "a google api key in a captured body" \
    ingress_request.json \
    "$(body_with "GEMINI_API_KEY=$(fake_key 'AIza' "$FAKE_RUN_35")")" \
    google-api-key

assert_caught "a bare three-segment jwt in a captured body" \
    ingress_request.json \
    "$(body_with "id_token was $(fake_key 'eyJ' "$FAKE_JWT_PAYLOAD")")" \
    jwt

assert_caught "a temporary aws access key id in a captured body" \
    ingress_request.json \
    "$(body_with "AWS_ACCESS_KEY_ID=$(fake_key 'ASIA' 'IOSFODNN7EXAMPLE')")" \
    aws-temp-key-id

assert_caught "an nvidia api key in a captured body" \
    ingress_request.json \
    "$(body_with "NVIDIA_API_KEY=$(fake_key 'nvapi-')")" \
    nvidia-api-key

# The class name is the whole diagnostic. A refusal that echoes the token
# copies the leak into the CI log that reports it.
assert_value_withheld() {
    local desc="$1" filename="$2" content="$3" needle="$4"
    local out rc work
    out="$(run_scrub "$filename" "$content" --check)"
    rc="${out%%$'\t'*}"
    work="${out#*$'\t'}"
    if [ "$rc" != "1" ]; then
        echo "FAIL: expected exit 1 but got $rc -- $desc"
        fails=$((fails + 1))
    elif grep -qF -- "$needle" "$work/scrub.log"; then
        echo "FAIL: the refusal echoed the matched value -- $desc"
        fails=$((fails + 1))
    else
        echo "PASS: value withheld from the refusal -- $desc"
    fi
    rm -rf "$work"
}

assert_value_withheld "the jwt refusal names the class, never the token" \
    ingress_request.json \
    "$(body_with "id_token was $(fake_key 'eyJ' "$FAKE_JWT_PAYLOAD")")" \
    "$(fake_key 'eyJ' "$FAKE_JWT_PAYLOAD")"

# The accept direction for all five, drawn from REAL content rather than
# invented near-misses: a gate with false positives is one whoever it
# blocks switches off, so the accept set is part of the contract.

# The in-tree test constant at crates/routectl-providers/src/gemini/auth.rs:63.
# 11 chars after the dot, provably below the rule's {20,} floor.
assert_clean "the in-tree short ya29 test token is accepted" \
    ingress_request.json \
    "$(body_with "apply_bearer(rb, \\\"ya29.token-value\\\")")"

# Anchoring is what makes this pass: an `AIza` run sitting mid-base64 is
# not a token boundary. Unanchored, this shape fires 2-4 times per 64MB of
# random base64, and a captured SSE body is mostly base64.
assert_clean "a base64 png data uri carrying an AIza run mid-payload is accepted" \
    ingress_request.json \
    "$(body_with "![shot](data:image/png;base64,iVBORw0KGgoAAAANSUhEUg$(fake_key 'AIza' "$FAKE_RUN_35")AAAASUVORK5CYII=)")"

# One mid-payload accept control PER RULE. Without these, deleting
# "$ANCHOR_LEFT" from an individual rule leaves the whole suite green --
# measured: four of the five anchors were unverified, so they were
# decoration a tuning pass could drop in silence, and each unanchored rule
# reintroduces the measured false-positive mode inside base64 runs. Each
# fixture below embeds the rule's own prefix MID-token, where a boundary
# never occurs, so the rule must decline it.
assert_clean "a base64 run carrying a ya29 sequence mid-payload is accepted" \
    ingress_request.json \
    "$(body_with "blob=iVBORw0KGgoAAAANSUhEUgAAquwXya29.${FAKE_RUN_35}AAAASUVORK5CYII=")"

assert_clean "a base64 run carrying an eyJ-dotted sequence mid-payload is accepted" \
    ingress_request.json \
    "$(body_with "blob=iVBORw0KGgoAAAANSUheyJhbGciOiJSUzI1NiJ9.AAAABBBBCCCC.DDDDEEEEFFFFAAAASUVORK5CYII=")"

assert_clean "a base64 run carrying an ASIA sequence mid-payload is accepted" \
    ingress_request.json \
    "$(body_with "blob=iVBORw0KGgoAAAANSUhEUgASIAIOSFODNN7EXAMPLEAAAASUVORK5CYII=")"

assert_clean "a base64 run carrying an nvapi- sequence mid-payload is accepted" \
    ingress_request.json \
    "$(body_with "blob=iVBORw0KGgoAAAANSUhEUgnvapi-${FAKE_RUN_35}AAAASUVORK5CYII=")"

assert_clean "a bare sha256 hex digest is accepted" \
    ingress_request.json \
    "$(body_with "config_sha = 9f2e4c1b7a05d38e6410cb92fd7e5a3b08c1de49f6027ab5c3d18e9f40a27b61")"

assert_clean "a 40-char git rev is accepted" \
    ingress_request.json \
    "$(body_with "pinned at d51e5a3592cf4b7e08a1d6f3c29b5e470a8d1c26")"

assert_clean "an SRI sha384 integrity hash is accepted" \
    ingress_request.json \
    "$(body_with "integrity=\\\"sha384-oqVuAfXRKap7fdgcCY5uykM6R9GqQ8Kuxy9rx7HNQlGYl1kPzQho1wx4JwY8wC\\\"")"

assert_clean "a v7 uuid is accepted" \
    ingress_request.json \
    "$(body_with "request_id = 0192f8c1-7a3b-7d4e-8f01-23456789abcd")"

assert_clean "a dated model id is accepted by the new vendor rules" \
    ingress_request.json \
    "$(body_with "model = claude-sonnet-4-5-20250929 and gemini-2.5-pro-002")"

# Prose naming the prefixes with no token after them: routectl's own docs
# and config-error messages do exactly this.
assert_clean "prose naming the google and aws key prefixes with no token is accepted" \
    ingress_request.json \
    "$(body_with "google keys start AIza, oauth tokens ya29. and temporary aws ids ASIA")"

# --- the shape-coverage table ----------------------------------------
# PROVIDER_SHAPE_KINDS is read as TEXT by consumers that never execute the
# script, so a row may not name a rule the gate does not actually have --
# a stale row reads as coverage and provides none.
# The LEGACY dotted access-token form. `ya29.1.AAD...` has a short middle
# segment, so a rule whose body class excludes `.` sees only `1` after the
# first dot and declines -- measured: it passed at exit 0 while the modern
# single-run form was refused. The body class therefore admits `.`; the
# paired prose control below proves that does not let the rule run through
# sentence punctuation.
assert_caught "a legacy dotted google oauth token is caught" \
    ingress_request.json \
    "$(body_with "GEMINI_OAUTH=$(fake_key 'ya29.1.AADtN_V')")" \
    google-oauth-token

# The accept boundary for the legacy-form alternative. The rule is TWO
# alternatives (modern single run, or `ya29.<1-3 digits>.<opaque>`) rather
# than one class widened with `.`, because the widened form was measured to
# REFUSE all three fixtures below -- dotted prose with 20+ chars after the
# prefix. The first control alone passed on the LENGTH FLOOR, not on any
# punctuation boundary, so it did not prove what its name claimed.
assert_clean "prose naming a short ya29 value then continuing a sentence is accepted" \
    ingress_request.json \
    "$(body_with "tokens look like ya29.SHORT. Then a new sentence continues here.")"

assert_clean "a dotted ya29 prose run past the length floor is accepted" \
    ingress_request.json \
    "$(body_with "see ya29.oauth-token-refresh.flow.docs for the refresh flow")"

assert_clean "a long dot-separated ya29 word list is accepted" \
    ingress_request.json \
    "$(body_with "ya29.token.value.here.and.more.text.follows in the prose")"

assert_caught "a legacy dotted-digit google oauth token is still caught" \
    ingress_request.json \
    "$(body_with "GEMINI_OAUTH=$(fake_key 'ya29.1.AADtN_V')")" \
    google-oauth-token

# --- the two AWS body shapes -------------------------------------------
# bedrock was EXCLUDED from the shape table until these landed, on a reason
# that cited the HEADER layer -- which covers only credentials routectl itself
# puts on the wire and says nothing about one arriving in a captured BODY.
# All three fixtures below passed --check at exit 0 before these rules.
assert_caught "an aws secret access key assignment in a captured body" \
    ingress_request.json \
    "$(body_with "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY$FAKE_RUN")" \
    aws-credential-assignment

assert_caught "a lowercase aws credentials-file secret is caught" \
    ingress_request.json \
    "$(body_with "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCY$FAKE_RUN")" \
    aws-credential-assignment

assert_caught "an aws session token assignment in a captured body" \
    ingress_request.json \
    "$(body_with "AWS_SESSION_TOKEN=IQoJb3JpZ2luX2Vj$FAKE_RUN$FAKE_RUN")" \
    aws-credential-assignment

assert_caught "a bedrock short-term api key in a captured body" \
    ingress_request.json \
    "$(body_with "AWS_BEARER_TOKEN_BEDROCK=$(fake_key 'bedrock-api-key-')&Version=1")" \
    bedrock-api-key

# The accept side, and the reason these rules key on the ASSIGNMENT rather than
# the value: the secret is 40 opaque base64 chars and the session token is a
# long opaque run, so a value-keyed rule would be the forbidden entropy
# matcher. Every spelling below carries the NAME with no secret after it, which
# is exactly how routectl's own config and docs reference them.
assert_clean "an env ref to the aws secret key in config prose is accepted" \
    ingress_request.json \
    "$(body_with "secret_key_ref = \"env://AWS_SECRET_ACCESS_KEY\"")"

assert_clean "an env ref to the aws session token is accepted" \
    ingress_request.json \
    "$(body_with "session_token_ref = \"env://AWS_SESSION_TOKEN\"")"

assert_clean "prose naming the aws secret key variable is accepted" \
    ingress_request.json \
    "$(body_with "set AWS_SECRET_ACCESS_KEY in your environment before running")"

assert_clean "prose naming the bedrock api key prefix is accepted" \
    ingress_request.json \
    "$(body_with "the bedrock-api-key- prefix names a short-term key")"

# --- escaped-newline boundary, both directions -------------------------
# A captured body is single-line JSON, so an embedded newline is the two
# BYTES `\` + `n`. `n` is a token character, so a credential that BEGINS an
# escaped line presents no word boundary unless ANCHOR_LEFT accepts the
# escape. Measured before the fix: all five rules exited 0 on exactly this
# shape while the same token after `=` was refused, and 554 of the 250
# committed live fixtures carry `\n` followed by a token character. These
# five assertions are what keep the escape alternative in ANCHOR_LEFT.
assert_caught "a google api key beginning an escaped line is caught" \
    ingress_request.json \
    "$(body_with "here:\\n$(fake_key 'AIza' "$FAKE_RUN_35")\\nend")" \
    google-api-key

assert_caught "a google oauth token beginning an escaped line is caught" \
    ingress_request.json \
    "$(body_with "here:\\n$(fake_key 'ya29.')\\nend")" \
    google-oauth-token

assert_caught "a jwt beginning an escaped line is caught" \
    ingress_request.json \
    "$(body_with "here:\\n$(fake_key 'eyJ' "$FAKE_JWT_PAYLOAD")\\nend")" \
    jwt

assert_caught "a temporary aws key id beginning an escaped line is caught" \
    ingress_request.json \
    "$(body_with "here:\\n$(fake_key 'ASIA' 'IOSFODNN7EXAMPLE')\\nend")" \
    aws-temp-key-id

assert_caught "an nvidia api key beginning an escaped line is caught" \
    ingress_request.json \
    "$(body_with "here:\\n$(fake_key 'nvapi-')\\nend")" \
    nvidia-api-key

assert_shape_table_rule_ids_exist() {
    local block rows regexes row ids id missing="" row_count=0
    block="$(sed -n \
        '/^# --- BEGIN PROVIDER_SHAPE_KINDS ---$/,/^# --- END PROVIDER_SHAPE_KINDS ---$/p' \
        "$SCRUB")"

    # Non-vacuity: without these the loop below iterates zero rows and
    # reports coverage it never checked.
    if ! printf '%s\n' "$block" | grep -q '^PROVIDER_SHAPE_KINDS=($' ||
        ! printf '%s\n' "$block" | grep -q '^PROVIDER_SHAPE_EXCLUDED=($'; then
        echo "FAIL: the shape-coverage block is not two closed array literals between the sentinels"
        fails=$((fails + 1))
        return
    fi

    rows="$(printf '%s\n' "$block" | grep -E '^  "[a-z0-9-]+=' || true)"
    # ONLY the credential-shape regexes, never every `*_RE=` line. A rule id
    # is a claim that some CREDENTIAL rule keys on it, and the wider haystack
    # accepts an incidental substring of an unrelated rule: `rwx` appears in
    # LS_MODE_RE, `bearer` in BEARER_RE, so a bogus row like `some-kind=rwx`
    # would read as covered by a gate that has no such rule -- the exact
    # silent under-read this guard exists to prevent, arriving through it.
    regexes="$(grep -E '^(PROVIDER_KEY|GOOGLE_OAUTH_TOKEN|GOOGLE_API_KEY|JWT|AWS_TEMP_KEY_ID|NVIDIA_API_KEY|BEDROCK_API_KEY|AWS_CRED_ASSIGN)_RE=' "$SCRUB")"

    while IFS= read -r row; do
        [ -n "$row" ] || continue
        row_count=$((row_count + 1))
        ids="${row#*=}"
        ids="${ids%\"}"
        while IFS= read -r id; do
            [ -n "$id" ] || continue
            printf '%s\n' "$regexes" | grep -qF -- "$id" ||
                missing+=" $id"
        done < <(printf '%s\n' "${ids//,/$'\n'}")
    done < <(printf '%s\n' "$rows")

    # KIND-SIDE check. The rule-id direction above proves a row names a real
    # regex; it says nothing about whether the KIND is a real lane token, so a
    # row like `not-a-lane-token=sk-ant-api03` was accepted as coverage.
    # normalize_lane in scripts/capture_fixtures.sh is the authority for the
    # token set (it is what writes meta.lane), parsed out of that script rather
    # than restated here -- a hand-copied list would be a third replica.
    local rig
    rig="$(dirname "$SCRUB")/capture_fixtures.sh"
    local lane_tokens
    lane_tokens="$(sed -n "/^normalize_lane()/,/^}/p" "$rig" \
        | grep -oE "printf '[a-z-]+" | sed "s/printf '//" | sort -u)"
    if [ "$(printf '%s\n' "$lane_tokens" | grep -c .)" -lt 5 ]; then
        echo "FAIL: parsed only $(printf '%s\n' "$lane_tokens" | grep -c .) lane tokens from normalize_lane; the parse broke"
        fails=$((fails + 1))
    fi
    local bad_kind=""
    while IFS= read -r k; do
        [ -n "$k" ] || continue
        printf '%s\n' "$lane_tokens" | grep -qx -- "$k" || bad_kind="$bad_kind $k"
    done <<KINDS
$(printf '%s\n' "$block" | grep -E '^  "[a-z0-9-]+=' | sed 's/^  "//; s/=.*//')
$(printf '%s\n' "$block" | grep -E '^  "[a-z0-9-]+"$' | sed 's/^  "//; s/"$//')
KINDS
    if [ -n "$bad_kind" ]; then
        echo "FAIL: shape-table names kinds that are not normalize_lane lane tokens --$bad_kind"
        fails=$((fails + 1))
    else
        echo "PASS: every kind in the shape-coverage table is a real lane token"
    fi

    if [ "$row_count" -lt 4 ]; then
        echo "FAIL: the shape-coverage table parsed only $row_count rows; the kind vocabulary has more"
        fails=$((fails + 1))
    elif [ -n "$missing" ]; then
        echo "FAIL: shape-table rows name rule ids no regex in the gate carries --$missing"
        fails=$((fails + 1))
    else
        echo "PASS: every rule id in the shape-coverage table appears verbatim in a gate regex"
    fi
}
assert_shape_table_rule_ids_exist

# --- auth-header -----------------------------------------------------
# The header files are the load-bearing surface: a capture against an
# OAuth lane records a live token there.
HEADERS_LIVE='[["user-agent","claude-cli/2.1.167 (external, cli)"],["authorization","Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.AAAA.BBBB"],["content-type","application/json"]]'
HEADERS_APIKEY='[["x-api-key","sk-ant-api03-QQQQQQQQQQQQQQQQQQQQQQQQ"],["anthropic-version","2023-06-01"]]'
HEADERS_REDACTED='[["user-agent","claude-cli/2.1.167 (external, cli)"],["authorization","Bearer [REDACTED]"],["content-type","application/json"]]'

assert_caught "a live authorization value in a headers file" \
    ingress_request.headers.json "$HEADERS_LIVE" auth-header

assert_caught "a live x-api-key value in a headers file" \
    ingress_request.headers.json "$HEADERS_APIKEY" auth-header

assert_clean "a headers file whose credential values are already redacted" \
    ingress_request.headers.json "$HEADERS_REDACTED"

# The three names this replica drifted from its Rust counterpart on. All
# three are in REDACT_HEADER_NAMES in crates/routectl-core/src/log_safe.rs,
# and the egress (dir-4) trace performs no in-process redaction, so a
# `set-cookie` reaches egress_response.headers.json verbatim. A session
# cookie is a replayable credential.
assert_caught "a set-cookie session credential in a headers file" \
    egress_response.headers.json \
    '[["set-cookie","__Secure-session=abcdefghijklmnopqrstuvwxyz012345; HttpOnly"]]' \
    auth-header

assert_caught "a cookie echoed back on a request" \
    ingress_request.headers.json \
    '[["cookie","sessionid=abcdefghijklmnopqrstuvwxyz012345"]]' \
    auth-header

assert_caught "the mitm seam nonce in a headers file" \
    ingress_request.headers.json \
    '[["x-routectl-mitm-proxied","d41d8cd98f00b204e9800998ecf8427e"]]' \
    auth-header

# Paired accept direction for the three names above: the redaction
# placeholder under the same names must pass, or a scrubbed fixture is
# permanently unpromotable.
assert_clean "redacted cookie headers are accepted" \
    egress_response.headers.json \
    '[["set-cookie","[REDACTED]"],["cookie","[REDACTED]"],["x-routectl-mitm-proxied","[REDACTED]"]]'

# Operational headers this repo promises stay visible must not read as
# credentials -- over-redaction destroys the wire shape the fixture pins.
assert_clean "operational quota and signing-metadata headers stay visible" \
    ingress_request.headers.json \
    '[["x-ratelimit-limit-tokens","40000"],["x-ratelimit-remaining-tokens","39000"],["x-amz-date","20260825T100000Z"],["anthropic-beta","prompt-caching-2024-07-31"]]'

# --- headers-unparseable ---------------------------------------------
# A headers file the gate cannot parse has auth content it cannot inspect;
# that is a refusal, not a pass.
assert_caught "an unparseable headers file is refused rather than skipped" \
    ingress_request.headers.json '[["authorization","Bearer x"' headers-unparseable

# --- write mode ------------------------------------------------------
# Auth redaction keeps the header NAME and replaces only the VALUE: a
# deletion would destroy the wire shape the fixture exists to pin.
assert_write "a live bearer token does not survive into the written fixture" \
    ingress_request.headers.json "$HEADERS_LIVE" \
    '! printf "%s" "$WRITTEN" | grep -q "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9"'

assert_write "the authorization header NAME survives redaction" \
    ingress_request.headers.json "$HEADERS_LIVE" \
    'printf "%s" "$WRITTEN" | grep -q "\"authorization\""'

assert_write "the bearer scheme survives as a placeholder" \
    ingress_request.headers.json "$HEADERS_LIVE" \
    'printf "%s" "$WRITTEN" | grep -qF "Bearer [REDACTED]"'

assert_write "a raw api key collapses to the bare placeholder" \
    ingress_request.headers.json "$HEADERS_APIKEY" \
    '! printf "%s" "$WRITTEN" | grep -q "sk-ant-api03"'

assert_write "a non-credential header value survives redaction untouched" \
    ingress_request.headers.json "$HEADERS_LIVE" \
    'printf "%s" "$WRITTEN" | grep -qF "claude-cli/2.1.167"'

# --write must redact the three drifted names too, not merely refuse them:
# a session cookie reaching a promoted fixture is the failure this closes.
assert_write "a set-cookie session credential does not survive the write pass" \
    egress_response.headers.json \
    '[["set-cookie","__Secure-session=abcdefghijklmnopqrstuvwxyz012345; HttpOnly"]]' \
    '! printf "%s" "$WRITTEN" | grep -q "abcdefghijklmnopqrstuvwxyz012345"'

assert_write "the set-cookie header NAME survives redaction" \
    egress_response.headers.json \
    '[["set-cookie","__Secure-session=abcdefghijklmnopqrstuvwxyz012345; HttpOnly"]]' \
    'printf "%s" "$WRITTEN" | grep -q "\"set-cookie\""'

assert_write "the mitm seam nonce does not survive the write pass" \
    ingress_request.headers.json \
    '[["x-routectl-mitm-proxied","d41d8cd98f00b204e9800998ecf8427e"]]' \
    '! printf "%s" "$WRITTEN" | grep -q "d41d8cd98f00b204e9800998ecf8427e"'

# Behavior preservation: the HOME rewrite moved out of the capture rig,
# both forms.
assert_write "the plain home path rewrite still lands on the placeholder" \
    ingress_request.json \
    "$(body_with "cat @HOME@/.config/routectl/config.toml")" \
    'printf "%s" "$WRITTEN" | grep -qF -- "/home/user/.config/routectl/config.toml"'

assert_write "the plain home path does not survive the rewrite" \
    ingress_request.json \
    "$(body_with "cat @HOME@/.config/routectl/config.toml")" \
    '! printf "%s" "$WRITTEN" | grep -qF -- "$FAKEHOME"'

assert_write "the dash-encoded home path rewrite still lands on the placeholder" \
    ingress_request.json \
    "$(body_with "session at @HOMEENC@-Desktop-build-routectl")" \
    'printf "%s" "$WRITTEN" | grep -qF -- "-home-user-Desktop-build-routectl"'

assert_write "the dash-encoded home path does not survive the rewrite" \
    ingress_request.json \
    "$(body_with "session at @HOMEENC@-Desktop-build-routectl")" \
    '! printf "%s" "$WRITTEN" | grep -qF -- "$FAKEHOMEENC"'

# --write followed by --check is the landing path a capture pipeline runs:
# what the writer scrubbed, the checker must accept.
run_write_then_check() {
    local filename="$1" content="$2"
    local work fake_home rc
    work="$(mktemp -d)"
    fake_home="$work/home/$FAKE_HOME_NAME"
    mkdir -p "$fake_home" "$work/repo/fixture" "$work/stubbin"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$FAKE_HOSTNAME" >"$work/stubbin/hostname"
    chmod +x "$work/stubbin/hostname"
    expand_home_tokens "$content" "$fake_home" >"$work/repo/fixture/$filename"
    (
        cd "$work/repo" || exit 2
        git init -q .
        git config user.name "$FAKE_GIT_NAME"
        git config user.email "$FAKE_GIT_EMAIL"
        export HOME="$fake_home" PATH="$work/stubbin:$PATH"
        bash "$SCRUB" --write fixture && bash "$SCRUB" --check fixture
    ) >"$work/scrub.log" 2>&1
    rc=$?
    if [ "$rc" = "0" ]; then
        echo "PASS: write then check -- $filename"
    else
        echo "FAIL: a scrubbed fixture is still refused by --check ($filename, exit $rc)"
        cat "$work/scrub.log"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}

run_write_then_check ingress_request.headers.json "$HEADERS_LIVE"
run_write_then_check ingress_request.json \
    "$(body_with "cat @HOME@/.config/routectl/config.toml")"

# --- interface and fail-closed behavior ------------------------------
assert_usage() {
    local desc="$1"
    shift
    local rc=0
    # Captured directly from the command, NOT from an enclosing `if`: `$?`
    # after an `if` block is the `if`'s own status and would read 0 on
    # every case, passing the assertion against a script that never
    # validated its arguments at all.
    bash "$SCRUB" "$@" >/dev/null 2>&1 || rc=$?
    if [ "$rc" = "2" ]; then
        echo "PASS: usage error -- $desc"
    else
        echo "FAIL: expected exit 2 but got $rc -- $desc"
        fails=$((fails + 1))
    fi
}

assert_usage "no mode is a usage error, not a vacuous pass" "$SCRUB"
assert_usage "no path is a usage error" --check
assert_usage "a nonexistent path is a usage error, not an empty clean scan" \
    --check /nonexistent/routectl-fixture
assert_usage "the two modes are mutually exclusive" --check --write "$SCRUB"
assert_usage "an unknown option is a usage error" --check --nope "$SCRUB"

# `--help` renders the header block, and the class list in it must name
# every class the gate can report. The sentinel must not leak into the
# rendered output -- a magic line-count range silently started cutting
# content once already, which is why the sentinel exists.
assert_help_lists_new_classes() {
    local out rc=0 class missing=""
    out="$(bash "$SCRUB" --help 2>&1)" || rc=$?
    if [ "$rc" != "0" ]; then
        echo "FAIL: --help exited $rc"
        fails=$((fails + 1))
        return
    fi
    if printf '%s\n' "$out" | grep -qF -- "END USAGE"; then
        echo "FAIL: --help leaked the END USAGE sentinel into its output"
        fails=$((fails + 1))
        return
    fi
    for class in google-oauth-token google-api-key jwt aws-temp-key-id nvidia-api-key seat-session-id; do
        printf '%s\n' "$out" | grep -qF -- "$class" || missing+=" $class"
    done
    if [ -n "$missing" ]; then
        echo "FAIL: the --help class list omits --$missing"
        fails=$((fails + 1))
    else
        echo "PASS: --help names every new deny class and leaks no sentinel"
    fi
}
assert_help_lists_new_classes

# --- the --lane-known query mode --------------------------------------
# Exit-code contract, driven through the REAL script. The rig's driver-mode
# landing gate reads nothing but this status, so each of the three states
# is pinned by its own case, including the EXCLUDED direction: a lane
# classified as having no prefix-detectable shape answers 0, and only a
# lane nobody has classified answers 1.
assert_lane_known() {
    local desc="$1" expected="$2"
    shift 2
    local rc=0
    # Captured directly from the command, not from an enclosing `if`, for
    # the reason assert_usage documents.
    bash "$SCRUB" --lane-known "$@" >/dev/null 2>&1 || rc=$?
    if [ "$rc" = "$expected" ]; then
        echo "PASS: --lane-known exits $expected -- $desc"
    else
        echo "FAIL: --lane-known expected exit $expected but got $rc -- $desc"
        fails=$((fails + 1))
    fi
}

assert_lane_known "a lane with a prefix-detectable shape is classified" 0 anthropic-api
assert_lane_known "a second table lane is classified" 0 gemini
# bedrock is in PROVIDER_SHAPE_EXCLUDED: no prefix shape, reason recorded
# in the table. That is a verdict, so it is CLASSIFIED, not unknown.
assert_lane_known "an explicitly excluded lane is classified, not unknown" 0 bedrock
# The fail-closed state the whole mode exists for.
assert_lane_known "a lane absent from both lists is unclassified" 1 not-a-lane
assert_lane_known "an empty lane value is a usage error, not an answer" 2 ""

# Usage errors that would otherwise read as a table verdict.
assert_usage "--lane-known with no value is a usage error" --lane-known
assert_usage "--lane-known and --check are mutually exclusive" \
    --lane-known gemini --check "$SCRUB"
assert_usage "--check and --lane-known are mutually exclusive in either order" \
    --check "$SCRUB" --lane-known gemini
assert_usage "--lane-known and --write are mutually exclusive" \
    --lane-known gemini --write "$SCRUB"
assert_usage "--lane-known takes no path argument" --lane-known gemini "$SCRUB"

# The mode is a TABLE query: it must not scan, and must not depend on a
# readable fixture or on the environment-derived deny set. A path that
# does not exist is fatal to `--check`; `--lane-known` never looks at one,
# and the WARN lines the deny-set derivation emits on an unconfigured box
# would be noise from a mode that scans nothing.
assert_lane_known_performs_no_scan() {
    local out rc=0
    out="$(cd / && HOME="" bash "$SCRUB" --lane-known anthropic-api 2>&1)" || rc=$?
    if [ "$rc" != "0" ]; then
        echo "FAIL: --lane-known exited $rc in an un-interrogable environment"
        printf '%s\n' "$out"
        fails=$((fails + 1))
        return
    fi
    if printf '%s\n' "$out" | grep -q "WARN deny class"; then
        echo "FAIL: --lane-known derived a deny set it never uses"
        printf '%s\n' "$out"
        fails=$((fails + 1))
    else
        echo "PASS: --lane-known answers from the table with no scan and no deny set"
    fi
}
assert_lane_known_performs_no_scan

# `--help` must document the mode: the rig depends on the exit contract,
# and a mode absent from the usage block is a mode nobody calls correctly.
assert_help_documents_lane_known() {
    local out rc=0
    out="$(bash "$SCRUB" --help 2>&1)" || rc=$?
    if [ "$rc" = "0" ] &&
        printf '%s\n' "$out" | grep -qF -- "--lane-known" &&
        ! printf '%s\n' "$out" | grep -qF -- "END USAGE"; then
        echo "PASS: --help documents --lane-known and leaks no sentinel"
    else
        echo "FAIL: --help omits --lane-known or leaks the sentinel (exit $rc)"
        fails=$((fails + 1))
    fi
}
assert_help_documents_lane_known

# An empty directory would scan zero files; that must refuse, not PASS.
assert_empty_dir_refused() {
    local work rc=0
    work="$(mktemp -d)"
    mkdir -p "$work/fixture"
    bash "$SCRUB" --check "$work/fixture" >/dev/null 2>&1 || rc=$?
    if [ "$rc" = "2" ]; then
        echo "PASS: an empty fixture directory is refused, not a vacuous PASS"
    else
        echo "FAIL: an empty fixture directory scanned nothing and exited $rc"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}
assert_empty_dir_refused

# An un-interrogable environment must narrow the deny set VISIBLY, never
# silently: no git identity configured means the two author classes drop,
# and the script must say so on stderr while still scanning the rest.
unconfigured_env_warns() {
    local work rc log
    work="$(mktemp -d)"
    mkdir -p "$work/home" "$work/plain/fixture"
    printf '%s' "$(body_with "ls /home/someoneelse/Desktop")" \
        >"$work/plain/fixture/ingress_request.json"
    (
        cd "$work/plain" || exit 2
        # No git repo and no global config under the fake HOME, so
        # `git config user.name` yields nothing.
        HOME="$work/home" bash "$SCRUB" --check fixture
    ) >"$work/scrub.log" 2>&1
    rc=$?
    log="$work/scrub.log"
    if grep -q "WARN deny class 'git-author-name' inactive" "$log" &&
        grep -q "WARN deny class 'git-author-email' inactive" "$log"; then
        echo "PASS: an unconfigured git identity warns rather than silently narrowing"
    else
        echo "FAIL: an unconfigured git identity narrowed the deny set silently"
        cat "$log"
        fails=$((fails + 1))
    fi
    # The structural classes stay active regardless of the missing identity.
    if [ "$rc" = "1" ] && grep -q "  home-prefix\$" "$log"; then
        echo "PASS: structural classes stay active when the identity is unknown"
    else
        echo "FAIL: a narrowed deny set stopped catching structural classes (exit $rc)"
        cat "$log"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}
unconfigured_env_warns

# The seat-session-id class derives its deny value from a file that is
# ABSENT on any box that has not logged in, and absent by construction
# under the hermetic XDG a capture run creates. Its absence must therefore
# be loud: a silent narrowing here is the class not existing on exactly the
# machines the capture happens on.
#
# `$1` a description, `$2` the credentials.json content to plant (empty
# plants no file at all), `$3` the WARN reason fragment expected.
assert_seat_warns() {
    local desc="$1" store="$2" needle="$3"
    local work log rc=0
    work="$(mktemp -d)"
    mkdir -p "$work/home" "$work/xdg/routectl" "$work/plain/fixture"
    [ -z "$store" ] || printf '%s' "$store" >"$work/xdg/routectl/credentials.json"
    printf '%s' "$(body_with "a clean body with no personal data in it")" \
        >"$work/plain/fixture/ingress_request.json"
    (
        cd "$work/plain" || exit 2
        HOME="$work/home" XDG_CONFIG_HOME="$work/xdg" bash "$SCRUB" --check fixture
    ) >"$work/scrub.log" 2>&1
    rc=$?
    log="$work/scrub.log"
    if [ "$rc" != "0" ]; then
        echo "FAIL: the surrounding scan was not clean (exit $rc) -- $desc"
        cat "$log"
        fails=$((fails + 1))
    elif grep -F "WARN deny class 'seat-session-id' inactive" "$log" | grep -qF -- "$needle"; then
        echo "PASS: seat-session-id warns rather than silently dropping -- $desc"
    else
        echo "FAIL: seat-session-id was dropped silently -- $desc"
        cat "$log"
        fails=$((fails + 1))
    fi
    rm -rf "$work"
}

assert_seat_warns "no seat store at all, the hermetic-XDG state a capture runs in" \
    "" \
    "no readable seat store"

assert_seat_warns "a seat store holding a record with no session id" \
    '{"schema_version":1,"providers":{"anthropic":{"access_token":"placeholder","refresh_token":"placeholder","token_type":"Bearer","expires_at_unix":4102444800,"obtained_at_unix":1756252800}}}' \
    "holds no usable session id"

assert_seat_warns "a seat store that is not the expected JSON document" \
    'not json at all' \
    "not readable as the expected JSON document"

# The neutral / too-short discipline the other derived classes follow: a
# degenerate value is skipped with a warning rather than deny-listed,
# because deny-listing it would refuse legitimate placeholder content.
assert_seat_warns "a stored session id shorter than the literal floor" \
    "$(seat_store_with "ab")" \
    "shorter than"

assert_seat_warns "a stored session id in the neutral set" \
    "$(seat_store_with "00000000-0000-0000-0000-000000000000")" \
    "in the neutral set"

# The paired positive control for the two skips above: the SAME degenerate
# values must be accepted in a body. Without this the skip assertions prove
# only that a WARN printed, not that the value stayed out of the deny set.
assert_clean "a nil-uuid session id in a body is accepted when the seat stores it" \
    ingress_request.json \
    "$(user_id_body_with "00000000-0000-0000-0000-000000000000")" \
    "$(seat_store_with "00000000-0000-0000-0000-000000000000")"

assert_clean "a two-char stored session id does not deny-list that fragment" \
    ingress_request.json \
    "$(body_with "the abbreviation ab appears in ordinary prose")" \
    "$(seat_store_with "ab")"

if [ "$fails" -gt 0 ]; then
    echo "scrub-fixture self-test: $fails failure(s)" >&2
    exit 1
fi
echo "scrub-fixture self-test: all assertions passed"
