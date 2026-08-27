#!/usr/bin/env bash
# Path confinement for scripts that write capture output to a
# caller-supplied directory.
#
# Sourced, never executed. It owns the ONE copy of the resolution pair
# and the containment test: fixtures carry RAW headers (auth included
# when the daemon runs with ROUTECTL_TRACE_HEADERS), so a script that
# takes a destination path from its caller is a write primitive aimed at
# whatever that caller names. Every such script confines through here.
#
# The logic below encodes three separately-discovered subtleties --
# collapse AFTER symlink resolution, dangling symlinks slipping past
# `cd -P`, and per-component `[ -L ]` checking BEFORE resolution. Each
# is explained at its site. A second implementation would rediscover
# them one incident at a time, which is why this file exists rather than
# a paragraph telling the next author what to write.
#
# Every function below is called from a script running under `set -eu`.

# Lexically resolve a path to absolute, collapsing `.` and `..` without
# touching the filesystem. Portable (no realpath / -m dependency) and
# works for a not-yet-created directory. Symlinks are NOT followed: the
# default captured tree has none and confinement only needs lexical
# containment.
abspath_lexical() {
  case "$1" in
    /*) _p="$1" ;;
    *)  _p="$PWD/$1" ;;
  esac
  printf '%s\n' "$_p" | awk -F/ '
    { n = 0
      for (i = 1; i <= NF; i++) {
        if ($i == "" || $i == ".") continue
        if ($i == "..") { if (n > 0) n--; continue }
        seg[++n] = $i
      }
      out = ""
      for (i = 1; i <= n; i++) out = out "/" seg[i]
      print (out == "" ? "/" : out)
    }'
}

# Physically resolve a path to absolute, FOLLOWING symlinks, so a
# symlinked component cannot disguise an out-of-tree destination as an
# in-tree one. The path need not exist yet: walk up the RAW path (no
# lexical `..` collapse first -- collapsing before symlink resolution is
# unsafe, since `link/..` must resolve through the link, not cancel its
# name) to the nearest EXISTING ancestor, resolve THAT with
# `cd -P` / `pwd -P` (portable; no `realpath -m` dependency), then
# re-append the non-existing tail. Tail components do not exist, so they
# cannot be symlinks; a final lexical collapse of the combined path is
# therefore physically faithful.
abspath_physical() {
  case "$1" in
    /*) _p="$1" ;;
    *)  _p="$PWD/$1" ;;
  esac
  _tail=""
  while [ ! -e "$_p" ] && [ "$_p" != "/" ]; do
    _tail="$(basename "$_p")${_tail:+/$_tail}"
    _p="$(dirname "$_p")"
  done
  _phys="$(cd -P "$_p" 2>/dev/null && pwd -P)" || {
    echo "cannot physically resolve path ancestor: $_p" >&2
    exit 2
  }
  if [ -n "$_tail" ]; then
    abspath_lexical "$_phys/$_tail"
  else
    printf '%s\n' "$_phys"
  fi
}

# Refuse `$1` (a lexically-collapsed absolute candidate) unless it is
# `$2` (an absolute allowed root) or lives under it. Exits 2 on refusal;
# returns 0 when the candidate is confined.
#
# The root is a PARAMETER so a second caller confines against its own
# tree rather than re-deriving this logic against a different constant.
#
# The candidate keeps its lexically-collapsed form on the write path;
# the confinement test compares the PHYSICALLY resolved
# (symlink-following) candidate against the physically resolved root. A
# purely lexical compare cannot see through a symlinked subdir under the
# root, so `<root>/<symlink>/x` could escape confinement -- resolving
# both sides physically closes that hole while still normalizing `..`
# traversals such as `<root>/../../src`.
confine_out_under() {
  _cand="$1"
  _root="$2"

  # Belt-and-suspenders: walk every candidate component UNDER the root
  # and reject any symlink, even a DANGLING one (target does not
  # exist). The physical resolution further down walks up to the
  # nearest EXISTING ancestor with `cd -P`, so a broken symlink under
  # the root (e.g. `<root>/<dangling-link>/<leaf>` where leaf does not
  # yet exist) slips past it; the caller's `mkdir -p` also cannot
  # reify a dangling symlink as a directory. `[ -L ]` is the POSIX
  # symlink test -- true for any symlink regardless of whether its
  # target resolves -- and it is run BEFORE physical resolution
  # because resolution loses the per-component symlink information.
  # Out-of-tree paths skip this loop and are rejected by the physical
  # confinement test below.
  case "$_cand" in
    "$_root" | "$_root"/*)
      _check="$_root"
      _remaining="${_cand:${#_root}}"
      _remaining="${_remaining#/}"
      while [ -n "$_remaining" ]; do
        case "$_remaining" in
          */*) _seg="${_remaining%%/*}"; _remaining="${_remaining#*/}" ;;
          *)   _seg="$_remaining"; _remaining="" ;;
        esac
        [ -z "$_seg" ] && continue
        _check="$_check/$_seg"
        if [ -L "$_check" ]; then
          echo "refusing --out '$_cand': symlink component at '$_check'." >&2
          echo "fixtures contain raw headers (auth when ROUTECTL_TRACE_HEADERS is on);" >&2
          echo "a symlink under the captured tree could redirect writes outside it." >&2
          echo "pass --allow-unsafe-out to override." >&2
          exit 2
        fi
      done
      ;;
  esac

  _out_phys="$(abspath_physical "$_cand")"
  _root_phys="$(abspath_physical "$_root")"
  case "$_out_phys" in
    "$_root_phys" | "$_root_phys"/*) : ;;
    *)
      echo "refusing --out '$_cand': outside the default captured dir '$_root'." >&2
      echo "fixtures contain raw headers (auth when ROUTECTL_TRACE_HEADERS is on)." >&2
      echo "pass --allow-unsafe-out to write outside the default tree on purpose." >&2
      exit 2
      ;;
  esac
}
