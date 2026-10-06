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


def run_binding(doc: str, markers: tuple[str, ...], text: str,
                files: dict[str, str]) -> list[str]:
    """The problems for the bindings whose claim lives at `markers` in `doc`.

    The document is keyed under the binding's own name because
    semantic_binding_problems looks the claim up by that name; a fixture
    filed under a synthetic key would silently match no binding at all and
    every assertion built on it would pass for the wrong reason. The filter
    keeps each fixture honest when SEMANTIC_BINDINGS grows: a new binding
    matching the same document must fail or pass on its own legs, not on
    this file's assertions about some other binding.
    """
    basenames = {os.path.basename(path): path for path in files}
    citations, parse_problems = checker.parse_text(
        doc,
        text.splitlines(),
        basenames,
        exists=lambda path: path in files,
    )
    assert parse_problems == [], parse_problems
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

    run_binding = staticmethod(run_binding)

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


class W820SemanticBindingTests(unittest.TestCase):
    """The W820 audit bindings: prose→anchor extensions of the W600 guard.

    Each binding below was added because the audit found a claim whose
    anchors held true bytes without saying the thing the sentence asserts —
    or, for the never-anchored legs, nothing at all. Every case replays the
    anchor the prose carried before the fix, expects red naming the leg that
    was missing, then plays the range that really carries the mechanism and
    expects green. A case that only ever saw green would pass with the
    binding deleted, so the red half is the test.
    """

    run_binding = staticmethod(run_binding)

    KIMI_SOURCE = (
        "export const KIMI_ACCESS_TOKEN_STORAGE_KEY = 'access_token';\n"     # 1
        "\n"                                                                 # 2
        "export function needsKimiBearer(url, pageOrigin) {\n"              # 3
        "  return false;\n"                                                  # 4
        "}\n"                                                                # 5
        "export function createKimiAuthorizedFetch(pageOrigin, rawFetch, options) {\n"  # 6
        "  const send = async (url, init, token) => {\n"                      # 7
        "    return rawFetch(url, { ...init, headers: { authorization: `Bearer ${token}` } });\n"  # 8
        "  };\n"                                                             # 9
        "  return async (url, init) => {\n"                                  # 10
        "    if (!needsKimiBearer(url, pageOrigin)) return rawFetch(url, init);\n"  # 11
        "    const token = usableHeaderToken(options.readToken());\n"        # 12
        "    const first = await send(url, init, token);\n"                   # 13
        "    if (first.status !== 401 || token === null) return first;\n"    # 14
        "    return send(url, init, usableHeaderToken(options.readToken()));\n"  # 15
        "  };\n"                                                             # 16
        "}\n"                                                                # 17
    )
    KIMI_READER = (
        "function readKimiAccessToken(): string | null {\n"                  # 1
        "  try {\n"                                                          # 2
        "    return window.localStorage.getItem(KIMI_ACCESS_TOKEN_STORAGE_KEY);\n"  # 3
        "  } catch {\n"                                                      # 4
        "    return null;\n"                                                 # 5
        "  }\n"                                                              # 6
        "}\n"                                                                # 7
    )

    def test_outbound_port_claim_needs_both_defaults(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "The extension's only outbound HTTP port defaults to a function that\n"
            "refuses to send, and when it is wired every request goes through\n"
            "`checkBackfillRequest`, which refuses anything that is not same-origin.\n"
        )
        files = {
            "apps/extension/lib/backfill/engine.ts": (
                "async function sendVia(http, url, init) {\n"                  # 1
                "  return http(url, init);\n"                                  # 2
                "}\n"                                                          # 3
                "\n"                                                           # 4
                "/** The default port: it blows up on purpose. */\n"           # 5
                "export const notWiredHttp = async (url: string) => {\n"       # 6
                "  throw new Error(`refused to fetch ${url}`);\n"              # 7
                "};\n"                                                         # 8
            ),
            "apps/extension/lib/backfill/tab-port.ts": (
                "/** Page JS cannot reach this. */\n"                          # 1
                "export function checkBackfillRequest(\n"                     # 2
                "  spec: BackfillRequestSpec,\n"                               # 3
                "  pageOrigin: string,\n"                                      # 4
                "  lookup: PlanLookup = backfillPlanFor,\n"                    # 5
                "): RequestVerdict {\n"                                        # 6
                "  const u: URL = new URL(spec.url);\n"                        # 7
                "  if (u.origin !== pageOrigin) {\n"                            # 8
                "    return refuseUrl('url is not same-origin with the page');\n"  # 9
                "  }\n"                                                        # 10
                "  const row = getPlatformByOrigin(u.origin);\n"               # 11
                "  if (!row) {\n"                                              # 12
                "    return refuseUrl('origin is not in the platform table');\n"  # 13
                "  }\n"                                                       # 14
                "}\n"                                                         # 15
            ),
        }
        markers = ("only outbound HTTP port", "checkBackfillRequest")

        # The function above the refusing default: neither mechanism is named.
        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/backfill/engine.ts:1-3`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("notWiredHttp", stale[0])
        self.assertIn("url is not same-origin with the page", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/backfill/engine.ts:5-8`;\n"
              "`apps/extension/lib/backfill/tab-port.ts:2-15`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_dashboard_loopback_claim_needs_the_address_not_the_signature(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "Any program running as you can connect to the dashboard's port,\n"
            "because it listens on `127.0.0.1`. Loopback is not a security\n"
            "boundary. Without the token, nothing: every accepted GET route checks\n"
            "it with a constant-time comparison, and any method other than GET is\n"
            "refused.\n"
        )
        source = (
            "pub fn bind_ephemeral() -> std::io::Result<TcpListener> {\n"      # 1
            "    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));\n"  # 2
            "    TcpListener::bind(addr)\n"                                    # 3
            "}\n"                                                              # 4
            "fn ct_eq(a: &str, b: &str) -> bool {\n"                           # 5
            "    a.as_bytes() == b.as_bytes()\n"                               # 6
            "}\n"                                                              # 7
            "fn gate(method: &str, token: &str) -> Response {\n"               # 8
            "    if method != \"GET\" {\n"                                     # 9
            "        return Response::text(405, \"Method Not Allowed\", \"only GET\");\n"  # 10
            "    }\n"                                                          # 11
            "    if ct_eq(token, token) { succeed(); }\n"                       # 12
            "}\n"                                                              # 13
        )
        files = {"crates/chat-stasher/src/view.rs": source}
        markers = ("Loopback is not a security boundary", "constant-time comparison")

        # The anchor the prose carried: the signature line alone. The address
        # is the line below it, so the loopback leg fails its own claim.
        stale = self.run_binding(
            doc, markers,
            claim + "(`crates/chat-stasher/src/view.rs:1`, `:8-13`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("Ipv4Addr::LOCALHOST", stale[0])
        self.assertNotIn("ct_eq", stale[0])

        # The gate leg is pinned too: a gate range without the compare fails.
        gateless = self.run_binding(
            doc, markers,
            claim + "(`crates/chat-stasher/src/view.rs:1-4`, `:8-11`).\n", files,
        )
        self.assertEqual(len(gateless), 1)
        self.assertIn("ct_eq", gateless[0])
        self.assertNotIn("Ipv4Addr::LOCALHOST", gateless[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`crates/chat-stasher/src/view.rs:1-4`, `:8-13`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_master_key_modes_need_a_bounded_anchor(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "The master key file. It is written as plaintext JSON. On Unix it is\n"
            "created `0600` — the mode is set when the file is created, not\n"
            "afterwards — inside a parent directory tightened to `0700`; on\n"
            "platforms without Unix modes it inherits whatever the filesystem\n"
            "gives it.\n"
        )
        filler = "\n".join(f"    let filler_{i:02d} = {i};" for i in range(40))
        source = "\n".join([
            "/// The mode is set *when the file is created*, not afterwards: a `write` then",  # 1
            "/// `set_permissions` pair leaves a window in which the only key to the",        # 2
            "/// archive is world-readable.",                                                 # 3
            "pub fn persist_key_file(cfg: &StoreConfig, mk: &MasterKey) -> anyhow::Result<()> {",  # 4
            filler,                                                                          # 5-44
            "    let _ = fs::set_permissions(&parent, fs::Permissions::from_mode(0o700));",  # 45
            "    let mut options = fs::OpenOptions::new();",                                 # 46
            "    options.mode(0o600);",                                                      # 47
            "    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;",            # 48
            "    Ok(())",                                                                     # 49
            "}",                                                                              # 50
            "",
        ])
        files = {"crates/chat-stasher/src/store.rs": source}
        markers = ("plaintext JSON", "0700")

        # The old anchor: one span holding the rationale and the whole write
        # path — 50 lines, which no bounded range can prove.
        stale = self.run_binding(
            doc, markers,
            claim + "(`crates/chat-stasher/src/store.rs:1-50`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("options.mode(0o600)", stale[0])
        self.assertIn("The mode is set *when the file is created*", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`crates/chat-stasher/src/store.rs:1-3`, `:45-48`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_threat_model_kimi_claim_needs_a_range_that_names_the_source(self) -> None:
        doc = "docs-dev/threat-model.md"
        claim = (
            "Kimi reads the page origin's own `localStorage.access_token` at\n"
            "request time, sends it to Kimi's two backfill paths and nothing else,\n"
            "and holds no copy.\n"
        )
        files = {
            "apps/extension/lib/platform-auth.ts": self.KIMI_SOURCE,
            "apps/extension/entrypoints/dw-bridge.content.ts": self.KIMI_READER,
        }
        markers = ("localStorage.access_token", "holds no copy")

        # The wrapper alone: cited for a token *source* its range never names.
        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:6-17`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("KIMI_ACCESS_TOKEN_STORAGE_KEY", stale[0])
        self.assertIn("dw-bridge.content.ts", stale[0])
        self.assertNotIn("needsKimiBearer", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:1`, `:6-17`;\n"
              "`apps/extension/entrypoints/dw-bridge.content.ts:1-7`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_privacy_session_token_needs_the_rules_it_quotes(self) -> None:
        doc = "docs-dev/privacy.md"
        claim = (
            "That request carries your session's access token, which the\n"
            "extension reads from ChatGPT's own `/api/auth/session` on the same\n"
            "origin. The token is held only in the page's content-script memory:\n"
            "it is never written to storage, never logged, never sent to the\n"
            "`chat-stasher` host, and never attached to any other request.\n"
        )
        source = (
            "/**\n"                                                            # 1
            " * Rules for the token, all enforced in this file:\n"              # 2
            " * · it lives in this module's memory only — never storage, IndexedDB, logs, or\n"  # 3
            " *   anything sent to the native host;\n"                         # 4
            " */\n"                                                            # 5
            "export const CHATGPT_SESSION_PATH = '/api/auth/session';\n"       # 6
            "export function needsChatgptBearer(url, pageOrigin) {\n"          # 7
            "  return parsed.pathname === CHATGPT_LIST_PATH\n"                 # 8
            "    || parsed.pathname.startsWith(CHATGPT_DETAIL_PATH);\n"        # 9
            "}\n"                                                              # 10
            "async function readSessionToken(pageOrigin, rawFetch) {\n"       # 11
            "  const res = await rawFetch(`${pageOrigin}${CHATGPT_SESSION_PATH}`);\n"  # 12
            "  const token = body.accessToken;\n"                             # 13
            "  return token;\n"                                                # 14
            "}\n"                                                              # 15
            "export function createAuthorizedFetch(pageOrigin, rawFetch) {\n"  # 16
            "  let token: string | null = null;\n"                             # 17
            "}\n"                                                              # 18
        )
        files = {"apps/extension/lib/platform-auth.ts": source}
        markers = ("access token", "/api/auth/session")

        # What the doc carried before the audit: the path constant, the
        # reader, the gate and the wrapper — every range true bytes, none of
        # them saying "never storage, logs, or the native host".
        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:6`, `:7-10`, `:11-15`, `:16-18`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("Rules for the token", stale[0])
        self.assertIn("never storage, IndexedDB, logs", stale[0])
        self.assertNotIn("readSessionToken", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:1-5`, `:6`, `:7-10`,\n"
              "`:11-15`, `:16-18`).\n", files,
        )
        self.assertEqual(fixed, [])

    def test_privacy_kimi_token_needs_the_key_it_names_and_the_reader(self) -> None:
        doc = "docs-dev/privacy.md"
        claim = (
            "Kimi keeps that token in the page origin's own `localStorage`, under\n"
            "`access_token`; the extension reads it there at the moment of each\n"
            "request. It is attached to those two endpoints and to no other\n"
            "request; after a 401 it is re-read once and the request retried once.\n"
        )
        files = {
            "apps/extension/lib/platform-auth.ts": self.KIMI_SOURCE,
            "apps/extension/entrypoints/dw-bridge.content.ts": self.KIMI_READER,
        }
        markers = ("Kimi", "access_token", "localStorage")

        # The wrapper range the prose used to carry: it stops above the retry,
        # and neither the storage key nor the reader is cited at all.
        stale = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:6-13`).\n", files,
        )
        self.assertEqual(len(stale), 1)
        self.assertIn("'access_token'", stale[0])
        self.assertIn("first.status !== 401", stale[0])
        self.assertIn("dw-bridge.content.ts", stale[0])

        fixed = self.run_binding(
            doc, markers,
            claim + "(`apps/extension/lib/platform-auth.ts:1`, `:10-16`;\n"
              "`apps/extension/entrypoints/dw-bridge.content.ts:1-7`).\n", files,
        )
        self.assertEqual(fixed, [])


if __name__ == "__main__":
    unittest.main()
