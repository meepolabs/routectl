#!/usr/bin/env bash
# Commit-subject length gate: rejects a commit message whose SUBJECT
# (line 1 only -- the body wraps at a different width by convention and
# is not bounded by this rule) exceeds SUBJECT_LIMIT characters.
#
# Character count, not byte count: the repo is ASCII-only by rule, where
# the two agree, but `${#subject}` is the character count in bash and
# that is the more literal reading of "N characters" if the two ever
# diverge (e.g. a stray non-ASCII byte slipping in before the ASCII-only
# gate catches it elsewhere).
#
# This is a real gate: an over-limit subject exits non-zero and blocks
# the commit. It runs from the commit-msg stage of
# .pre-commit-config.yaml, which appends the message-file path.
#
# Usage: check-subject-length.sh COMMIT_MSG_FILE

set -euo pipefail

# "Subject under 70 characters" (project style rule) is read here as
# <= 70: the more permissive of the two readings the wording admits, so
# a subject sitting exactly at the boundary is not punished for an
# ambiguity in the prose. The error message below states this bound
# explicitly so the enforcement is never itself ambiguous.
readonly SUBJECT_LIMIT=70

usage() {
    echo "usage: $0 COMMIT_MSG_FILE" >&2
    exit 2
}

# Print the effective subject line of a commit-message file, or nothing
# if the file has no subject to check (empty message -- git's own
# aborted-empty-commit check handles that case; a first line that is
# itself a comment, which happens when the whole message is still the
# unedited comment-only template).
subject_of() {
    local msg_file="$1" first_line
    first_line="$(head -n1 "$msg_file" 2>/dev/null || true)"
    [[ -z "$first_line" ]] && return 0
    [[ "$first_line" == "#"* ]] && return 0
    printf '%s' "$first_line"
}

main() {
    [[ $# -ge 1 ]] || usage
    local msg_file="$1" subject len
    subject="$(subject_of "$msg_file")"
    [[ -z "$subject" ]] && exit 0
    len=${#subject}
    if (( len > SUBJECT_LIMIT )); then
        echo "check-subject-length: commit subject is $len characters, limit is $SUBJECT_LIMIT (<=$SUBJECT_LIMIT allowed):" >&2
        echo "  $subject" >&2
        exit 1
    fi
    exit 0
}

main "$@"
