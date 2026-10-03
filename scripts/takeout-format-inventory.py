#!/usr/bin/env python3
"""Print a bounded structural inventory of a caller-supplied Takeout JSON file.

Values are never emitted. The report contains field names, value types and
counts only; it is intended to help plan an importer, not to import anything.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter, defaultdict
from typing import Any

MAX_INPUT_BYTES = 32 * 1024 * 1024
MAX_NODES = 100_000
MAX_DEPTH = 10
MAX_FIELDS = 256
SAFE_FIELD_NAMES = frozenset(
    "author children content conversation_id conversations create_time current_node end_turn "
    "finish_details id is_visually_hidden_from_conversation mapping message "
    "message_flags message_type metadata model_slug parent parts recipient "
    "role status summary text update_time weight channel timestamp messages "
    "title prompt system fingerprint moderation_results content_references "
    "user_metadata gizmo_id conversation_template_id default_model_slug "
    "plugin_ids safe_urls blocked_urls voice voice_mode image_gen output audio "
    "asset_pointer size width height expiry mime_type name type url "
    "content_type metadata_type json_present code language recipient_id "
    "is_complete is_final is_user_system_message is_visually_hidden_from_conversation "
    "is_contextual_answers_system_message request_id parent_id children_ids "
    "message_id author_id parts role content_type text_value value "
    "timezone locale email account_id user_id access_token refresh_token "
    "conversation_title messages_count mapping_count"
    .split()
)


def _kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return "other"


def inventory(document: Any) -> dict[str, Any]:
    """Summarize field paths while preserving missing, empty, and null states."""
    paths: dict[str, Counter[str]] = defaultdict(Counter)
    parents: Counter[str] = Counter()
    present_by_parent: dict[str, Counter[str]] = defaultdict(Counter)
    field_presence: dict[str, Counter[str]] = defaultdict(Counter)
    field_types: dict[str, Counter[str]] = defaultdict(Counter)
    empty_states: dict[str, Counter[str]] = defaultdict(Counter)
    field_paths_seen: set[str] = set()
    nodes = 0
    truncated = False
    unsafe_keys = 0

    def walk(value: Any, path: str, depth: int) -> None:
        nonlocal nodes, truncated, unsafe_keys
        nodes += 1
        if nodes > MAX_NODES or depth > MAX_DEPTH:
            truncated = True
            return
        paths[path][_kind(value)] += 1
        if isinstance(value, list):
            child_path = f"{path}[]"
            for item in value:
                if nodes >= MAX_NODES:
                    truncated = True
                    break
                walk(item, child_path, depth + 1)
            return
        if not isinstance(value, dict):
            return

        parents[path] += 1
        safe_items = []
        for key, child in value.items():
            if not isinstance(key, str) or key not in SAFE_FIELD_NAMES:
                unsafe_keys += 1
                continue
            child_path = f"{path}.{key}"
            if child_path not in field_paths_seen and len(field_paths_seen) >= MAX_FIELDS:
                truncated = True
                continue
            field_paths_seen.add(child_path)
            safe_items.append((key, child, child_path))

        present = {key for key, _, _ in safe_items}
        for key in present:
            present_by_parent[path][key] += 1
            field_presence[f"{path}.{key}"]["present"] += 1

        for key, child, child_path in safe_items:
            field_types[child_path][_kind(child)] += 1
            if child is None:
                empty_states[child_path]["null"] += 1
            elif child == "":
                empty_states[child_path]["empty_string"] += 1
            elif isinstance(child, (list, dict)) and not child:
                empty_states[child_path]["empty_array" if isinstance(child, list) else "empty_object"] += 1
            walk(child, child_path, depth + 1)
            if nodes >= MAX_NODES:
                truncated = True
                break

    walk(document, "$", 0)
    fields = []
    for path in sorted(field_paths_seen):
        parent_path, _, field_name = path.rpartition(".")
        missing = max(0, parents[parent_path] - present_by_parent[parent_path][field_name])
        fields.append({
            "path": path,
            "present": field_presence[path]["present"],
            "missing": missing,
            "types": dict(sorted(field_types[path].items())),
            "empty": dict(sorted(empty_states[path].items())),
        })
    return {
        "format": "takeout-structure-v1",
        "nodes_examined": min(nodes, MAX_NODES),
        "truncated": truncated,
        "unrecognized_keys_omitted": unsafe_keys,
        "fields": fields,
        "value_shapes": [
            {"path": path, "types": dict(sorted(counts.items()))}
            for path, counts in sorted(paths.items())
        ],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("json_file", help="caller-supplied JSON file")
    args = parser.parse_args(argv)
    try:
        size = os.path.getsize(args.json_file)
        if size > MAX_INPUT_BYTES:
            print("takeout inventory: input exceeds size limit", file=sys.stderr)
            return 1
        with open(args.json_file, "r", encoding="utf-8") as handle:
            document = json.load(handle)
    except (OSError, UnicodeError, json.JSONDecodeError, RecursionError):
        print("takeout inventory: input could not be read as JSON", file=sys.stderr)
        return 1
    print(json.dumps(inventory(document), sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
