#!/usr/bin/env bash
# The ingress-dialect vocabulary, as one sourced declaration.
#
# Sourced, never executed. Three scripts need it -- the capture rig, the
# driver runner, and the promotion script -- and all three enforce the
# expected-ingress pin against it, so a per-script copy would be three
# spellings of one closed set. Same posture as
# scripts/drivers/lib/confine.sh: one owner, sourced by every caller,
# absent library is a hard failure rather than a skipped check.
#
# A REPLICA of what `IngressAdapter::id()` returns in
# crates/routectl-cli/src/ingress/ -- the same vocabulary the rig writes
# into `meta.ingress_kind` and every replay consumer dispatches on. It is
# a replica because the rig runs in throwaway trees that carry scripts/
# and no crates/; the self-tests derive the real set out of those `id()`
# bodies and assert the two agree, so a dialect added to the code without
# being added here is a red test rather than a pin nobody can spell.
#
# The sentinels bound the block a caller may parse as TEXT. Renaming or
# dropping one turns such a parse into a loud failure rather than a
# silently empty vocabulary.
# --- BEGIN INGRESS_KINDS ---
INGRESS_KINDS=(
  "anthropic"
  "openai"
  "openai-responses"
)
# --- END INGRESS_KINDS ---

# Is `$1` a member of the vocabulary? 0 yes, 1 no.
#
# The EMPTY string is deliberately NOT a member. Empty means "the capture
# could not observe the dialect" everywhere else in the fixture schema,
# and a pin is a statement about what the run expects -- so an empty pin
# is the empty claim the mandatory-pin rule exists to refuse, not a
# wildcard that matches an unobserved capture.
ingress_kind_is_known() {
  local candidate="$1" known
  for known in "${INGRESS_KINDS[@]}"; do
    [ "$candidate" = "$known" ] && return 0
  done
  return 1
}

# The vocabulary as a comma-separated list, for a refusal message that
# tells the caller what it could have said instead.
ingress_kinds_list() {
  local IFS=,
  printf '%s\n' "${INGRESS_KINDS[*]}"
}
