#!/usr/bin/env python3
"""Compare `ipatool mcp` of the Go and Rust tools over stdio.

Usage: scripts/mcp_parity.py GO_BIN RUST_BIN

Sends the same JSON-RPC session to both servers: initialize, the lists, argument
validation errors and read-only tool calls (search, versions, version metadata and
purchases of apps the account owns). Prints one line per call and exits 1 on a
difference. Server versions differ by design ("dev" in a Go build without ldflags).
"""

import json
import subprocess
import sys

PERPLEXITY = 1668000334

CALLS = [
    ("tools/list", "tools/list", {}),
    ("prompts/list", "prompts/list", {}),
    ("resources/list", "resources/list", {}),
    ("unknown tool", "tools/call", {"name": "auth_login", "arguments": {}}),
    ("missing term", "tools/call", {"name": "search_apps", "arguments": {}}),
    ("term type", "tools/call", {"name": "search_apps", "arguments": {"term": 123}}),
    ("extra argument", "tools/call", {"name": "search_apps", "arguments": {"term": "x", "password": "p"}}),
    ("fractional limit", "tools/call", {"name": "search_apps", "arguments": {"term": "x", "limit": 1.5}}),
    ("blank term", "tools/call", {"name": "search_apps", "arguments": {"term": "  "}}),
    ("bad platform", "tools/call", {"name": "search_apps", "arguments": {"term": "x", "platform": "unknown"}}),
    ("negative app", "tools/call", {"name": "purchase_app", "arguments": {"app_id": -1}}),
    ("missing version", "tools/call", {"name": "get_version_metadata", "arguments": {"app_id": 1}}),
    ("page 0", "tools/call", {"name": "list_purchases", "arguments": {"page": 0}}),
    ("search", "tools/call", {"name": "search_apps", "arguments": {"term": "perplexity", "limit": 3}}),
    ("versions", "tools/call", {"name": "list_app_versions", "arguments": {"app_id": PERPLEXITY}}),
    ("metadata", "tools/call", {"name": "get_version_metadata",
                                "arguments": {"bundle_identifier": "ai.perplexity.app", "external_version_id": "895516672"}}),
    ("purchases", "tools/call", {"name": "list_purchases", "arguments": {"max_results": 5}}),
]


def session(command):
    server = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def request(request_id, method, params):
        server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
        server.stdin.flush()
        while True:
            message = json.loads(server.stdout.readline())
            if message.get("id") == request_id:
                message.pop("jsonrpc", None)
                return message

    replies = {"initialize": request(0, "initialize", {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "parity", "version": "1"}})}
    replies["initialize"]["result"]["serverInfo"].pop("version")
    server.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    for request_id, (label, method, params) in enumerate(CALLS, 1):
        replies[label] = request(request_id, method, params)
    server.stdin.close()
    server.wait(timeout=30)
    return replies


def main():
    go, rust = session([sys.argv[1], "mcp"]), session([sys.argv[2], "mcp"])
    status = 0
    for label in go:
        if go[label] == rust[label]:
            print(f"ok    mcp {label}")
        else:
            status = 1
            print(f"DIFF  mcp {label}\n  go:   {json.dumps(go[label])[:400]}\n  rust: {json.dumps(rust[label])[:400]}")
    return status


if __name__ == "__main__":
    sys.exit(main())
