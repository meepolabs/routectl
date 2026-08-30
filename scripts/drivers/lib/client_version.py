#!/usr/bin/env python3
"""Compare the two independent statements a fixture makes about its client version.

A fixture carries two: `meta.client.version`, parsed by the capture rig out
of the CLIENT-CONTROLLED ingress `user-agent`, and
`meta.client.binary_version`, read by the driver off the RUNNING binary
before any session opened. Until they were compared, the wire value was the
only one that reached a fixture and nothing could contradict it -- a client
whose binary and self-report disagree is not evidence about either, and the
disagreement is exactly what an auto-updater mid-run produces.

This module is the comparison, invoked at the two promotion boundaries
(`capture_fixtures.sh`'s `write_fixture` in driver mode and
`promote_fixture.sh` on the staged copy), the same shape the wire-pattern
predicate uses. A DISAGREEMENT verdict refuses the promotion.

# Why the comparison is on TOKENS and not on strings

The two sources spell one version differently by construction. A binary's
`--version` prints a human line -- `2.1.246 (Claude Code)`,
`codex-cli 0.151.0` -- while the rig extracts a bare `2.1.246` from
`claude-cli/2.1.246 (external, cli)`. A string equality would therefore
report every real capture as a disagreement, which is a gate that refuses
everything and proves nothing.

So both sides are reduced to their DOTTED-NUMERIC version tokens and the
verdict is whether the two token sets intersect. The reduction is applied
to both sides by the same code, so neither side's decoration can decide the
outcome, and a prerelease suffix (`2.1.246-beta`) reduces the same way in
both.

A side carrying NO token is NOT COMPARABLE (exit 3), never a disagreement:
a client whose version line is a word rather than a number has said nothing
this comparison can contradict, and refusing it would refuse the client
rather than the contradiction.

Usage:
  client_version.py --compare <binary-version> <wire-version>

Exit codes: 0 the two agree, 1 they DISAGREE (reason on stderr), 2 usage
error, 3 not comparable -- at least one side carries no version token.
"""

import re
import sys

# A version token: at least two dot-separated numeric components, so a bare
# count (`tools_len=16`) or a lone major cannot be read as a version. The
# leading and trailing boundaries keep `1.2` out of `v10.1.20`'s middle.
VERSION_TOKEN = re.compile(r"(?<![0-9.])[0-9]+(?:\.[0-9]+)+(?![0-9.])")

AGREE = 0
DISAGREE = 1
USAGE = 2
NOT_COMPARABLE = 3


def version_tokens(text):
    """The dotted-numeric version tokens in `text`, in order, deduplicated."""
    seen = []
    for token in VERSION_TOKEN.findall(text or ""):
        if token not in seen:
            seen.append(token)
    return seen


def compare(binary_version, wire_version):
    """Return (exit code, message) for one binary-vs-wire pair."""
    binary_tokens = version_tokens(binary_version)
    wire_tokens = version_tokens(wire_version)

    if not binary_tokens:
        return (
            NOT_COMPARABLE,
            f"the binary-side version {binary_version!r} carries no version token",
        )
    if not wire_tokens:
        return (
            NOT_COMPARABLE,
            f"the wire version {wire_version!r} carries no version token",
        )

    if set(binary_tokens) & set(wire_tokens):
        return AGREE, ""
    return (
        DISAGREE,
        f"the binary reports {binary_tokens} and the wire reports {wire_tokens}: "
        f"a client whose binary and user-agent disagree is not evidence about either",
    )


def main(argv):
    if len(argv) != 3 or argv[0] != "--compare":
        print(__doc__, file=sys.stderr)
        return USAGE

    code, message = compare(argv[1], argv[2])
    if message:
        print(f"client_version: {message}", file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
