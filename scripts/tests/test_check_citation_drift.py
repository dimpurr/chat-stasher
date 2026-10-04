"""Regression tests for semantic citation bindings in the citation checker."""

from __future__ import annotations

import importlib.util
import os
import re
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
        self.assertIn("apps/extension/lib/platform-auth.ts", problems[0])

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
        # Only the session binding's own disappearance is asserted here: this
        # document is a synthetic fixture, so the other W600 bindings bound to
        # the same document are legitimately missing from it too.
        self.assertEqual(len(self.for_session_claim(problems)), 1)
        self.assertIn("disappeared or drifted", self.for_session_claim(problems)[0])

    @staticmethod
    def for_session_claim(problems: list[str]) -> list[str]:
        """The problems belonging to the session-token binding alone."""
        return [p for p in problems if "'access token'" in p]

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
        self.assertEqual(len(self.for_session_claim(problems)), 1)
        self.assertIn("disappeared or drifted", self.for_session_claim(problems)[0])

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


class AuditedClaimBindingTests(unittest.TestCase):
    """The W600 bindings: a prose→anchor mismatch must turn the check red.

    Each case builds the claim with the anchor the prose used to carry — the one
    that held the bytes but not the words — and then with the range that really
    contains what the sentence names. Red on the first, green on the second is
    what makes the binding worth having; a test that only ever saw green would
    pass just as well with no binding at all.
    """

    def run_binding(self, doc: str, markers: tuple[str, ...], text: str,
                    files: dict[str, str]) -> list[str]:
        """The problems for the one binding that owns `doc` and `markers`.

        The document is keyed under the binding's own name because
        semantic_binding_problems looks the claim up by that name; a fixture
        filed under a synthetic key would silently match no binding at all and
        every assertion here would pass for the wrong reason.
        """
        basenames = {os.path.basename(path): path for path in files}
        citations, parse_problems = checker.parse_text(
            doc,
            text.splitlines(),
            basenames,
            exists=lambda path: path in files,
        )
        self.assertEqual(parse_problems, [])
        problems = checker.semantic_binding_problems(
            citations,
            {doc: text},
            lambda path: files[path].splitlines() if path in files else [],
        )
        return [
            problem
            for problem in problems
            if all(repr(marker) in problem for marker in markers)
        ]

    def test_sqlite_read_only_claim_needs_the_flag_and_both_uri_spellings(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "Harness session stores are opened read-only. Every SQLite connection uses\n"
            "`SQLITE_OPEN_READ_ONLY` with a `mode=ro` URI, falling back to\n"
            "`mode=ro&immutable=1` when a WAL store has no `-shm`\n"
        )
        # The policy comment: it states the intent and none of the mechanism.
        comment_only = (
            "  ///   * WAL store with no `-shm` \u21d2 open `mode=ro&immutable=1`.\n"
            "  ///   * everything else \u21d2 `mode=ro`.\n"
        )
        flags_and_uri = comment_only + (
            "fn open_readonly(db: &Path) -> rusqlite::Result<Connection> {\n"
            "    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;\n"
            "    let mut uri = format!(\"file:{}?mode=ro\", db.display());\n"
        )
        files = {"crates/chat-stasher/src/sqlite_probe.rs": flags_and_uri}

        stale = self.run_binding(
            doc, ("Harness session stores are opened read-only",),
            claim + "(`crates/chat-stasher/src/sqlite_probe.rs:1-2`).\n",
            files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("SQLITE_OPEN_READ_ONLY", stale[0])

        fixed = self.run_binding(
            doc, ("Harness session stores are opened read-only",),
            claim + "(`crates/chat-stasher/src/sqlite_probe.rs:1-5`).\n",
            files,
        )
        self.assertEqual(fixed, [])

    def test_process_boundary_claim_needs_the_send_call_not_only_the_host_name(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "Its one other process boundary is `runtime.sendNativeMessage` to the\n"
            "pinned host name "
        )
        source = (
            "export const NATIVE_HOST_NAME = 'com.chat_stasher.host';\n"
            "\n"
            "function sendOnce(runtime, message, onResponse) {\n"
            "  return new Promise((resolve) => {\n"
            "    if (!runtime || typeof runtime.sendNativeMessage !== 'function') {\n"
            "      resolve({ kind: 'no-api' });\n"
            "      return;\n"
            "    }\n"
            "    try {\n"
            "      const p = runtime.sendNativeMessage(NATIVE_HOST_NAME, message, onResponse);\n"
            "    } catch (err) {\n"
            "    }\n"
        )
        files = {"apps/extension/lib/native-host.ts": source}
        markers = ("process boundary", "runtime.sendNativeMessage")

        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/native-host.ts:1`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("runtime.sendNativeMessage", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/native-host.ts:1`, `:10-11`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_volatile_field_claim_needs_the_key_table_with_the_field(self) -> None:
        doc = "docs-dev/privacy.md"
        claim = (
            "For platforms with a known volatile field (ChatGPT's `safe_urls` today),\n"
            "the bundle also carries a **content fingerprint**\n"
        )
        source = (
            "const VOLATILE_KEYS: Readonly<Record<string, readonly string[]>> = {\n"
            "  chatgpt: ['safe_urls'],\n"
            "};\n"
            "\n"
            "export async function contentFingerprint(platform: string, text: string) {\n"
            "  const volatile = VOLATILE_KEYS[platform];\n"
            "  return sha256Hex(`${platform}\\n${JSON.stringify(volatile)}`);\n"
            "}\n"
        )
        files = {"apps/extension/lib/recapture.ts": source}
        markers = ("known volatile field", "safe_urls", "content fingerprint")

        stale = self.run_binding(
            doc, markers, claim + "(`apps/extension/lib/recapture.ts:5-7`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("safe_urls", stale[0])

        fixed = self.run_binding(
            doc, markers, claim + "(`apps/extension/lib/recapture.ts:1-7`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_org_order_claim_needs_cookie_and_route_in_two_files(self) -> None:
        doc = "docs-dev/privacy.md"
        claim = (
            "It is resolved in a fixed order \u2014 the `lastActiveOrg` cookie second,\n"
            "and one `GET /api/organizations` third\n"
        )
        # claude-page.ts builds its URL from a variable, so it says neither.
        files = {
            "apps/extension/lib/backfill/claude-org.ts": (
                "export const CLAUDE_ORG_COOKIE = 'lastActiveOrg';\n"
                "\n"
                "export function resolveClaudeOrg(input) {\n"
                "  const fromCookie = orgFromCookie(input.cookie);\n"
                "  if (fromCookie !== null) return { ok: true, org: fromCookie };\n"
                "  return { ok: false, halt: 'org-unresolved' };\n"
                "}\n"
            ),
            "apps/extension/lib/backfill/enumerate.ts": (
                "export const CLAUDE_LIST_PATH_TEMPLATE = '/api/organizations/{org}/chat_conversations';\n"
                "export const CLAUDE_RESOLVE_PATH = '/api/organizations';\n"
            ),
            "apps/extension/lib/backfill/claude-page.ts": (
                "  const fetchOrganizations = async (): Promise<string> => {\n"
                "    const reply = await serveBackfillFetch(\n"
                "      `${deps.pageOrigin}${resolvePath}`, deps.pageOrigin, deps.fetchImpl);\n"
                "    if (!reply.ok) throw new Error('did not complete');\n"
                "    return reply.text;\n"
                "  };\n"
            ),
        }
        markers = ("lastActiveOrg", "/api/organizations")

        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/backfill/claude-page.ts:1-5`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("CLAUDE_ORG_COOKIE", stale[0])
        self.assertIn("CLAUDE_RESOLVE_PATH", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim
            + "(`apps/extension/lib/backfill/claude-org.ts:1`, `:3-6`;\n"
              "`apps/extension/lib/backfill/enumerate.ts:2`).\n",
            files,
        )
        self.assertEqual(fixed, [])

    def test_one_met_requirement_does_not_satisfy_a_multi_file_claim(self) -> None:
        """A claim with three requirements must not pass on one of them.

        Pinning only the file a claim happens to mention is how the class of
        error this audit is about stays invisible: each requirement that is left
        unchecked is a name the prose may assert with nothing behind it.
        """
        doc = "docs-dev/privacy.md"
        claim = (
            "It is resolved in a fixed order \u2014 the `lastActiveOrg` cookie second,\n"
            "and one `GET /api/organizations` third\n"
        )
        files = {
            "apps/extension/lib/backfill/claude-org.ts": (
                "export const CLAUDE_ORG_COOKIE = 'lastActiveOrg';\n"
                "\n"
                "export function resolveClaudeOrg(input) {\n"
                "  const fromCookie = orgFromCookie(input.cookie);\n"
                "  if (fromCookie !== null) return { ok: true, org: fromCookie };\n"
                "  return { ok: false, halt: 'org-unresolved' };\n"
                "}\n"
            ),
            "apps/extension/lib/backfill/enumerate.ts": (
                "export const CLAUDE_LIST_PATH_TEMPLATE = '/api/organizations/{org}/chat_conversations';\n"
                "export const CLAUDE_RESOLVE_PATH = '/api/organizations';\n"
            ),
        }
        # Only the cookie constant is cited; the resolver and the route are not.
        partial = self.run_binding(
            doc, ("lastActiveOrg", "/api/organizations"),
            claim + "(`apps/extension/lib/backfill/claude-org.ts:1`).\n", files,
        )
        self.assertEqual(len(partial), 1)
        self.assertIn("resolveClaudeOrg", partial[0])
        self.assertIn("CLAUDE_RESOLVE_PATH", partial[0])
        self.assertNotIn("CLAUDE_ORG_COOKIE", partial[0].split("in each of:")[1].split(";")[0])


if __name__ == "__main__":
    unittest.main()
