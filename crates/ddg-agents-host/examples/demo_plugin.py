#!/usr/bin/env python3
"""Minimal example backend plugin for ddg-agents-host (ext.demo.* over stdio JSON-RPC 2.0)."""

import json
import sys

next_rev_id = 1000


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def reverse_call(method, params):
    global next_rev_id
    req_id = next_rev_id
    next_rev_id += 1
    send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
    while True:
        raw = sys.stdin.readline()
        if not raw:
            raise EOFError("host closed pipe")
        msg = json.loads(raw)
        if msg.get("id") == req_id:
            return msg


for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    req = json.loads(line)
    req_id = req.get("id")
    method = req.get("method", "")
    params = req.get("params") or {}

    if method == "plugin.hello":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "name": "demo",
                "version": "0.1.0",
                "methods": [
                    {"name": "ext.demo.echo", "description": "Echo back input payload"},
                    {"name": "ext.demo.room_count", "description": "Read room count via host_read"},
                    {"name": "ext.demo.try_forbidden_write", "description": "Attempt forbidden host_write"},
                    {"name": "ext.demo.crash", "description": "Simulate plugin process crash"},
                ],
                "events": [],
            },
        })
    elif method == "ext.demo.echo":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"echo": params.get("text", "")}})
    elif method == "ext.demo.room_count":
        reply = reverse_call("host.catalog_get", {})
        if "error" in reply and reply["error"] is not None:
            send({"jsonrpc": "2.0", "id": req_id, "error": reply["error"]})
        else:
            rooms = reply["result"].get("rooms", [])
            room_ids = [room["id"] for room in rooms]
            send({"jsonrpc": "2.0", "id": req_id, "result": {
                "rooms": len(room_ids), "room_ids": room_ids,
            }})
    elif method == "ext.demo.try_forbidden_write":
        reply = reverse_call("host.room_create", {"cols": 80, "rows": 24})
        if "error" in reply and reply["error"] is not None:
            send({"jsonrpc": "2.0", "id": req_id, "error": reply["error"]})
        else:
            send({"jsonrpc": "2.0", "id": req_id, "result": {"unexpected": True}})
    elif method == "ext.demo.crash":
        sys.exit(1)
    else:
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32001,
                "message": f"unknown method {method}",
                "data": {"kind": "not_found"},
            },
        })
