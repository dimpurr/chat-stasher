"""Regression tests for semantic citation bindings in the citation checker."""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = os.path.join(os.path.dirname(os.path.dirname(__file__)), "check-citation-drift.py")
SPEC = importlib.util.spec_from_file_location("check_citation_drift", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class SessionTokenCitationTests(unittest.TestCase):
    DOC = "docs-dev/threat-model.md"
    TARGET = "apps/extension/lib/platform-auth.ts"

    def check_fixture(self, target: str, source: str, claim: str | None = None) -> list[str]:
        claim = claim or (
            "The extension requests the full conversation with the access token it reads "
            "from the same origin's `/api/auth/session`"
        )
        doc = f"{claim} (`{target}:1-3`).\n"
        basenames = {"page-hook.ts": ["apps/extension/lib/page-hook.ts"],
                     "platform-auth.ts": ["apps/extension/lib/platform-auth.ts"]}
        citations, parse_problems = checker.parse_text(
            self.DOC,
            doc.splitlines(),
            basenames,
            exists=lambda path: path in basenames[os.path.basename(path)],
        )
        self.assertEqual(parse_problems, [])
        return checker.semantic_binding_problems(
            citations,
            {self.DOC: doc},
            lambda path: source.splitlines() if path == target else [],
        )

    def test_page_hook_anchor_does_not_support_session_token_claim(self) -> None:
        problems = self.check_fixture(
            "apps/extension/lib/page-hook.ts",
            "const paged = platform && normalizedMethod === 'GET';\n"
            "post({ type: conversationSeenMessage });\n"
            "return;\n",
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("must cite apps/extension/lib/platform-auth.ts", problems[0])

    def test_platform_auth_token_read_anchor_supports_session_token_claim(self) -> None:
        problems = self.check_fixture(
            "apps/extension/lib/platform-auth.ts",
            "async function readSessionToken(pageOrigin, rawFetch) {\n"
            "  const res = await rawFetch(`${pageOrigin}${CHATGPT_SESSION_PATH}`);\n"
            "  const token = body.accessToken;\n",
        )
        self.assertEqual(problems, [])

    def test_live_claim_shape_allows_fetches_and_wrapped_prose(self) -> None:
        problems = self.check_fixture(
            self.TARGET,
            "async function readSessionToken(pageOrigin, rawFetch) {\n"
            "  const res = await rawFetch(`${pageOrigin}${CHATGPT_SESSION_PATH}`);\n"
            "  const token = body.accessToken;\n",
            "The extension sends the access token it\nfetches from `/api/auth/session`.",
        )
        self.assertEqual(problems, [])

    def test_duplicate_claim_paragraphs_fail(self) -> None:
        claim = "The access token comes from `/api/auth/session`."
        doc = f"{claim} (`{self.TARGET}:1-3`).\n\n{claim} (`{self.TARGET}:1-3`).\n"
        problems = checker.semantic_binding_problems(
            [], {self.DOC: doc}, lambda _path: [],
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("found 2", problems[0])

    def test_right_file_with_wrong_range_fails(self) -> None:
        problems = self.check_fixture(
            self.TARGET,
            "const unrelated = true;\n" * 3,
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("semantic citation mismatch", problems[0])

    def test_overbroad_range_fails(self) -> None:
        citation = checker.Citation(self.DOC, 1, f"{self.TARGET}:1-200", self.TARGET, 1, 200)
        source = ["async function readSessionToken() {", "CHATGPT_SESSION_PATH", ".accessToken"]
        problems = checker.semantic_binding_problems(
            [citation],
            {self.DOC: "The access token comes from `/api/auth/session`.\n"},
            lambda _path: source,
        )
        self.assertEqual(len(problems), 1)

    def test_live_binding_disappearance_fails_loudly(self) -> None:
        problems = checker.semantic_binding_problems(
            [], {self.DOC: "The session endpoint supplies a credential.\n"}, lambda _path: [],
            require_claims=True,
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("disappeared or drifted", problems[0])

    def test_collect_requires_the_binding_in_a_project_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            os.mkdir(os.path.join(root, "docs-dev"))
            with open(os.path.join(root, "Cargo.toml"), "w", encoding="utf-8") as handle:
                handle.write("[workspace]\n")
            with open(os.path.join(root, self.DOC), "w", encoding="utf-8") as handle:
                handle.write("The session endpoint supplies a credential.\n")
            with patch.object(checker, "REPO", root), \
                    patch.object(checker, "build_basename_index", return_value={}), \
                    patch.object(checker, "parse_docs", return_value=([], [])):
                _entries, problems = checker.collect()
        self.assertEqual(len(problems), 1)
        self.assertIn("disappeared or drifted", problems[0])

    def test_documents_without_the_bound_claim_keep_ordinary_behavior(self) -> None:
        problems = checker.semantic_binding_problems(
            [],
            {self.DOC: "A synthetic document with no session claim.\n"},
            lambda _path: [],
        )
        self.assertEqual(problems, [])

    def test_claimless_document_with_citations_keeps_ordinary_behavior(self) -> None:
        doc = "A synthetic citation (`apps/extension/lib/page-hook.ts:1`).\n"
        basenames = {"page-hook.ts": ["apps/extension/lib/page-hook.ts"]}
        citations, parse_problems = checker.parse_text(
            self.DOC, doc.splitlines(), basenames,
            exists=lambda path: path in basenames[os.path.basename(path)],
        )
        self.assertEqual(parse_problems, [])
        self.assertEqual(
            checker.semantic_binding_problems(citations, {self.DOC: doc}, lambda _path: []), []
        )


if __name__ == "__main__":
    unittest.main()
