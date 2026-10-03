from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest


SCRIPT = os.path.join(os.path.dirname(os.path.dirname(__file__)), "takeout-format-inventory.py")
SPEC = importlib.util.spec_from_file_location("takeout_format_inventory", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class TakeoutFormatInventoryTest(unittest.TestCase):
    def test_inventory_keeps_missing_empty_and_null_distinct(self) -> None:
        sample = {
            "conversations": [
                {"messages": [], "title": "", "metadata": None},
                {"messages": [{"author": {"role": "user"}}], "title": "synthetic title"},
            ]
        }
        result = MODULE.inventory(sample)
        fields = {field["path"]: field for field in result["fields"]}
        self.assertEqual(fields["$.conversations[].messages"]["types"], {"array": 2})
        self.assertEqual(fields["$.conversations[].messages"]["empty"], {"empty_array": 1})
        self.assertEqual(fields["$.conversations[].title"]["empty"], {"empty_string": 1})
        self.assertEqual(fields["$.conversations[].metadata"]["empty"], {"null": 1})
        self.assertEqual(fields["$.conversations[].metadata"]["missing"], 1)
        self.assertEqual(fields["$.conversations[].title"]["missing"], 0)

    def test_cli_emits_structure_without_input_values_or_path(self) -> None:
        secret_text = "synthetic-secret-message"
        secret_id = "synthetic-account-id"
        sample = {
            "id": secret_id,
            "messages": [{"content": secret_text}],
            "synthetic-account-key-93842": "synthetic-account-value",
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "synthetic-takeout.json")
            with open(path, "w", encoding="utf-8") as handle:
                json.dump(sample, handle)
            result = subprocess.run(
                [sys.executable, SCRIPT, path], capture_output=True, text=True, check=False
            )
        self.assertEqual(result.returncode, 0)
        self.assertNotIn(secret_text, result.stdout)
        self.assertNotIn(secret_id, result.stdout)
        self.assertNotIn("synthetic-account-key-93842", result.stdout)
        self.assertNotIn("synthetic-account-value", result.stdout)
        self.assertNotIn(path, result.stdout)
        self.assertNotIn(path, result.stderr)
        self.assertLess(len(result.stdout), 24_000)
        decoded = json.loads(result.stdout)
        self.assertEqual(decoded["format"], "takeout-structure-v1")
        self.assertEqual(decoded["unrecognized_keys_omitted"], 1)

    def test_oversized_and_unreadable_inputs_do_not_echo_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "synthetic-too-large.json")
            with open(path, "wb") as handle:
                handle.truncate(MODULE.MAX_INPUT_BYTES + 1)
            result = subprocess.run(
                [sys.executable, SCRIPT, path], capture_output=True, text=True, check=False
            )
        self.assertEqual(result.returncode, 1)
        self.assertNotIn(path, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
