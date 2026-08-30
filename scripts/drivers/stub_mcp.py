#!/usr/bin/env python3
"""A deterministic stdio MCP server for driver captures.

Answers exactly the three JSON-RPC methods a client exercises when it
lists and calls tools over the MCP stdio transport: `initialize`,
`tools/list`, `tools/call`. The tool set and every response are STATIC --
no state, no filesystem or network access, no randomness -- because two
captures of the same case are only comparable if this server answers the
same way both times.

`python3` is already an image dependency the drivers run against, so this
script needs no image change: any driver that wants an MCP wire pattern
hands a client `--mcp-config` pointing at this file directly.

Wire framing: one JSON-RPC object per line on stdin, one JSON-RPC object
per line on stdout, per the MCP stdio transport. A notification (no
`id`) gets no reply. EOF on stdin (the client closing its end when it
exits) ends the loop and this process, rather than the process leaking
past its owner.
"""

import json
import sys

PROTOCOL_VERSION = "2024-11-05"
SERVER_NAME = "routectl-fixture"
SERVER_VERSION = "1.0.0"

# Two tools, deterministic inputs and outputs. `echo` returns its argument
# unchanged; `add` returns the fixed sum of two integers. Enough to prove a
# call round-trips through the client without needing three.
TOOLS = [
    {
        "name": "echo",
        "description": "Return the given text unchanged.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": False,
        },
    },
    {
        "name": "add",
        "description": "Return the sum of two integers.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "b": {"type": "integer"},
            },
            "required": ["a", "b"],
            "additionalProperties": False,
        },
    },
]

TOOL_NAMES = frozenset(tool["name"] for tool in TOOLS)


def _text_result(text):
    return {"content": [{"type": "text", "text": text}], "isError": False}


def _error_result(message):
    return {"content": [{"type": "text", "text": message}], "isError": True}


def _call_echo(arguments):
    text = arguments.get("text") if isinstance(arguments, dict) else None
    if not isinstance(text, str):
        return _error_result("echo requires a string argument named 'text'")
    return _text_result(text)


def _call_add(arguments):
    if not isinstance(arguments, dict):
        return _error_result("add requires integer arguments named 'a' and 'b'")
    a, b = arguments.get("a"), arguments.get("b")
    if not isinstance(a, int) or not isinstance(b, int):
        return _error_result("add requires integer arguments named 'a' and 'b'")
    return _text_result(str(a + b))


TOOL_HANDLERS = {"echo": _call_echo, "add": _call_add}


def _reply(request_id, result):
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _reply_error(request_id, code, message):
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def _handle(message):
    """The response for one decoded JSON-RPC message, or None for a
    notification (a message with no `id`, which gets no reply)."""
    method = message.get("method")
    request_id = message.get("id")

    if method == "initialize":
        return _reply(
            request_id,
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )
    if method == "notifications/initialized":
        return None
    if method == "tools/list":
        return _reply(request_id, {"tools": TOOLS})
    if method == "tools/call":
        params = message.get("params") if isinstance(message.get("params"), dict) else {}
        name = params.get("name")
        arguments = params.get("arguments")
        handler = TOOL_HANDLERS.get(name)
        if handler is None:
            return _reply_error(request_id, -32602, f"unknown tool {name!r}")
        return _reply(request_id, handler(arguments))

    if request_id is None:
        return None
    return _reply_error(request_id, -32601, f"method not found: {method!r}")


def main():
    for raw_line in sys.stdin:
        line = raw_line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        response = _handle(message)
        if response is not None:
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
