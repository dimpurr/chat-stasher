"""Regression tests for semantic citation bindings in the citation checker."""

from __future__ import annotations

import importlib.util
import os
import unittest


SCRIPT = os.path.join(os.path.dirname(os.path.dirname(__file__)), "check-citation-drift.py")
SPEC = importlib.util.spec_from_file_location("check_citation_drift", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class SessionTokenCitationTests(unittest.TestCase):
    def check_fixture(self, target: str, source: str) -> list[str]:
        doc = (
            "The extension requests the full conversation with the access token it reads "
            "from the same origin's `/api/auth/session` (`" + target + ":1-3`).\n"
        )
        basenames = {"page-hook.ts": ["apps/extension/lib/page-hook.ts"],
                     "platform-auth.ts": ["apps/extension/lib/platform-auth.ts"]}
        citations, parse_problems = checker.parse_text(
            "docs-dev/threat-model.md",
            doc.splitlines(),
            basenames,
            exists=lambda path: path in basenames[os.path.basename(path)],
        )
        self.assertEqual(parse_problems, [])
        return checker.semantic_binding_problems(
            citations,
            {"docs-dev/threat-model.md": doc},
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

    def test_documents_without_the_bound_claim_keep_ordinary_behavior(self) -> None:
        problems = checker.semantic_binding_problems(
            [],
            {"docs-dev/threat-model.md": "A synthetic document with no session claim.\n"},
            lambda _path: [],
        )
        self.assertEqual(problems, [])


if __name__ == "__main__":
    unittest.main()
