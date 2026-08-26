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
# dir, the git identity is set in the repo's own config, and `hostname`
# resolves to a stub. `PATH` keeps the real tools the script needs.
run_scrub() {
    local filename="$1" content="$2" mode="$3"
    local work
    work="$(mktemp -d)"
    local fake_home="$work/home/$FAKE_HOME_NAME"
    mkdir -p "$fake_home" "$work/repo/fixture" "$work/stubbin"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$FAKE_HOSTNAME" >"$work/stubbin/hostname"
    chmod +x "$work/stubbin/hostname"
    expand_home_tokens "$content" "$fake_home" >"$work/repo/fixture/$filename"
    (
        cd "$work/repo" || exit 2
        git init -q .
        git config user.name "$FAKE_GIT_NAME"
        git config user.email "$FAKE_GIT_EMAIL"
        HOME="$fake_home" PATH="$work/stubbin:$PATH" \
            bash "$SCRUB" "$mode" fixture
    ) >"$work/scrub.log" 2>&1
    printf '%s\t%s\n' "$?" "$work"
}

# `$1` description, `$2` fixture filename, `$3` content.
assert_caught() {
    local desc="$1" filename="$2" content="$3" expect_class="${4:-}"
    local out rc work
    out="$(run_scrub "$filename" "$content" --check)"
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
    local desc="$1" filename="$2" content="$3"
    local out rc work
    out="$(run_scrub "$filename" "$content" --check)"
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

if [ "$fails" -gt 0 ]; then
    echo "scrub-fixture self-test: $fails failure(s)" >&2
    exit 1
fi
echo "scrub-fixture self-test: all assertions passed"
