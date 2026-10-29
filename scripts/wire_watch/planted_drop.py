#!/usr/bin/env python3
"""Planted-drop cells for scripts/wire_watch/planted_drop.test.sh.

Each cell drives one request through a real routectl daemon against the mock
upstream below, planting a member the daemon is known to lose so the live wire
check must name it. The plants are taken from the zero-detection harness's
KNOWN_UNDECLARED list; when one of those losses is fixed, its plant here has to
be swapped for another entry of that list, and `weld` says so.

Modes:
  serve --port P --ready-file F [--plants on|off]
                       run the mock upstream on 127.0.0.1:P (a port the caller
                       probed free) and create F once listening
  request CELL [--plants on|off]
                       print the cell's ingress route and JSON body, one line
                       each, for the curl stub driver
  cells                print every cell as `name lane_file expected_ingress`
  weld --source FILE   check every plant against KNOWN_UNDECLARED in FILE
  check CELL HEALTH    check one fresh health.json carries the cell's plant
                       (or, for the control, no finding and nonzero checks)
  compare FRESH RECORDED
                       compare the (lane, leg, path, path_class) finding sets

Exit codes: 0 ok, 1 a check failed, 2 usage error.
"""

import json
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import strict_json  # noqa: E402

UPSTREAM_MODEL = "planted-upstream"
RESPONSES_LANE = "responses->openai-responses"


class Plant:
    def __init__(self, lane: str, leg: str, path: str) -> None:
        self.lane, self.leg, self.path = lane, leg, path

    def key(self) -> tuple[str, str, str]:
        return (self.lane, self.leg, self.path)


REQUEST_PLANT = Plant(RESPONSES_LANE, "request", "/temperature")
RESPONSE_PLANT = Plant(RESPONSES_LANE, "response", "/output/0/id")
PLANTS = (REQUEST_PLANT, RESPONSE_PLANT)

# name -> (lane config template, expected ingress, route, plant or None)
CELLS = {
    "request-leg-plant": ("responses", "openai-responses", "/v1/responses", REQUEST_PLANT),
    "response-leg-plant": ("responses", "openai-responses", "/v1/responses", RESPONSE_PLANT),
    "clean-control": ("chat", "openai", "/v1/chat/completions", None),
}


def fail(message: str) -> int:
    print(f"planted_drop: {message}", file=sys.stderr)
    return 1


def usage(message: str) -> int:
    print(f"planted_drop: {message}", file=sys.stderr)
    return 2


# ---------------------------------------------------------------------------
# Requests the stub driver sends
# ---------------------------------------------------------------------------

# The response-leg cell's input text, forwarded verbatim to the mock: it is
# how the mock tells that cell from the request-leg cell on the same route,
# so only the response-leg answer carries a message item id. The cell does
# not ask to stream: a streamed Responses render re-stamps `created_at`, a
# second known loss that would ride along with the plant.
ITEM_ID_MARKER = "plant the message item id"


def responses_input(text: str) -> list:
    return [{"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": text}]}]


def request_body(cell: str, plants_on: bool) -> tuple[str, dict]:
    _, _, route, _ = CELLS[cell]
    if cell == "request-leg-plant":
        body: dict = {"model": "planted", "input": responses_input("hi")}
        if plants_on:
            body["temperature"] = 0.3
        return route, body
    if cell == "response-leg-plant":
        return route, {"model": "planted", "input": responses_input(ITEM_ID_MARKER)}
    return route, {"model": "planted", "messages": [{"role": "user", "content": "hi"}]}


# ---------------------------------------------------------------------------
# Mock upstream
# ---------------------------------------------------------------------------

def sse(frames: list[tuple[str | None, dict]], done: bool) -> bytes:
    out = []
    for event, data in frames:
        if event:
            out.append(f"event: {event}\n")
        out.append(f"data: {json.dumps(data)}\n\n")
    if done:
        out.append("data: [DONE]\n\n")
    return "".join(out).encode()


def chat_complete() -> dict:
    return {
        "id": "c1", "object": "chat.completion", "created": 1_700_000_000,
        "model": UPSTREAM_MODEL, "system_fingerprint": None,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
    }


# Nonzero on purpose: a zero `created_at` is re-stamped by the egress, which
# adds a second, unplanted response-leg finding to the request-leg cell.
RESPONSES_CREATED_AT = 1_700_000_000


def carries_item_id_marker(body: dict) -> bool:
    return ITEM_ID_MARKER in json.dumps(body.get("input"))


def responses_stream(item_id: bool) -> bytes:
    """The upstream answer; `item_id` plants a message item id the egress
    drops, so the client's document lacks `/output/0/id`."""
    message = {"type": "message", "role": "assistant", "status": "completed",
               "content": [{"type": "output_text", "text": "ok", "annotations": []}]}
    if item_id:
        message["id"] = "msg_planted"
    done = {
        "id": "resp_1", "object": "response", "created_at": RESPONSES_CREATED_AT,
        "status": "completed", "model": UPSTREAM_MODEL,
        "output": [message],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4},
    }
    added = {"type": "message", "role": "assistant", "status": "in_progress", "content": []}
    if item_id:
        added["id"] = message["id"]
    created = dict(done, status="in_progress", output=[])
    return sse([
        ("response.created", {"type": "response.created", "response": created}),
        ("response.output_item.added",
         {"type": "response.output_item.added", "output_index": 0, "item": added}),
        ("response.content_part.added",
         {"type": "response.content_part.added", "output_index": 0, "content_index": 0,
          "part": {"type": "output_text", "text": "", "annotations": []}}),
        ("response.output_text.delta",
         {"type": "response.output_text.delta", "output_index": 0,
          "content_index": 0, "delta": "ok"}),
        ("response.completed", {"type": "response.completed", "response": done}),
    ], done=False)


MAX_REQUEST_BYTES = 1 << 20


def make_handler(plants_on: bool) -> type:
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: object) -> None:
            sys.stderr.write("mock: " + (format % args) + "\n")

        def reply(self, status: int, content_type: str, payload: bytes) -> None:
            self.send_response(status)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_POST(self) -> None:  # noqa: N802 - http.server's naming
            length = int(self.headers.get("content-length") or 0)
            if length > MAX_REQUEST_BYTES:
                self.reply(413, "text/plain", b"too large")
                return
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
            except json.JSONDecodeError:
                self.reply(400, "text/plain", b"bad json")
                return
            if self.path.endswith("/responses"):
                item_id = plants_on and carries_item_id_marker(body)
                self.reply(200, "text/event-stream", responses_stream(item_id))
            elif self.path.endswith("/chat/completions"):
                self.reply(200, "application/json", json.dumps(chat_complete()).encode())
            else:
                self.reply(404, "text/plain", b"no such route")

    return Handler


def serve(port: int, ready_file: str, plants_on: bool) -> int:
    try:
        server = ThreadingHTTPServer(("127.0.0.1", port), make_handler(plants_on))
    except OSError as e:
        return fail(f"mock upstream could not bind 127.0.0.1:{port}: {e.strerror}")
    with open(ready_file, "w", encoding="utf-8") as f:
        f.write("ready\n")
    server.serve_forever()
    return 0


# ---------------------------------------------------------------------------
# Weld against KNOWN_UNDECLARED
# ---------------------------------------------------------------------------

LANE_CONST = re.compile(r'^const ([A-Z_]+): &str = "([^"]+)";', re.M)
LIST = re.compile(r"const KNOWN_UNDECLARED: &\[KnownUndeclared\] = &\[(.*?)\n\];", re.S)
ENTRY = re.compile(
    r'KnownUndeclared \{\s*lane: ([A-Z_]+),\s*leg: Leg::(Request|Response),\s*path: "([^"]+)",'
)


def known_undeclared(source: str) -> set[tuple[str, str, str]]:
    lanes = dict(LANE_CONST.findall(source))
    block = LIST.search(source)
    if block is None:
        return set()
    return {
        (lanes.get(const, const), leg.lower(), path)
        for const, leg, path in ENTRY.findall(block.group(1))
    }


def weld(source_path: str) -> int:
    try:
        with open(source_path, encoding="utf-8") as f:
            source = f.read()
    except OSError as e:
        return fail(f"cannot read {source_path}: {e.strerror}")
    known = known_undeclared(source)
    if not known:
        return fail(f"no KNOWN_UNDECLARED entries parsed from {source_path}; the weld cannot run")
    rc = 0
    for plant in PLANTS:
        if plant.key() not in known:
            rc = fail(
                f"plant {plant.key()} is no longer in KNOWN_UNDECLARED ({source_path}): "
                "the loss was fixed or renamed -- swap the plant for a current entry "
                "and re-record the snapshots"
            )
    if rc == 0:
        print(f"planted_drop: {len(PLANTS)} plants welded to {len(known)} KNOWN_UNDECLARED entries")
    return rc


# ---------------------------------------------------------------------------
# Snapshot checks
# ---------------------------------------------------------------------------

def wire_check(health_path: str) -> dict:
    with open(health_path, encoding="utf-8") as f:
        text = f.read()
    try:
        doc = strict_json.loads(text)
    except strict_json.StrictJsonError as e:
        raise ValueError(f"{health_path}: {e.detail}") from None
    if not isinstance(doc, dict):
        raise ValueError(f"{health_path}: not a JSON object")
    data = doc.get("data")
    if not isinstance(data, dict):
        raise ValueError(f"{health_path}: data is not an object")
    rows = data.get("wire_check")
    if not isinstance(rows, dict):
        raise ValueError(f"{health_path}: no data.wire_check object")
    return rows


def finding_set(rows: dict) -> set[tuple[str, str, str, str]]:
    return {
        (r["lane"], r["leg"], r["path"], r["path_class"])
        for r in rows.get("loss_findings") or []
    }


def check_cell(cell: str, health_path: str) -> int:
    rows = wire_check(health_path)
    plant = CELLS[cell][3]
    if plant is None:
        if rows.get("loss_findings"):
            return fail(f"{cell}: the clean control carries findings {sorted(finding_set(rows))}")
        checks = sum(int(r.get("count", 0)) for r in rows.get("checks") or [])
        if checks <= 0:
            return fail(f"{cell}: the clean control ran no wire checks, so its empty finding list proves nothing")
        print(f"planted_drop: {cell}: no findings over {checks} checks")
        return 0
    named = {f[:3] for f in finding_set(rows)}
    if plant.key() not in named:
        return fail(f"{cell}: planted {plant.key()} not among findings {sorted(named)}")
    print(f"planted_drop: {cell}: planted {plant.key()} found")
    return 0


def compare(fresh: str, recorded: str) -> int:
    got, want = finding_set(wire_check(fresh)), finding_set(wire_check(recorded))
    if got == want:
        print(f"planted_drop: fresh findings match the recording ({len(got)} rows)")
        return 0
    return fail(
        f"fresh findings differ from the recording {recorded}: "
        f"missing {sorted(want - got)}, unexpected {sorted(got - want)}"
    )


# ---------------------------------------------------------------------------

def plants_flag(args: list[str]) -> tuple[list[str], bool]:
    if "--plants" in args:
        i = args.index("--plants")
        if i + 1 >= len(args) or args[i + 1] not in ("on", "off"):
            raise ValueError("--plants takes on or off")
        return args[:i] + args[i + 2:], args[i + 1] == "on"
    return args, True


def main(argv: list[str]) -> int:
    if not argv:
        return usage("no mode given (see the module docstring)")
    mode, args = argv[0], argv[1:]
    try:
        args, plants_on = plants_flag(args)
    except ValueError as e:
        return usage(str(e))
    if mode == "cells" and not args:
        for name, (lane, ingress, _, _) in CELLS.items():
            print(f"{name} {lane} {ingress}")
        return 0
    if (mode == "serve" and len(args) == 4 and args[0] == "--port"
            and args[1].isdigit() and args[2] == "--ready-file"):
        return serve(int(args[1]), args[3], plants_on)
    if mode == "request" and len(args) == 1 and args[0] in CELLS:
        route, body = request_body(args[0], plants_on)
        print(route)
        print(json.dumps(body))
        return 0
    if mode == "weld" and len(args) == 2 and args[0] == "--source":
        return weld(args[1])
    try:
        if mode == "check" and len(args) == 2 and args[0] in CELLS:
            return check_cell(args[0], args[1])
        if mode == "compare" and len(args) == 2:
            return compare(args[0], args[1])
    except (OSError, ValueError, KeyError, TypeError) as e:
        return fail(f"unreadable snapshot: {e}")
    return usage(f"bad arguments for mode {mode!r}")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
