#!/usr/bin/env python3
"""Report structural evidence in a user-supplied DeepSeek response JSON file.

The probe reads only the response tree pointers and a short allowlist of
possible continuation indicators. It never prints response values or message
content. Its output describes only the supplied file; fixture evidence is not
a live measurement and cannot settle the unresolved truncation case without a
genuinely truncated response.

Usage:
    python3 scripts/deepseek-response-probe.py RESPONSE.json
    python3 scripts/deepseek-response-probe.py --selftest

Exit codes: 0 = diagnostic completed (including unknown evidence) ·
2 = usage error · 3 = file could not be read.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import Counter
from pathlib import Path
from typing import Any


CONTINUATION_KEYS = (
    "has_more",
    "next_cursor",
    "next_page_token",
    "page_token",
    "cursor",
    "next",
)
CONTINUATION_PATHS = (
    (),
    ("data",),
    ("data", "biz_data"),
)


def json_type(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    return "object"


def is_finite_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def analyze(body: Any) -> dict[str, Any]:
    """Return content-free findings; unknown is retained when evidence is weak."""
    finding: dict[str, Any] = {
        "closure": "unknown",
        "closure_reason": "required tree evidence is missing or unreadable",
        "parent_types": Counter(),
        "continuation": {},
    }

    if isinstance(body, dict):
        data = body.get("data")
        biz = data.get("biz_data") if isinstance(data, dict) else None
        if isinstance(biz, dict):
            session = biz.get("chat_session")
            messages = biz.get("chat_messages")
            if isinstance(messages, list):
                for message in messages:
                    if isinstance(message, dict):
                        finding["parent_types"][json_type(message.get("parent_id"))
                            if "parent_id" in message else "absent"] += 1
            if isinstance(session, dict) and isinstance(messages, list):
                leaf = session.get("current_message_id")
                if is_finite_number(leaf) and all(isinstance(msg, dict) for msg in messages):
                    ids: dict[int | float, dict[str, Any]] = {}
                    duplicate = False
                    for message in messages:
                        message_id = message.get("message_id")
                        if not is_finite_number(message_id):
                            duplicate = True  # unusable identity makes closure unknown
                            break
                        if message_id in ids:
                            duplicate = True
                            break
                        ids[message_id] = message
                    if not duplicate:
                        if leaf not in ids:
                            finding["closure_reason"] = "current leaf is absent from the supplied message array"
                        else:
                            visited: set[int | float] = set()
                            current: int | float = leaf
                            while True:
                                if current in visited:
                                    finding["closure_reason"] = "parent chain contains a cycle"
                                    break
                                visited.add(current)
                                message = ids.get(current)
                                if message is None:
                                    finding["closure"] = "open"
                                    finding["closure_reason"] = "parent chain leaves the supplied message array"
                                    break
                                if "parent_id" not in message:
                                    finding["closure_reason"] = "a reached message has no parent_id field"
                                    break
                                parent = message["parent_id"]
                                if parent is None:
                                    finding["closure"] = "closed"
                                    finding["closure_reason"] = "walk from current leaf reaches a null parent"
                                    break
                                if not is_finite_number(parent):
                                    finding["closure_reason"] = "a reached parent link has an unsupported type"
                                    break
                                current = parent

        for path in CONTINUATION_PATHS:
            node: Any = body
            valid = True
            for key in path:
                if not isinstance(node, dict) or key not in node:
                    valid = False
                    break
                node = node[key]
            if not valid or not isinstance(node, dict):
                continue
            for key in CONTINUATION_KEYS:
                if key in node:
                    label = ".".join((*path, key)) if path else key
                    finding["continuation"][label] = json_type(node[key])

    return finding


def report(finding: dict[str, Any], *, source: str) -> None:
    print(f"[deepseek-probe] source: {source}")
    print("[deepseek-probe] evidence scope: supplied JSON only; not a live measurement")
    print(f"[deepseek-probe] visible-branch tree closure: {finding['closure']}")
    print(f"[deepseek-probe] closure basis: {finding['closure_reason']}")
    types: Counter = finding["parent_types"]
    if types:
        summary = ", ".join(f"{kind}={types[kind]}" for kind in sorted(types))
        print(f"[deepseek-probe] parent_id presence/type counts: {summary}")
    else:
        print("[deepseek-probe] parent_id presence/type counts: unknown (no readable message objects)")
    continuation: dict[str, str] = finding["continuation"]
    if continuation:
        summary = ", ".join(f"{key}:{continuation[key]}" for key in sorted(continuation))
        print(f"[deepseek-probe] recognized continuation indicators present (type only): {summary}")
    else:
        print("[deepseek-probe] recognized continuation indicators: none found in inspected locations")
    print(
        "[deepseek-probe] limitation: a closed tree cannot rule out truncation whose boundary "
        "parent_id was rewritten to null; resolving that requires a genuinely truncated response"
    )


def selftest() -> int:
    def fixture(messages: list[dict[str, Any]], leaf: Any = 2) -> dict[str, Any]:
        return {"data": {"biz_data": {
            "chat_session": {"current_message_id": leaf},
            "chat_messages": messages,
        }}}

    cases = [
        ("synthetic closed branch", fixture([
            {"message_id": 1, "parent_id": None},
            {"message_id": 2, "parent_id": 1},
        ]), "closed", {"null": 1, "number": 1}),
        ("synthetic open parent edge", fixture([
            {"message_id": 2, "parent_id": 1},
        ]), "open", {"number": 1}),
        ("synthetic missing tree fields", {"data": {"biz_data": {"chat_messages": []}}}, "unknown", {}),
        ("synthetic contradictory duplicate ids", fixture([
            {"message_id": 2, "parent_id": None},
            {"message_id": 2, "parent_id": 1},
        ]), "unknown", {"null": 1, "number": 1}),
    ]
    passed = 0
    for label, body, want_closure, want_types in cases:
        got = analyze(body)
        types = dict(got["parent_types"])
        ok = got["closure"] == want_closure and types == want_types
        print(f"[deepseek-probe] selftest {'PASS' if ok else 'FAIL'}: {label}")
        passed += int(ok)

    signal_body = fixture([{ "message_id": 2, "parent_id": None }])
    signal_body["data"]["biz_data"]["has_more"] = False
    signal_body["next_cursor"] = "synthetic-secret-value"
    signals = analyze(signal_body)["continuation"]
    signal_ok = signals == {"data.biz_data.has_more": "boolean", "next_cursor": "string"}
    print(f"[deepseek-probe] selftest {'PASS' if signal_ok else 'FAIL'}: synthetic continuation keys expose types only")
    passed += int(signal_ok)
    print(f"[deepseek-probe] selftest: {passed}/{len(cases) + 1} synthetic assertions passed")
    print("[deepseek-probe] selftest evidence is synthetic and is not a live measurement")
    return 0 if passed == len(cases) + 1 else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("response", nargs="?", help="user-supplied DeepSeek response JSON file")
    parser.add_argument("--selftest", action="store_true", help="run synthetic self-tests")
    args = parser.parse_args(argv)
    if args.selftest:
        if args.response:
            parser.error("--selftest does not take a response file")
        return selftest()
    if not args.response:
        parser.error("provide a response JSON file or use --selftest")
    try:
        raw = Path(args.response).read_text(encoding="utf-8")
    except OSError:
        print("[deepseek-probe] unable to read supplied file; evidence is unknown", file=sys.stderr)
        return 3
    try:
        body = json.loads(raw)
    except (json.JSONDecodeError, UnicodeError):
        body = None
    report(analyze(body), source="user-supplied JSON file")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
