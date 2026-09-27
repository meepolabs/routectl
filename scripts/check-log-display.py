"""Production log-sink inventory: the checker behind check-log-display.sh.

Proves ONE thing: the only production code that can build or install a
tracing subscriber is the approved builder in APPROVED, and that builder
still wires the escaping field formatter. It does not reason about
sanitizer dataflow.

1. Targets come from `cargo metadata`. `test`, `bench` and `example`
   targets are test-only; routectl-testkit is test-only only while no
   workspace package takes it as a normal or build dependency.
2. Every `.rs` file git reports as cached or as untracked-but-not-ignored
   is inventoried and classified by walking each target's module tree (`mod x;`, `#[path]`, `include!` in all three bracket
   forms) with Rust's own resolution rules. A module or include under a
   `cfg(test)` item is test-only; nothing is exempt by file name. The
   untracked half matters when the hook runs against an index that does
   not yet hold a file the work tree already compiles, such as a
   squash-merge or partial-commit index.
3. Any subscriber / global-dispatch token in production scope outside
   APPROVED is a finding, as is any token in a file no target reaches.
4. APPROVED is pinned: its `use` lines, the builder statement, the match
   that finishes it, and the single global install must match exactly,
   with exact counts, after comments are stripped and CR normalized.
5. `cargo metadata` must show no renamed tracing or log dependency.

Any git / cargo / read / UTF-8 / resolution failure exits non-zero.

Explicit test-only allowlist, and why each entry is safe:
  - cargo target kinds `test`, `bench`, `example`: cargo never links them
    into the shipped binary (`cargo metadata` target `kind`).
  - the routectl-testkit package: guarded in `targets()` -- it stays
    test-only only while every workspace dependency on it has
    `kind == "dev"`; a normal or build dependency is itself a finding.
  - code under an item whose `cfg` can only hold with `test` set
    (`cfg(test)`, `cfg(all(test, ..))`; never `any` or `not`).
"""

import json
import os
import re
import subprocess
import sys

APPROVED = "crates/routectl-cli/src/log_sink.rs"
TESTKIT = "routectl-testkit"
TEST_KINDS = {"test", "bench", "example"}

SINK_IDENTS = {
    "tracing_subscriber", "tracing_core", "tracing_log", "tracing_appender",
    "tracing_test", "traced_test", "set_global_default", "set_default",
    "with_default", "Dispatch", "Subscriber", "FmtSubscriber", "SubscriberBuilder",
    "SubscriberExt", "SubscriberInitExt", "LogTracer", "set_logger",
    "set_boxed_logger", "try_init", "NoSubscriber",
}
SINK_PATH_HEADS = {"subscriber", "dispatcher"}
RENAME_GUARDED = ("tracing", "log")

APPROVED_USES = [
    "use std::fmt;",
    "use tracing_subscriber::EnvFilter;",
    "use tracing_subscriber::field::RecordFields;",
    "use tracing_subscriber::fmt::MakeWriter;",
    "use tracing_subscriber::fmt::format::{DefaultFields, FmtSpan, FormatFields, Writer};",
    "use tracing_subscriber::util::SubscriberInitExt as _;",
]
APPROVED_BUILDER = [
    "let configured = tracing_subscriber::fmt()",
    ".fmt_fields(EscapingFields)",
    ".with_env_filter(filter)",
    ".with_target(true)",
    ".with_span_events(FmtSpan::CLOSE)",
    ".with_ansi(ansi)",
    ".with_writer(writer);",
    "match clock {",
    "Clock::Wall => Box::new(configured.finish()),",
    "Clock::Off => Box::new(configured.without_time().finish()),",
    "}",
]
APPROVED_INSTALL = "subscriber(filter, std::io::stderr, ansi, Clock::Wall).init();"
APPROVED_COUNTS = {
    "configured": 3, "fmt_fields": 1, "EscapingFields": 3, "Subscriber": 1,
    "tracing_subscriber": 6, "init": 2, "finish": 2,
}
USE_LINE = re.compile(r"^(pub(\([^)]*\))? )?use ")
# Production (normal / build) dependencies allowed to come from the tracing
# family, per package. Anything else -- another subscriber crate, a
# tracing-* helper that may install a default internally -- is a finding.
PROD_TRACING_DEPS = {"tracing": None, "tracing-subscriber": {"routectl-cli"}}
APPROVED_FORBIDDEN = {
    "set_global_default", "set_default", "with_default", "Dispatch", "FmtSubscriber",
    "SubscriberBuilder", "SubscriberExt", "LogTracer", "set_logger", "set_boxed_logger",
    "try_init", "NoSubscriber", "Registry", "registry", "Layer", "layer", "builder",
    "event_format", "map_event_format", "map_fmt_fields", "json", "pretty", "compact",
    "extern", "macro_rules", "mod", "include", "type",
}


class GuardError(Exception):
    """An environment, read, or resolution failure: the gate cannot vouch."""


# ---------------------------------------------------------------- lexer ---

def lex(text):
    """Tokenize Rust source. Returns (tokens, code) where tokens are
    (kind, value, line) with kind in ident/str/lit/punct, and code is the
    text with every comment blanked (newlines kept) for line pinning."""
    toks, out, i, n, line = [], [], 0, len(text), 1

    def blank(segment):
        out.append("".join("\n" if ch == "\n" else " " for ch in segment))

    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            out.append(c)
            i += 1
        elif c.isspace():
            out.append(c)
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(text[i:j])
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            if depth:
                raise GuardError("unterminated block comment")
            seg = text[i:j]
            blank(seg)
            line += seg.count("\n")
            i = j
        elif raw_string_start(text, i):
            j, value = read_raw_string(text, i)
            seg = text[i:j]
            toks.append(("str", value, line))
            out.append(seg)
            line += seg.count("\n")
            i = j
        elif c == '"' or (c in "bc" and text.startswith('"', i + 1)):
            j = i + (1 if c == '"' else 2)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            if j >= n:
                raise GuardError("unterminated string literal")
            seg = text[i:j + 1]
            toks.append(("str", seg[seg.index('"') + 1:-1], line))
            out.append(seg)
            line += seg.count("\n")
            i = j + 1
        elif c == "'" or (c == "b" and text.startswith("'", i + 1)):
            j = char_literal_end(text, i + (1 if c == "'" else 2))
            if j is None:
                k = i + 1
                while k < n and (text[k].isalnum() or text[k] == "_"):
                    k += 1
                toks.append(("lit", text[i:k], line))
                out.append(text[i:k])
                i = k
            else:
                toks.append(("lit", text[i:j], line))
                out.append(text[i:j])
                i = j
        elif c.isalpha() or c == "_":
            j = i + 1
            while j < n and (text[j].isalnum() or text[j] == "_"):
                j += 1
            word = text[i:j]
            if word == "r" and text.startswith("#", j) and j + 1 < n and (
                    text[j + 1].isalpha() or text[j + 1] == "_"):
                k = j + 1
                while k < n and (text[k].isalnum() or text[k] == "_"):
                    k += 1
                word, j = text[j + 1:k], k
            toks.append(("ident", word, line))
            out.append(text[i:j])
            i = j
        elif c.isdigit():
            j = i + 1
            while j < n and (text[j].isalnum() or text[j] == "_" or (
                    text[j] == "." and j + 1 < n and text[j + 1].isdigit())):
                j += 1
            toks.append(("lit", text[i:j], line))
            out.append(text[i:j])
            i = j
        elif text.startswith("::", i):
            toks.append(("punct", "::", line))
            out.append("::")
            i += 2
        else:
            toks.append(("punct", c, line))
            out.append(c)
            i += 1
    return toks, "".join(out)


def raw_string_start(text, i):
    j = i
    if text.startswith(("br", "cr"), j):
        j += 2
    elif text.startswith("r", j):
        j += 1
    else:
        return False
    if i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_"):
        return False
    while j < len(text) and text[j] == "#":
        j += 1
    return j < len(text) and text[j] == '"' and j > i + (2 if text[i] in "bc" else 1) - 1


def read_raw_string(text, i):
    j = i + (2 if text[i] in "bc" else 1)
    hashes = 0
    while text[j] == "#":
        hashes, j = hashes + 1, j + 1
    close = '"' + "#" * hashes
    end = text.find(close, j + 1)
    if end < 0:
        raise GuardError("unterminated raw string literal")
    return end + len(close), text[j + 1:end]


def char_literal_end(text, j):
    """Index past a char literal body starting at j, or None for a lifetime."""
    if j < len(text) and text[j] == "\\":
        k = text.find("'", j + 2)
        return None if k < 0 or k - j > 12 else k + 1
    if j + 1 < len(text) and text[j + 1] == "'":
        return j + 2
    return None


# ------------------------------------------------------------ structure ---

def matching(toks, i):
    """Index of the bracket closing toks[i]."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    opener, closer, depth = toks[i][1], pairs[toks[i][1]], 0
    for k in range(i, len(toks)):
        if toks[k][0] == "punct" and toks[k][1] == opener:
            depth += 1
        elif toks[k][0] == "punct" and toks[k][1] == closer:
            depth -= 1
            if depth == 0:
                return k
    raise GuardError("unbalanced bracket")


def item_end(toks, i):
    """Last index of the item starting at i: its `;`, its `,`, or the brace
    closing its body. Stops early rather than late, so a test scope is never
    wider than the item it gates."""
    depth = 0
    for k in range(i, len(toks)):
        kind, v, _ = toks[k]
        if kind != "punct":
            continue
        if v in "([{":
            depth += 1
        elif v in ")]}":
            depth -= 1
            if depth < 0:
                return k - 1
            if depth == 0 and v == "}":
                return k
        elif depth == 0 and v in ";,":
            return k
    return len(toks) - 1


def cfg_is_test_only(toks, lo, hi):
    """Whether the cfg predicate in toks[lo:hi] can only hold under `test`."""
    def parse(k):
        kind, v, _ = toks[k]
        if kind != "ident":
            return False, k + 1
        if k + 1 < hi and toks[k + 1][1] == "(" and v in ("all", "any", "not"):
            close = matching(toks, k + 1)
            parts, j = [], k + 2
            while j < close:
                val, j = parse(j)
                parts.append(val)
                while j < close and toks[j][1] == ",":
                    j += 1
            val = {"all": any(parts), "any": bool(parts) and all(parts), "not": False}[v]
            return val, close + 1
        j = k + 1
        if j < hi and toks[j][1] == "=":
            j += 2
        return v == "test", j
    return parse(lo)[0] if lo < hi else False


def attribute(toks, i):
    """Parse the attribute at toks[i] == '#'. Returns (end, inner, facts)."""
    inner = toks[i + 1][1] == "!"
    open_at = i + (2 if inner else 1)
    if toks[open_at][1] != "[":
        return None
    close = matching(toks, open_at)
    body = toks[open_at + 1:close]
    facts = {"test": False, "path": None, "cfg_attr_path": False}
    if body and body[0][1] == "cfg" and len(body) > 1 and body[1][1] == "(":
        facts["test"] = cfg_is_test_only(toks, open_at + 3, close - 1)
    elif body and body[0][1] == "path" and len(body) == 3 and body[1][1] == "=":
        if body[2][0] != "str":
            raise GuardError("non-literal #[path]")
        facts["path"] = body[2][1]
    elif body and body[0][1] == "cfg_attr":
        facts["cfg_attr_path"] = any(t[1] == "path" for t in body)
    return close, inner, facts


class Parsed:
    """One file's tokens with test-scope mask, module decls, and includes."""

    def __init__(self, text):
        self.toks, self.code = lex(text)
        n = len(self.toks)
        self.test = [False] * n
        self.mods, self.includes, self.cfg_attr_paths = [], [], []
        self._scan()

    def _mark(self, lo, hi):
        for k in range(lo, min(hi, len(self.toks) - 1) + 1):
            self.test[k] = True

    def _scan(self):
        toks, n, i = self.toks, len(self.toks), 0
        inline = []  # (close_index, name)
        pending = {"test": False, "path": None, "cfg_attr_path": False}
        while i < n:
            while inline and i > inline[-1][0]:
                inline.pop()
            kind, v, _ = toks[i]
            if kind == "punct" and v == "#" and i + 1 < n and toks[i + 1][1] in "[!":
                parsed = attribute(toks, i)
                if parsed:
                    close, inner, facts = parsed
                    if inner and facts["test"]:
                        self._mark(i, inline[-1][0] if inline else n - 1)
                    elif not inner:
                        pending["test"] |= facts["test"]
                        pending["path"] = facts["path"] or pending["path"]
                        pending["cfg_attr_path"] |= facts["cfg_attr_path"]
                    i = close + 1
                    continue
            if pending["test"]:
                self._mark(i, item_end(toks, i))
            if kind == "ident" and v in ("pub", "unsafe"):
                i = matching(toks, i + 1) + 1 if i + 1 < n and toks[i + 1][1] == "(" else i + 1
                continue
            if kind == "ident" and v == "mod" and i + 2 < n and toks[i + 1][0] == "ident":
                name, after = toks[i + 1][1], toks[i + 2][1]
                chain = [nm for _, nm in inline]
                if pending["cfg_attr_path"]:
                    self.cfg_attr_paths.append((i, toks[i][2]))
                if after == ";":
                    self.mods.append((i, name, pending["path"], chain))
                elif after == "{":
                    inline.append((matching(toks, i + 2), name))
            if kind == "ident" and v == "include" and i + 2 < n and toks[i + 1][1] == "!" \
                    and toks[i + 2][1] in "([{":
                close = matching(toks, i + 2)
                arg = toks[i + 3:close]
                literal = arg[0][1] if len(arg) == 1 and arg[0][0] == "str" else None
                self.includes.append((i, literal, [nm for _, nm in inline], toks[i][2]))
            pending = {"test": False, "path": None, "cfg_attr_path": False}
            i += 1

    def sink_hits(self):
        toks = self.toks
        for k, (kind, v, line) in enumerate(toks):
            if kind != "ident":
                continue
            if v in SINK_IDENTS or (v in SINK_PATH_HEADS and k + 1 < len(toks)
                                    and toks[k + 1][1] == "::"):
                yield k, v, line


# --------------------------------------------------------------- driver ---

def run(cmd):
    try:
        done = subprocess.run(cmd, capture_output=True, check=False)
    except OSError as err:
        raise GuardError(f"cannot run {cmd[0]}: {err}") from err
    if done.returncode != 0:
        detail = done.stderr.decode("utf-8", "replace").strip().splitlines()[-1:]
        raise GuardError(f"{' '.join(cmd[:2])} failed: {' '.join(detail)}")
    return done.stdout


def read_text(path):
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except OSError as err:
        raise GuardError(f"cannot read inventoried file {path}: {err}") from err
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as err:
        raise GuardError(f"{path} is not valid UTF-8") from err
    return text.replace("\r\n", "\n").replace("\r", "\n")


class Inventory:
    def __init__(self, root):
        self.root = root
        listing = run(["git", "-C", root, "ls-files", "-z", "--cached", "--others",
                       "--exclude-standard", "--", "*.rs"])
        self.files = {p for p in listing.decode("utf-8").split("\0") if p}
        if APPROVED not in self.files:
            raise GuardError(f"approved builder {APPROVED} is not in the inventory")
        self.parsed = {}
        self.visits = {}  # rel path -> set of scopes ("prod" / "test")
        self.seen = set()
        self.findings = []

    def parse(self, rel):
        if rel not in self.parsed:
            self.parsed[rel] = Parsed(read_text(os.path.join(self.root, rel)))
        return self.parsed[rel]

    def resolve(self, rel, origin):
        rel = os.path.normpath(rel)
        if rel in self.files:
            return rel
        where = "ignored or outside the repo" if os.path.exists(os.path.join(self.root, rel)) \
            else "missing"
        raise GuardError(f"{origin}: module source {rel} is {where}")

    def walk(self, rel, test, mod_dir, chain=(), included=False):
        """Visit `rel` (as a module whose children live under mod_dir)."""
        key = (rel, test, mod_dir, tuple(chain))
        if key in self.seen:
            return
        self.seen.add(key)
        self.visits.setdefault(rel, set()).add("test" if test else "prod")
        parsed = self.parse(rel)
        here = os.path.dirname(rel)
        if included and parsed.mods:
            line = parsed.toks[parsed.mods[0][0]][2]
            raise GuardError(f"{rel}:{line}: file module declared inside an include!d file")
        for idx, line in parsed.cfg_attr_paths:
            if not (test or parsed.test[idx]):
                self.findings.append(f"conditional #[cfg_attr(.., path)] module: {rel}:{line}")
        for idx, name, path_attr, inner in parsed.mods:
            scoped = test or parsed.test[idx]
            base = os.path.join(mod_dir, *chain, *inner)
            origin = f"{rel}:{parsed.toks[idx][2]}"
            if path_attr is not None:
                attr_base = os.path.join(here, *chain, *inner) if not (chain or inner) \
                    else base
                child = self.resolve(os.path.join(attr_base, path_attr), origin)
                self.walk(child, scoped, os.path.dirname(child))
                continue
            flat, nested = os.path.join(base, name + ".rs"), os.path.join(base, name, "mod.rs")
            found = [p for p in (flat, nested) if os.path.normpath(p) in self.files]
            if len(found) != 1:
                raise GuardError(f"{origin}: mod {name} resolves to {len(found)} inventoried files")
            child = os.path.normpath(found[0])
            child_dir = os.path.dirname(child) if child.endswith("mod.rs") \
                else os.path.join(os.path.dirname(child), name)
            self.walk(child, scoped, child_dir)
        for idx, literal, inner, line in parsed.includes:
            if literal is None:
                raise GuardError(f"{rel}:{line}: include! argument is not a string literal")
            child = self.resolve(os.path.join(here, literal), f"{rel}:{line}")
            self.walk(child, test or parsed.test[idx], mod_dir, list(chain) + inner, True)

    def scope_of(self, rel, idx):
        scopes = self.visits.get(rel, set())
        if "prod" in scopes and not self.parsed[rel].test[idx]:
            return "prod"
        return "test" if scopes else "unreached"


def targets(root):
    meta = json.loads(run(["cargo", "metadata", "--format-version", "1", "--no-deps",
                           "--offline", "--manifest-path", os.path.join(root, "Cargo.toml")]))
    findings, roots = [], []
    testkit_in_prod = False
    for pkg in meta["packages"]:
        for dep in pkg["dependencies"]:
            name, kind = dep["name"], dep.get("kind")
            if dep.get("rename") and name.startswith(RENAME_GUARDED):
                findings.append(f"renamed dependency {dep['rename']} = {name} "
                                f"in {pkg['name']}")
            if kind in (None, "build") and name.startswith(("tracing-", "tracing_")):
                allowed = PROD_TRACING_DEPS.get(name, set())
                if allowed is not None and pkg["name"] not in allowed:
                    findings.append(f"production dependency {name} in {pkg['name']} "
                                    "is outside the log-sink allowlist")
            if dep["name"] == TESTKIT and dep.get("kind") in (None, "build"):
                testkit_in_prod = True
                findings.append(f"{TESTKIT} is a {dep.get('kind') or 'normal'} dependency "
                                f"of {pkg['name']}")
    for pkg in meta["packages"]:
        for target in pkg["targets"]:
            src = os.path.relpath(os.path.realpath(target["src_path"]), root)
            test_only = bool(set(target["kind"]) & TEST_KINDS) or (
                pkg["name"] == TESTKIT and not testkit_in_prod)
            roots.append((src, test_only))
    if not roots:
        raise GuardError("cargo metadata listed no targets")
    return roots, findings


def check_approved(inv):
    parsed = inv.parsed.get(APPROVED)
    if parsed is None or "prod" not in inv.visits.get(APPROVED, set()):
        inv.findings.append(f"{APPROVED} is not reached as production code")
        return
    lines = [" ".join(l.split()) for l in parsed.code.split("\n")]
    lines = [l for l in lines if l]
    uses = [l for l in lines if USE_LINE.match(l)]
    if uses != APPROVED_USES:
        inv.findings.append(f"{APPROVED} imports differ from the pinned set: {uses}")
    starts = [k for k, l in enumerate(lines) if l == APPROVED_BUILDER[0]]
    if len(starts) != 1 or lines[starts[0]:starts[0] + len(APPROVED_BUILDER)] != APPROVED_BUILDER:
        inv.findings.append(f"{APPROVED} builder statement differs from the pinned chain "
                            "(.fmt_fields(EscapingFields) and nothing else)")
    if lines.count(APPROVED_INSTALL) != 1:
        inv.findings.append(f"{APPROVED} must install exactly once via: {APPROVED_INSTALL}")
    idents = [t for t in parsed.toks if t[0] == "ident"]
    count = lambda name: sum(1 for t in idents if t[1] == name)
    for name, want in APPROVED_COUNTS.items():
        if count(name) != want:
            inv.findings.append(f"{APPROVED} names {name} {count(name)} times, pinned {want}")
    toks = parsed.toks
    ctor = sum(1 for k in range(len(toks) - 3) if toks[k][1] == "tracing_subscriber"
               and toks[k + 1][1] == "::" and toks[k + 2][1] == "fmt"
               and toks[k + 3][1] != "::")
    inits = sum(1 for k in range(1, len(toks)) if toks[k][1] == "init" and toks[k - 1][1] == ".")
    if ctor != 1:
        inv.findings.append(f"{APPROVED} references the fmt() constructor {ctor} times")
    if inits != 1:
        inv.findings.append(f"{APPROVED} calls .init {inits} times")
    bad = sorted({t[1] for t in idents if t[1] in APPROVED_FORBIDDEN})
    if bad:
        inv.findings.append(f"{APPROVED} uses forbidden names: {', '.join(bad)}")


def main():
    try:
        root = os.path.realpath(
            run(["git", "rev-parse", "--show-toplevel"]).decode("utf-8").strip())
    except GuardError:
        print("check-log-display: not inside a git work tree", file=sys.stderr)
        return 1
    try:
        inv = Inventory(root)
        roots, meta_findings = targets(root)
        inv.findings.extend(meta_findings)
        for src, test_only in roots:
            src = inv.resolve(src, "cargo metadata target")
            inv.walk(src, test_only, os.path.dirname(src))
        counts = {"prod": 0, "test": 0, "unreached": 0}
        for rel in sorted(inv.files):
            parsed = inv.parse(rel)
            scopes = inv.visits.get(rel, set())
            counts["prod" if "prod" in scopes else "test" if scopes else "unreached"] += 1
            for idx, name, line in parsed.sink_hits():
                scope = inv.scope_of(rel, idx)
                if scope == "test" or (scope == "prod" and rel == APPROVED):
                    continue
                if scope == "prod":
                    inv.findings.append(f"subscriber token {name} outside {APPROVED}: {rel}:{line}")
                else:
                    inv.findings.append(f"subscriber token {name} in a file no target reaches: "
                                        f"{rel}:{line}")
        check_approved(inv)
    except GuardError as err:
        print(f"check-log-display: {err}", file=sys.stderr)
        return 1
    for finding in inv.findings:
        print(f"check-log-display: {finding}", file=sys.stderr)
    if inv.findings:
        print(f"check-log-display: FAIL ({len(inv.findings)} finding(s))", file=sys.stderr)
        return 1
    print(f"check-log-display: PASS ({counts['prod']} production, {counts['test']} test-only, "
          f"{counts['unreached']} unreached Rust files; one escaping sink)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
