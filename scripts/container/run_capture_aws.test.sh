# shellcheck shell=bash
# The bedrock provider's AWS profile legs of scripts/container/run_capture.test.sh.
# Sourced by that suite (never run alone): it uses its check helpers, $WORK,
# and the argv-recording docker stand-in, which also copies every file
# mounted under /etc/routectl-aws out to $FAKE_DOCKER_MOUNT_DUMP while the
# wrapper's staged copy still exists.
#
# NO REAL AWS FILE IS READ. The fixture is a synthetic AWS directory under
# $WORK: a config holding the profile under test, a [default] and a second
# profile that must never cross, a shared-credentials file that must never
# cross, and a credential_process target with obviously fake values. Every
# planted value is a marker that must appear in no mounted byte and in no
# line the wrapper prints.

AWS_FIX="$WORK/aws-fixture/.aws"
mkdir -p "$AWS_FIX"
chmod 0700 "$WORK/aws-fixture" "$AWS_FIX"
AWS_FIX_PROFILE="synthetic-cell-profile"
AWS_OTHER_MARKER="PLANTED-other-profile-value-must-not-cross"
AWS_DEFAULT_MARKER="PLANTED-default-profile-value-must-not-cross"
AWS_SHARED_MARKER="PLANTED-shared-credentials-value-must-not-cross"
AWS_LINE_MARKER="PLANTED-line-text-must-not-be-echoed"
AWS_CRED="$AWS_FIX/cell-credential.json"
AWS_OTHER_CRED="$AWS_FIX/other-credential.json"

write_aws_credential() {
    printf '{"Version": 1, "AccessKeyId": "%s", "SecretAccessKey": "%s", "SessionToken": "%s"}\n' \
        "fake-access-$2" "fake-secret-$2" "fake-session-$2" >"$1"
    chmod 0600 "$1"
}
write_aws_credential "$AWS_CRED" "cell-${FAKE_RUN}"
write_aws_credential "$AWS_OTHER_CRED" "$AWS_OTHER_MARKER"
printf '[default]\naws_access_key_id = %s\n' "$AWS_SHARED_MARKER" >"$AWS_FIX/credentials"
chmod 0600 "$AWS_FIX/credentials"

# The profile section under test, in the shape an operator writes it.
aws_profile_section() {
    printf '[profile %s]\ncredential_process = /bin/cat %s\nregion = us-west-2\n' \
        "$AWS_FIX_PROFILE" "$AWS_CRED"
}

# A realistic operator config: the profile under test between a [default]
# and another profile, each with values that must never cross.
aws_good_config() {
    printf '[default]\nregion = us-east-1\ncredential_process = /bin/cat %s\noutput = %s\n\n' \
        "$AWS_OTHER_CRED" "$AWS_DEFAULT_MARKER"
    aws_profile_section
    printf '\n[profile other-synthetic]\ncredential_process = /bin/cat %s\noutput = %s\n' \
        "$AWS_OTHER_CRED" "$AWS_OTHER_MARKER"
}

AWS_GOOD_CONFIG="$AWS_FIX/config"
aws_good_config >"$AWS_GOOD_CONFIG"

# The exact bytes the cell must receive: the placeholder profile the lane
# configs name, the credential_process rewritten to the in-cell path, and
# the region. Nothing else.
AWS_EXPECTED_CELL_CONFIG="$(printf '[profile routectl-cell]\ncredential_process = /bin/cat /etc/routectl-aws/credential\nregion = us-west-2')"

AWS_RUNNER_ARGS=(--lane bedrock-invoke --case plain-turn-01 --expected-ingress anthropic)

# Run the wrapper for bedrock against the docker stand-in, dumping what the
# AWS mounts carried into <root>.mounts/.
#
# Usage: aws_run <scratch-root> <config> <credential-file> [profile]
aws_run() {
    local root="$1" config="$2" cred="$3" profile="${4:-$AWS_FIX_PROFILE}"
    mkdir -p "$root.mounts"
    fake_docker_run "$root" "FAKE_DOCKER_MOUNT_DUMP=$root.mounts" -- \
        --scratch "$root/land" --provider bedrock \
        --aws-profile "$profile" --aws-config "$config" --aws-credential-file "$cred" -- \
        "${AWS_RUNNER_ARGS[@]}" -- "/scratch/$ASSETS_REL/driver.sh"
}

# The `-v` source of every mount whose target is under /etc/routectl-aws.
aws_mount_sources() {
    awk 'prev == "-v" && $0 ~ /:\/etc\/routectl-aws\// { sub(/:\/etc\/routectl-aws\/.*/, ""); print } { prev = $0 }' \
        "$1.argv" 2>/dev/null
}

AWS_LEAK_NEEDLES=("$AWS_OTHER_MARKER" "$AWS_DEFAULT_MARKER" "$AWS_SHARED_MARKER"
    "$AWS_LINE_MARKER" "fake-secret-" "other-synthetic")

# How many planted markers, or lines of the credential, reached <file>.
aws_leak_count() {
    local n=0 needle
    for needle in "${AWS_LEAK_NEEDLES[@]}"; do
        grep -qF -- "$needle" "$1" 2>/dev/null && n=$((n + 1))
    done
    echo "$n"
}

# Positive control: every "0 leaks" below is evidence only if the helper
# counts each needle when it IS present, one needle per file so a needle the
# helper dropped cannot hide behind another one's hit.
aws_leak_probe="$WORK/aws-fixture/leak-probe.txt"
for needle in "${AWS_LEAK_NEEDLES[@]}"; do
    printf 'prefix %s suffix\n' "$needle" >"$aws_leak_probe"
    check "aws_leak_count detects a planted '$needle'" "1" "$(aws_leak_count "$aws_leak_probe")"
done
printf '%s\n' "${AWS_LEAK_NEEDLES[@]}" >"$aws_leak_probe"
check "aws_leak_count counts every planted needle at once" "${#AWS_LEAK_NEEDLES[@]}" \
    "$(aws_leak_count "$aws_leak_probe")"
write_aws_credential "$aws_leak_probe" "probe"
check "aws_leak_count detects a credential file's content" "1" "$(aws_leak_count "$aws_leak_probe")"
rm -f "$aws_leak_probe"

# --- the accept leg: one profile, two read-only files ----------------------
AWS_ROOT="$(new_scratch)"
rc=0
aws_run "$AWS_ROOT" "$AWS_GOOD_CONFIG" "$AWS_CRED" || rc=$?
check "--provider bedrock with the three AWS profile inputs reaches docker run" "0" "$rc"
check "the cell config is exactly the placeholder profile, rewritten path and region" \
    "$AWS_EXPECTED_CELL_CONFIG" "$(cat "$AWS_ROOT.mounts/config" 2>/dev/null)"
check "the mounted credential is the credential_process target, byte for byte" "yes" \
    "$(cmp -s "$AWS_CRED" "$AWS_ROOT.mounts/credential" && echo yes || echo no)"
check "the cell config carries no other profile, default value or shared credential" "0" \
    "$(aws_leak_count "$AWS_ROOT.mounts/config")"
check "exactly two mounts target /etc/routectl-aws" "2" \
    "$(aws_mount_sources "$AWS_ROOT" | wc -l | tr -d ' ')"
check "both AWS mounts are read-only" "2" \
    "$(grep -cE '^.*:/etc/routectl-aws/(config|credential):ro$' "$AWS_ROOT.argv" || true)"
AWS_CRED_REAL="$(realpath "$AWS_CRED")"
check "the credential mount's source is the credential file itself" "yes" \
    "$(aws_mount_sources "$AWS_ROOT" | grep -qxF -- "$AWS_CRED_REAL" && echo yes || echo no)"
# The operator's AWS directory never crosses: no mount source is that
# directory, the operator's config, its shared-credentials file, or any
# other file in it. The one file allowed out of it is the credential.
aws_foreign_sources=0
while IFS= read -r src; do
    [ "$src" = "$AWS_CRED_REAL" ] && continue
    case "$src" in
        "$AWS_FIX"|"$AWS_FIX"/*|"$(dirname "$AWS_FIX")"|"$(dirname "$AWS_FIX")"/*)
            aws_foreign_sources=$((aws_foreign_sources + 1)) ;;
    esac
done < <(awk 'prev == "-v" { print } { prev = $0 }' "$AWS_ROOT.argv" | sed 's/:.*//')
check "no mount source is the operator's AWS directory or another file in it" "0" "$aws_foreign_sources"
check "the generated config is staged outside the writable scratch mount" "0" \
    "$(aws_mount_sources "$AWS_ROOT" | grep -cE "^$AWS_ROOT/land(/|\$)" || true)"
check "the generated config is staged beside the scratch mount" "1" \
    "$(aws_mount_sources "$AWS_ROOT" | grep -cE "^$AWS_ROOT/land\.aws\.[A-Za-z0-9]+/config\$" || true)"
check "the staged AWS config is removed when the wrapper exits" "0" \
    "$(find "$WORK" -maxdepth 2 -name 'land.aws.*' | wc -l | tr -d ' ')"
check "the runner is told the in-cell config path" "yes" \
    "$(argv_has_pair "$AWS_ROOT" --aws-config /etc/routectl-aws/config)"
check "no AWS variable is set on the docker command line" "0" \
    "$(grep -cE '^AWS_' "$AWS_ROOT.argv" || true)"
check "the wrapper prints no planted value or credential line" "0" \
    "$(aws_leak_count "$AWS_ROOT.log")"

# A bedrock key in the operator's environment is never forwarded: bedrock
# signs with the profile, so the key-resolution loop must skip it rather
# than pass the variable through by name.
AWS_ROOT="$(new_scratch)"
mkdir -p "$AWS_ROOT.mounts"
rc=0
fake_docker_run "$AWS_ROOT" "ROUTECTL_DRIVER_BEDROCK_API_KEY=$FAKE_BEDROCK_KEY" \
    "FAKE_DOCKER_MOUNT_DUMP=$AWS_ROOT.mounts" -- \
    --scratch "$AWS_ROOT/land" --provider bedrock \
    --aws-profile "$AWS_FIX_PROFILE" --aws-config "$AWS_GOOD_CONFIG" --aws-credential-file "$AWS_CRED" -- \
    "${AWS_RUNNER_ARGS[@]}" -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "a bedrock run with a bedrock key in the environment reaches docker run" "0" "$rc"
check "a bedrock key in the environment is not forwarded by name" "0" \
    "$(grep -cx -- 'ROUTECTL_DRIVER_BEDROCK_API_KEY' "$AWS_ROOT.argv" || true)"
check "a bedrock key in the environment reaches no docker argv word" "0" \
    "$(grep -cF -- "$FAKE_BEDROCK_KEY" "$AWS_ROOT.argv" || true)"

# A profile with no region gets none in the cell config.
AWS_NOREGION_CONFIG="$AWS_FIX/config-noregion"
printf '[profile %s]\ncredential_process = /bin/cat %s\n' "$AWS_FIX_PROFILE" "$AWS_CRED" >"$AWS_NOREGION_CONFIG"
AWS_ROOT="$(new_scratch)"
rc=0
aws_run "$AWS_ROOT" "$AWS_NOREGION_CONFIG" "$AWS_CRED" || rc=$?
check "a profile without a region is accepted" "0" "$rc"
check "a profile without a region yields a config without one" \
    "$(printf '[profile routectl-cell]\ncredential_process = /bin/cat /etc/routectl-aws/credential')" \
    "$(cat "$AWS_ROOT.mounts/config" 2>/dev/null)"

# --- the inputs are required, and only for bedrock -------------------------
AWS_ROOT="$(new_scratch)"
rc=0
fake_docker_run "$AWS_ROOT" "ROUTECTL_DRIVER_BEDROCK_API_KEY=$FAKE_BEDROCK_KEY" -- \
    --scratch "$AWS_ROOT/land" --provider bedrock -- \
    "${AWS_RUNNER_ARGS[@]}" -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "--provider bedrock without AWS profile inputs is refused with exit 20, key or not" "20" "$rc"
check_refusal "the missing-profile refusal names the flags" "pass --aws-profile, --aws-config and --aws-credential-file" "$AWS_ROOT.log"
check "the missing-profile refusal reached no docker run" "no" \
    "$([ -e "$AWS_ROOT.argv" ] && echo yes || echo no)"

for aws_drop in profile config credential-file; do
    AWS_ROOT="$(new_scratch)"
    aws_partial=(--aws-profile "$AWS_FIX_PROFILE" --aws-config "$AWS_GOOD_CONFIG" --aws-credential-file "$AWS_CRED")
    case "$aws_drop" in
        profile) aws_partial=("${aws_partial[@]:2}") ;;
        config) aws_partial=("${aws_partial[@]:0:2}" "${aws_partial[@]:4}") ;;
        credential-file) aws_partial=("${aws_partial[@]:0:4}") ;;
    esac
    rc=0
    fake_docker_run "$AWS_ROOT" -- --scratch "$AWS_ROOT/land" --provider bedrock "${aws_partial[@]}" -- \
        "${AWS_RUNNER_ARGS[@]}" -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
    check "--provider bedrock without --aws-$aws_drop is refused with exit 20" "20" "$rc"
done

AWS_ROOT="$(new_scratch)"
rc=0
fake_docker_run "$AWS_ROOT" -- --scratch "$AWS_ROOT/land" \
    --aws-profile "$AWS_FIX_PROFILE" --aws-config "$AWS_GOOD_CONFIG" --aws-credential-file "$AWS_CRED" -- \
    --lane anthropic-api --case plain-turn-01 --expected-ingress anthropic \
    -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "AWS profile inputs on a run without bedrock are a usage error" "2" "$rc"
check "AWS profile inputs on a run without bedrock mount nothing" "no" \
    "$([ -e "$AWS_ROOT.argv" ] && echo yes || echo no)"

AWS_ROOT="$(new_scratch)"
rc=0
fake_docker_run "$AWS_ROOT" -- --scratch "$AWS_ROOT/land" --provider bedrock \
    --aws-profile "$AWS_FIX_PROFILE" --aws-config "$AWS_GOOD_CONFIG" --aws-credential-file "$AWS_CRED" -- \
    "${AWS_RUNNER_ARGS[@]}" --aws-config /workspace/elsewhere -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "a caller's own --aws-config for the runner is refused" "2" "$rc"

# --- the strict reader: every ambiguity and foreign shape fails closed -----
# Each row is one config the reader must refuse, the problem its one stderr
# line must name, and (by construction) a planted marker on the offending
# line that must never be echoed. A refusal is exit 20 so the matrix runner
# records the cell NOT_RUN(credential).
aws_refusal() {
    local label="$1" needle="$2" body="$3" cred="${4:-$AWS_CRED}" profile="${5:-$AWS_FIX_PROFILE}"
    local cfg root rc=0
    cfg="$WORK/aws-fixture/refusal-$RANDOM.cfg"
    printf '%s\n' "$body" >"$cfg"
    root="$(new_scratch)"
    aws_run "$root" "$cfg" "$cred" "$profile" || rc=$?
    check "the AWS reader refuses $label with exit 20" "20" "$rc"
    check "the $label refusal names its problem" "yes" \
        "$(grep -qF -- "$needle" "$root.log" && echo yes || echo no)"
    check "the $label refusal echoes no line or value of the config" "0" "$(aws_leak_count "$root.log")"
    check "the $label refusal reached no docker run" "no" \
        "$([ -e "$root.argv" ] && echo yes || echo no)"
}

AWS_SECTION="$(aws_profile_section)"
aws_refusal "a missing profile" "no such profile in the config" \
    "$(printf '[profile other-synthetic]\ncredential_process = /bin/cat %s\noutput = %s' "$AWS_OTHER_CRED" "$AWS_OTHER_MARKER")"
aws_refusal "a repeated profile header" "repeats a section header" \
    "$(printf '%s\n%s\noutput = %s' "$AWS_SECTION" "$AWS_SECTION" "$AWS_LINE_MARKER")"
aws_refusal "a profile header with extra whitespace" "profile header is ambiguous" \
    "$(printf '[profile  %s]\ncredential_process = /bin/cat %s' "$AWS_FIX_PROFILE" "$AWS_CRED")"
aws_refusal "a second header normalizing to the profile" "profile header is ambiguous" \
    "$(printf '%s\n[profile  %s]\noutput = %s' "$AWS_SECTION" "$AWS_FIX_PROFILE" "$AWS_LINE_MARKER")"
aws_refusal "a bare section of the profile's name" "bare section of that name" \
    "$(printf '%s\n[%s]\noutput = %s' "$AWS_SECTION" "$AWS_FIX_PROFILE" "$AWS_LINE_MARKER")"
aws_refusal "a repeated key" "repeats a key" \
    "$(printf '%s\nregion = %s' "$AWS_SECTION" "$AWS_LINE_MARKER")"
aws_refusal "a line that is not a key = value pair" "not a key = value pair" \
    "$(printf '%s\n%s' "$AWS_SECTION" "$AWS_LINE_MARKER")"
aws_refusal "a line before the first header" "before its first section header" \
    "$(printf 'x = %s\n%s' "$AWS_LINE_MARKER" "$AWS_SECTION")"
aws_refusal "a key outside the allowlist" "key(s) this cell cannot honor" \
    "$(printf '%s\nrole_arn = %s' "$AWS_SECTION" "$AWS_LINE_MARKER")"
# A key NAME is config text too (a pasted token followed by `= x` parses as
# a key), so the refusal must not echo it. The bare-line case is the
# "not a key = value pair" row above.
aws_refusal "a planted text used as a key name" "key(s) this cell cannot honor" \
    "$(printf '%s\n%s = x' "$AWS_SECTION" "$AWS_LINE_MARKER")"
aws_refusal "a profile without credential_process" "has no credential_process" \
    "$(printf '[profile %s]\nregion = us-west-2' "$AWS_FIX_PROFILE")"
# configparser would hand a [DEFAULT] section's keys to every section; AWS
# does not, so the profile above must still read as having none.
aws_refusal "a credential_process only a [DEFAULT] section carries" "has no credential_process" \
    "$(printf '[DEFAULT]\ncredential_process = /bin/cat %s\n[profile %s]\nregion = us-west-2' "$AWS_CRED" "$AWS_FIX_PROFILE")"
aws_refusal "a credential_process that is not /bin/cat" "is not of the form '/bin/cat <absolute path>'" \
    "$(printf '[profile %s]\ncredential_process = /usr/bin/env %s' "$AWS_FIX_PROFILE" "$AWS_LINE_MARKER")"
aws_refusal "a credential_process with a second argument" "is not of the form '/bin/cat <absolute path>'" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s /%s' "$AWS_FIX_PROFILE" "$AWS_CRED" "$AWS_LINE_MARKER")"
aws_refusal "a credential_process with a shell separator" "is not of the form '/bin/cat <absolute path>'" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s;/%s' "$AWS_FIX_PROFILE" "$AWS_CRED" "$AWS_LINE_MARKER")"
aws_refusal "a credential_process continued onto a second line" "is not of the form '/bin/cat <absolute path>'" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s\n  /%s' "$AWS_FIX_PROFILE" "$AWS_CRED" "$AWS_LINE_MARKER")"
aws_refusal "a relative credential_process target" "is not of the form '/bin/cat <absolute path>'" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s' "$AWS_FIX_PROFILE" "$AWS_LINE_MARKER")"
aws_refusal "a credential_process reading another file" "reads a different file than --credential-file names" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s' "$AWS_FIX_PROFILE" "$AWS_OTHER_CRED")"
aws_refusal "a region that is not a region" "region is not an AWS region identifier" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s\nregion = %s' "$AWS_FIX_PROFILE" "$AWS_CRED" "$AWS_LINE_MARKER")"
aws_refusal "the default profile" "must match" "$AWS_SECTION" "$AWS_CRED" default

AWS_LOOSE_CRED="$AWS_FIX/loose-credential.json"
write_aws_credential "$AWS_LOOSE_CRED" "loose-${FAKE_RUN}"
chmod 0640 "$AWS_LOOSE_CRED"
aws_refusal "a credential file readable by group" "readable or writable by group or other" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s' "$AWS_FIX_PROFILE" "$AWS_LOOSE_CRED")" \
    "$AWS_LOOSE_CRED"

# A directory named as the credential file (an operator pointing at the AWS
# directory by mistake) is refused before anything is mounted.
AWS_DIR_CRED="$AWS_FIX/credential-dir"
mkdir -p "$AWS_DIR_CRED"
chmod 0700 "$AWS_DIR_CRED"
aws_refusal "a credential file that is a directory" "not a regular file" \
    "$(printf '[profile %s]\ncredential_process = /bin/cat %s' "$AWS_FIX_PROFILE" "$AWS_DIR_CRED")" \
    "$AWS_DIR_CRED"

# The staged config is removed by an EXIT trap; the runner's exit code must
# still come out of the wrapper untouched on a bedrock run.
AWS_ROOT="$(new_scratch)"
mkdir -p "$AWS_ROOT.mounts"
rc=0
fake_docker_run "$AWS_ROOT" "FAKE_DOCKER_RUN_EXIT=9" "FAKE_DOCKER_MOUNT_DUMP=$AWS_ROOT.mounts" -- \
    --scratch "$AWS_ROOT/land" --provider bedrock \
    --aws-profile "$AWS_FIX_PROFILE" --aws-config "$AWS_GOOD_CONFIG" --aws-credential-file "$AWS_CRED" -- \
    "${AWS_RUNNER_ARGS[@]}" -- "/scratch/$ASSETS_REL/driver.sh" || rc=$?
check "a runner exit 9 on a bedrock run comes out of the wrapper as 9" "9" "$rc"
check "the staged AWS config is removed after a nonzero runner exit too" "0" \
    "$(find "$AWS_ROOT" -maxdepth 1 -name 'land.aws.*' | wc -l | tr -d ' ')"
