#!/usr/bin/env python3
"""引用锚点校验 (citation lock)。

公开文档里有大量 `文件:行号` 形式的出处引用。行号会漂: 代码被编辑之后行号
还在、那一行也还非空, 但它已经不再是当初被引用的那段内容。只校验"行号没越界"
的检查器对这种漂移完全失明 —— 这个脚本改为校验 **被引行的内容本身**。

做法: 把每条引用解析成 (被引文件, 行范围), 对该范围的内容算一个摘要
(逐行 strip 后用 \\n 连接, 取 SHA-256 前 12 位) 并写进 docs-dev/citations.lock。
再次运行时重新计算并与 lockfile 比对, 不一致就退出非零。

默认只校验、绝不写 lockfile。要接受一次真实的内容变化, 必须显式跑 --update,
并且那次改动会出现在 git diff 里、被人看见。

用法:
    python3 scripts/check-citation-drift.py            # 校验 (默认, 只读)
    python3 scripts/check-citation-drift.py --update   # 重新生成 lockfile
    python3 scripts/check-citation-drift.py --list     # 打印解析出的全部引用

退出码: 0 = 一致; 1 = 检出漂移/无法解析的引用; 2 = 用法错误或 lockfile 缺失。
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import os
import re
import sys
from typing import Callable

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOCK_PATH = os.path.join(REPO, "docs-dev", "citations.lock")

# 被扫描的公开文档。lockfile 自己不在其中。
DOC_FILES = [
    "README.md",
    "SECURITY.md",
    "CONTRIBUTING.md",
    "docs-dev/install.md",
    "docs-dev/node-metadata.md",
    "docs-dev/openclaw-scanner.md",
    "docs-dev/orphan-packs.md",
    "docs-dev/privacy.md",
    "docs-dev/threat-model.md",
]


def doc_files() -> list[str]:
    """The documents in scope: DOC_FILES plus every contracts/*.md.

    contracts/ is globbed rather than enumerated one path at a time, so a new
    contract document is covered the moment it exists. contracts/ sat outside
    the scan for exactly that reason: it was not on a hand-written list, and a
    citation there could name a file that does not exist while every gate stayed
    green.

    🔴 A document named here that is missing is reported, not skipped: see
    parse_docs(). "We could not look" must never read as "nothing to find".
    """
    return DOC_FILES + sorted(
        os.path.relpath(p, REPO) for p in glob.glob(os.path.join(REPO, "contracts", "*.md"))
    )


# 解析被引文件时不进入的目录。
SKIP_DIRS = {"target", "node_modules", "dist", ".output", ".wxt"}

CITED_EXTS = ("rs", "ts", "tsx", "js", "mjs", "json", "toml", "sh", "py", "md", "html", "css", "yml", "yaml")

# 行内 code span。所有出处引用都写在反引号里。
CODE_SPAN_RE = re.compile(r"`([^`\n]+)`")

# code span 内部的一条引用:
#   crates/chat-stasher/src/main.rs:25
#   data/harness-registry-v1.json:64-85
#   main.rs:163-165,2700-2735        (省略路径, 沿用上一条引用的文件)
#   :603-611                          (省略文件名, 沿用上一条引用的文件)
CITATION_RE = re.compile(
    r"(?P<path>[A-Za-z0-9_./-]+\.(?:" + "|".join(CITED_EXTS) + r"))?"
    r":(?P<spans>\d+(?:-\d+)?(?:,\d+(?:-\d+)?)*)(?![\d./A-Za-z-])"
)

# A path the regex above does not recognise is NOT automatically a continuation.
# This picks up the token sitting directly before the colon inside the same code
# span. `.gitignore:15` is the case that motivated this: `.gitignore` has no
# extension, so the citation matched as a bare `:15` and silently inherited the
# file named five lines above it — an anchor pointing at a file the sentence
# never mentioned, with every gate green.
#
# 🔴 A token found here is a citation only when it is *shaped* like a path —
# see is_path_shaped(). The first version of this rule demanded that every token
# before a `:N` resolve, which made ordinary inline code a hard failure:
# `http://x:8080` (token `//x`), `example.com:8080`, `std::fmt:5` and `HH:23`
# are not citations, and a document that mentions one must not go red.
PATH_TOKEN_RE = re.compile(r"(?P<token>[A-Za-z0-9_./-]+)$")

# A sentence ends where one of these is followed by whitespace or the end of the
# line. Continuation inheritance is scoped to one sentence: a bare `:N` takes
# its file from a citation in the same sentence — the `\`a.ts:1\`, `\`:2\``
# backtick list is the common shape — and never from an earlier sentence or
# across a paragraph break. A wrapped line does not end a sentence, so a
# citation list that runs over several lines still works.
#
# The split is deliberately crude (`e.g. ` counts as a sentence end). A wrong
# split does not silently re-point anything: the continuation loses its scope
# and the run goes red, which is a loud "cannot resolve", not a guess.
SENTENCE_END_RE = re.compile(r"[.!?](?=\s|$)")

SNIPPET_LEN = 60

# The lock proves that cited bytes have not changed; it cannot prove that the
# bytes support the sentence. A `SEMANTIC_BINDINGS` entry pins one audited claim:
# the paragraph is found by its subject matter, and every `requirements` entry
# must be satisfied by a citation *inside that paragraph*. Each requirement names
# the file the evidence has to live in and the source names that file's cited
# range must contain, so the binding is about content rather than line numbers.
#
# A claim may need more than one requirement. "the `lastActiveOrg` cookie
# second, and one `GET /api/organizations` third" names a cookie constant and a
# route literal that live in different files, and pinning only the file the
# request leg happens to sit in would let the claim decay quietly: the digest in
# citations.lock would keep matching bytes that no longer say either thing.
SEMANTIC_BINDINGS = [
    {
        "doc": "docs-dev/threat-model.md",
        # Match the bound claim by its subject and endpoint, not one exact
        # verb phrase: prose may say "reads" or "fetches", and Markdown may
        # wrap the phrase across lines.
        "claim_markers": ("access token", "/api/auth/session"),
        "requirements": [
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("readSessionToken", "CHATGPT_SESSION_PATH", ".accessToken"),
            },
        ],
    },
    # W600 · The read-only-probe claim names the flag constant and both URI
    # spellings. All three sit below the policy comment, so a citation that
    # stopped at the comment cited prose about the intent and none of the
    # mechanism it describes.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("Harness session stores are opened read-only", "SQLITE_OPEN_READ_ONLY"),
        "requirements": [
            {
                "target": "crates/chat-stasher/src/sqlite_probe.rs",
                "source_markers": ("SQLITE_OPEN_READ_ONLY", "mode=ro&immutable=1", 'mode=ro'),
            },
        ],
    },
    # W600 · "its one other process boundary is runtime.sendNativeMessage to
    # the pinned host name" is two facts in two places: the pinned name and the
    # one call that uses it. Citing only the constant left the transport claim
    # resting on a range that never mentions it.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("process boundary", "runtime.sendNativeMessage"),
        "requirements": [
            {
                "target": "apps/extension/lib/native-host.ts",
                "source_markers": ("NATIVE_HOST_NAME", "com.chat_stasher.host"),
            },
            {
                "target": "apps/extension/lib/native-host.ts",
                "source_markers": ("runtime.sendNativeMessage", "NATIVE_HOST_NAME"),
            },
        ],
    },
    # W600 · The content-fingerprint claim names the volatile field it strips.
    # `safe_urls` is in the VOLATILE_KEYS table, directly above the function the
    # citation used to name, so the range held the algorithm and not the field.
    {
        "doc": "docs-dev/privacy.md",
        "claim_markers": ("known volatile field", "safe_urls", "content fingerprint"),
        "requirements": [
            {
                "target": "apps/extension/lib/recapture.ts",
                "source_markers": ("VOLATILE_KEYS", "safe_urls", "sha256Hex"),
            },
        ],
    },
    # W600 · The Claude organization order names a cookie and a route. The
    # request leg this claim cited builds its URL from a `resolvePath` variable,
    # so neither literal appeared in it; the cookie name and the resolver are in
    # claude-org.ts and the route literal is CLAUDE_RESOLVE_PATH in enumerate.ts.
    {
        "doc": "docs-dev/privacy.md",
        "claim_markers": ("lastActiveOrg", "/api/organizations"),
        "requirements": [
            {
                "target": "apps/extension/lib/backfill/claude-org.ts",
                "source_markers": ("CLAUDE_ORG_COOKIE", "lastActiveOrg"),
            },
            {
                "target": "apps/extension/lib/backfill/claude-org.ts",
                "source_markers": ("resolveClaudeOrg", "orgFromCookie"),
            },
            {
                "target": "apps/extension/lib/backfill/enumerate.ts",
                "source_markers": ("CLAUDE_RESOLVE_PATH", "/api/organizations"),
            },
        ],
    },
    # W820 · The no-project-endpoint claim rests on two mechanisms in two
    # files: the default port that throws instead of fetching, and the
    # admission check that names the things it refuses. Neither was wrong —
    # but nothing stopped this sentence from being re-cited at only one of
    # them, which would hold true bytes and let the other default rot
    # unnoticed, so both legs are pinned.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("only outbound HTTP port", "checkBackfillRequest"),
        "requirements": [
            {
                "target": "apps/extension/lib/backfill/engine.ts",
                "source_markers": ("notWiredHttp", "refused to fetch"),
            },
            {
                "target": "apps/extension/lib/backfill/tab-port.ts",
                "source_markers": ("checkBackfillRequest", "url is not same-origin with the page"),
            },
        ],
    },
    # W820 · The dashboard row cited `bind_ephemeral`'s signature line alone
    # for "it listens on 127.0.0.1" — the address is the line *below* the one
    # cited — and its gate sentence had no anchor pairing the constant-time
    # compare with the method refusal it sits beside. Both legs now name the
    # mechanism: the bind of `Ipv4Addr::LOCALHOST`, and `ct_eq` within the
    # method gate.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("Loopback is not a security boundary", "constant-time comparison"),
        "requirements": [
            {
                "target": "crates/chat-stasher/src/view.rs",
                "source_markers": ("bind_ephemeral", "Ipv4Addr::LOCALHOST"),
            },
            {
                "target": "crates/chat-stasher/src/view.rs",
                "source_markers": ("ct_eq", 'method != "GET"'),
            },
        ],
    },
    # W820 · The master-key modes were cited as one 71-line span holding the
    # rationale comment and the whole write path together. A span wider than
    # SEMANTIC_MAX_LINES cannot prove content, and no bounded range carried
    # the mode mechanism, so the claim is pinned by two: the "mode is set
    # when the file is created" rationale, and the two mode values with the
    # create-with-mode call.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("plaintext JSON", "0700"),
        "requirements": [
            {
                "target": "crates/chat-stasher/src/store.rs",
                "source_markers": ("0o700", "0o600", "options.mode(0o600)"),
            },
            {
                "target": "crates/chat-stasher/src/store.rs",
                "source_markers": ("The mode is set *when the file is created*",),
            },
        ],
    },
    # W820 · The threat-model copy of the Kimi claim carried the same gap as
    # the privacy one below: a wrapper cited for a token *source* its range
    # never named — `localStorage.access_token` appeared in no anchor, and
    # the reader lived in a file the sentence did not mention.
    {
        "doc": "docs-dev/threat-model.md",
        "claim_markers": ("localStorage.access_token", "holds no copy"),
        "requirements": [
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("KIMI_ACCESS_TOKEN_STORAGE_KEY", "'access_token'"),
            },
            {
                "target": "apps/extension/entrypoints/dw-bridge.content.ts",
                "source_markers": ("localStorage", "KIMI_ACCESS_TOKEN_STORAGE_KEY"),
            },
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("needsKimiBearer", "options.readToken()"),
            },
        ],
    },
    # W820 · W346 pinned the threat-model copy of the session-token claim and
    # left this, the privacy.md copy, outside that round. Three legs: the read
    # of `/api/auth/session`; the gate that limits the bearer header to
    # ChatGPT's list/body paths — the mechanism behind "never attached to any
    # other request"; and the module's token rules, the only lines that say
    # "never storage, IndexedDB, logs, or anything sent to the native host"
    # — which the old citation never contained.
    {
        "doc": "docs-dev/privacy.md",
        "claim_markers": ("access token", "/api/auth/session"),
        "requirements": [
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("readSessionToken", "CHATGPT_SESSION_PATH", ".accessToken"),
            },
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("needsChatgptBearer", "CHATGPT_LIST_PATH", "CHATGPT_DETAIL_PATH"),
            },
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("Rules for the token", "never storage, IndexedDB, logs"),
            },
        ],
    },
    # W820 · The Kimi sentence names where the token lives — the page origin's
    # `localStorage` under `access_token` — but both its anchors sat in the
    # wrapper that attaches it: the storage key's literal was never cited, and
    # the re-read-once/retry code sat below the cited range. Three legs: the
    # key constant, the reader in the bridge, and the attach gate beside the
    # once-only retry.
    {
        "doc": "docs-dev/privacy.md",
        "claim_markers": ("Kimi", "access_token", "localStorage"),
        "requirements": [
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("KIMI_ACCESS_TOKEN_STORAGE_KEY", "'access_token'"),
            },
            {
                "target": "apps/extension/entrypoints/dw-bridge.content.ts",
                "source_markers": ("localStorage", "KIMI_ACCESS_TOKEN_STORAGE_KEY"),
            },
            {
                "target": "apps/extension/lib/platform-auth.ts",
                "source_markers": ("needsKimiBearer", "options.readToken()", "first.status !== 401"),
            },
        ],
    },
]

# The widest a bound citation may be and still count as "an implementation
# range" rather than a pointer at a whole file. A range this size is not
# evidence: it can contain the marker by accident.
SEMANTIC_MAX_LINES = 40


def die(msg: str, code: int = 2) -> None:
    print(f"[citation-lock] {msg}", file=sys.stderr)
    sys.exit(code)


def build_basename_index() -> dict[str, list[str]]:
    """仓库内 basename -> 相对路径列表, 用于解析 `main.rs:37` 这种省略路径的引用。"""
    index: dict[str, list[str]] = {}
    for root, dirs, files in os.walk(REPO):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS and not d.startswith(".")]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), REPO)
            index.setdefault(name, []).append(rel)
    return index


class Citation:
    __slots__ = ("doc", "doc_line", "raw", "target", "start", "end")

    def __init__(self, doc: str, doc_line: int, raw: str, target: str, start: int, end: int):
        self.doc = doc
        self.doc_line = doc_line
        self.raw = raw
        self.target = target
        self.start = start
        self.end = end

    @property
    def key(self) -> str:
        if self.start == self.end:
            return f"{self.target}:{self.start}"
        return f"{self.target}:{self.start}-{self.end}"

    @property
    def where(self) -> str:
        return f"{self.doc}:{self.doc_line}"


def is_url_tail(before_token: str, token: str) -> bool:
    """Whether `token` is what follows `scheme://` in a URL, not a file name.

    `PATH_TOKEN_RE` cannot match the colon, so in `http://x:8080` the token is
    `//x` and in `https://host/a.ts:8080` it is `//host/a.ts`. Both look like
    paths to a shape test; neither names a file.

    The test is deliberately narrow — a `/`-leading token whose character
    before it is a colon. A path written after a word and a colon still reads as
    a path (`see:a/b.rs:3`), which is why the leading `/` is required rather
    than the colon alone.
    """
    return token.startswith("/") and before_token.endswith(":")


def is_path_shaped(token: str, basenames: dict[str, list[str]]) -> bool:
    """Whether a token written before a `:N` is shaped like a file path.

    Three shapes qualify, and only these three:

      * it contains `/` — a path, not a bare name (`not-a-real/dir.rs`);
      * it ends in an extension the checker knows (`main.rs`, `a.ts`);
      * it names a file that exists in the repository, whatever its extension
        (`.gitignore`, `LICENSE`, `Makefile`, a script under `scripts/hooks/`).

    Everything else is ordinary inline code that happens to precede a number:
    `HH:23` is a time, `std::fmt:5` is a Rust path, and both would otherwise be
    forced through resolve_target() and reported as "not a file in this
    repository" — a red gate for a document that only mentioned them.

    🔴 This asks about *shape*, never about resolvability. A path-shaped token
    that does not resolve is still a hard failure (W35): `example.com:8080` is
    not a citation, but `not-a-real/dir.rs:3` is one and the file is missing.
    Deciding by "did it resolve" would turn exactly the wrong way: the citation
    that should be loudest (a path to a file nobody can find) would go quiet.
    """
    if "/" in token:
        return True
    if os.path.splitext(token)[1].lstrip(".") in CITED_EXTS:
        # A path group in CITATION_RE normally catches these first; this branch
        # is what keeps that path branch must-resolve in one place.
        return True
    return token in basenames


def _worktree_file(rel: str) -> bool:
    """Whether `rel` is a file in the working tree the checker is reading."""
    return os.path.isfile(os.path.join(REPO, rel))


def resolve_target(
    token: str,
    last_target: str | None,
    basenames: dict[str, list[str]],
    exists: Callable[[str], bool] | None = None,
) -> tuple[str | None, str | None]:
    """Resolve a cited path token to a repo-relative file, or say why it cannot be.

    Returns (target, problem); exactly one of the two is set.

    A file that exists in the repository is a citation even when its name has no
    extension at all — `.gitignore`, `LICENSE`, `Makefile`, `Dockerfile`, a
    script under `scripts/hooks/`. The extension used to be the whole test, so
    `.gitignore:15` was not a path, matched as a bare `:15`, and inherited
    whatever file the previous citation named.

    🔴 A token that cannot be resolved is a problem, never a fallback. Guessing
    a file for it is how a typo becomes a wrong anchor that still validates.

    🔴 `exists` is the tree the resolution happens *in*, and it defaults to the
    working tree. It exists because `relocate-citations.py` parses a parent's
    document too, and the sentence "what file does this token name" has a
    different answer in a tree that has different files: with a root `a.ts` at
    the parent and only `pkg/a.ts` after the merge, `a.ts:1-2` named one file
    then and a different one now. Resolving the parent's document against the
    working tree answers the merged tree's question and calls it the parent's,
    which is how a citation came to be moved onto a file the sentence never
    named. Callers that read a committed document pass that commit's own tree —
    both this predicate and a `basenames` index built from it.
    """
    is_file = exists if exists is not None else _worktree_file
    if "/" in token:
        if is_file(token):
            return token, None
        return None, f"names `{token}`, which is not a file in this repository"
    # Bare file name: the repo root, then the file the previous citation used,
    # then a name that is unique in the repo.
    if is_file(token):
        return token, None
    if last_target is not None and os.path.basename(last_target) == token:
        return last_target, None
    hits = basenames.get(token, [])
    if len(hits) == 1:
        return hits[0], None
    if not hits:
        return None, f"names `{token}`, which is not a file in this repository"
    return None, (
        f"names `{token}`, a name shared by {len(hits)} files, so it cannot be "
        f"resolved: {', '.join(sorted(hits))}"
    )


def parse_text(
    doc: str,
    lines: list[str],
    basenames: dict[str, list[str]],
    exists: Callable[[str], bool] | None = None,
) -> tuple[list[Citation], list[str]]:
    """把一份文档的正文行解析成 (引用列表, 无法解析的问题列表)。

    这是唯一的解析实现: parse_docs() 拿工作树里的文件调它, 而
    scripts/relocate-citations.py 拿 **某个提交版本** 的同一份文档调它 —— 它必须
    知道某个 parent 的文档到底写了哪些引用, 才能判断一条引用是不是那个 parent
    的坐标系。两份实现会漂移, 而漂移出来的那份就是「这条引用到底是不是引用」的
    第二个答案。

    🔴 文件名解析走这里的 resolve_target(): 省略路径的 `main.rs:3` 落在它真正
    指的那个文件上, 而一个被多个文件共用的名字会返回问题而不是猜一个。裸 `:N`
    继承的是**同一句话里**上一条引用所指的文件, 所以 `a.ts:1`, `:2` 的第二段
    只属于 a.ts —— 它不是对每个文件第 2 行的断言。
    """
    citations: list[Citation] = []
    problems: list[str] = []

    last_target: str | None = None  # the file a bare `:N` inherits in this sentence
    # Absolute offsets in the document where inheritance stops: the end of
    # every sentence, and the end of every blank line (a new paragraph).
    # Computed over the whole document rather than line by line, because a
    # sentence that ends *at* a line break must stop the scope just as one
    # that ends mid-line does.
    stops = [m.end() for m in SENTENCE_END_RE.finditer("\n".join(lines))]
    line_base = 0
    for line in lines:
        if not line.strip():
            stops.append(line_base)
        line_base += len(line) + 1
    stops.sort()
    stop = 0
    base = 0
    for lineno, line in enumerate(lines, start=1):
        for span in CODE_SPAN_RE.finditer(line):
            while stop < len(stops) and base + span.start() >= stops[stop]:
                last_target = None
                stop += 1
            text = span.group(1)
            for m in CITATION_RE.finditer(text):
                # The token the author wrote before this colon: the path the
                # regex recognised, or — when it recognised none — the run of
                # path characters sitting directly before the colon.
                token = m.group("path")
                token_start = m.start()
                if token is None:
                    named = PATH_TOKEN_RE.search(text[: m.start()])
                    if named is not None:
                        token = named.group("token")
                        token_start = named.start()
                # A token is a citation only when it is shaped like a path and
                # is not the tail of a URL. Anything else is ordinary inline
                # code (`HH:23`) — it names no file, so there is nothing to
                # resolve. Such a `:N` is then read exactly as a token-less
                # one: it continues a citation from its own sentence if there
                # is one, and otherwise it is not a citation at all (quiet,
                # not red — a document may mention `HH:23` in a sentence that
                # cites nothing).
                not_a_citation = token is not None and (
                    is_url_tail(text[:token_start], token)
                    or not is_path_shaped(token, basenames)
                )
                if not_a_citation:
                    token = None
                if token is not None:
                    # The citation as the author wrote it: from the token's
                    # start, so a path the regex recognised is still reported
                    # as `a.rs:3` and not as the token plus its own tail.
                    raw = text[token_start : m.end()]
                    target, problem = resolve_target(token, last_target, basenames, exists)
                elif last_target is not None:
                    # A bare `:N` inside the sentence of a citation: the
                    # common `\`a.ts:1\`, \`:2\`` list.
                    raw = m.group(0)
                    target, problem = last_target, None
                elif not_a_citation:
                    # Nothing here names a file, and nothing was cited
                    # earlier in the sentence to continue. Not a citation.
                    continue
                else:
                    raw = m.group(0)
                    problems.append(
                        f"{doc}:{lineno}: citation `{raw}` omits the file name, and no "
                        f"citation in the same sentence names one to inherit it from"
                    )
                    continue
                if target is None:
                    problems.append(f"{doc}:{lineno}: citation `{raw}` {problem}")
                    continue
                last_target = target

                for chunk in m.group("spans").split(","):
                    if "-" in chunk:
                        a, b = chunk.split("-", 1)
                        start, end = int(a), int(b)
                    else:
                        start = end = int(chunk)
                    if start < 1 or end < start:
                        problems.append(
                            f"{doc}:{lineno}: citation `{raw}` has an illegal line range {chunk}"
                        )
                        continue
                    citations.append(Citation(doc, lineno, raw, target, start, end))
        base += len(line) + 1

    return citations, problems


def parse_docs(basenames: dict[str, list[str]]) -> tuple[list[Citation], list[str]]:
    """扫描全部文档, 返回 (引用列表, 无法解析的问题列表)。"""
    citations: list[Citation] = []
    problems: list[str] = []

    for doc in doc_files():
        abs_doc = os.path.join(REPO, doc)
        if not os.path.exists(abs_doc):
            problems.append(f"{doc}: document does not exist")
            continue
        with open(abs_doc, "r", encoding="utf-8") as fh:
            lines = fh.read().splitlines()
        doc_citations, doc_problems = parse_text(doc, lines, basenames)
        citations.extend(doc_citations)
        problems.extend(doc_problems)

    return citations, problems


def semantic_binding_problems(
    citations: list[Citation],
    documents: dict[str, str],
    source_lines: Callable[[str], list[str]],
    *,
    require_claims: bool = False,
) -> list[str]:
    """Check the audited claims whose cited range must establish their subject.

    A binding names a document, the markers that find the claim's paragraph, and
    one or more requirements. Each requirement must be met by a citation inside
    that paragraph, so a claim is pinned by what its anchors say rather than by
    the bytes they happened to hold when the lockfile was written.

    🔴 A claim that cannot be found is a problem, not a pass. Dropping the
    paragraph would otherwise delete the claim and its evidence together, and
    the lockfile would go on reporting a clean run for prose that no longer
    exists — `require_claims` is what turns that silence red.
    """
    problems: list[str] = []
    for binding in SEMANTIC_BINDINGS:
        doc = binding["doc"]
        document = documents.get(doc)
        if document is None:
            continue

        lines = document.splitlines()
        paragraphs: list[tuple[int, int, str]] = []
        start = 1
        for index in range(len(lines) + 1):
            if index == len(lines) or not lines[index].strip():
                if start <= index:
                    paragraphs.append((start, index, "\n".join(lines[start - 1:index])))
                start = index + 2

        matches = [
            (first, last)
            for first, last, paragraph in paragraphs
            if all(
                marker.casefold() in re.sub(r"\s+", " ", paragraph).casefold()
                for marker in binding["claim_markers"]
            )
        ]
        if not matches:
            # Relocation and parser fixtures often carry synthetic document
            # sets that do not include this claim. They retain ordinary drift
            # behavior; the binding applies only where the claim is present.
            if require_claims:
                problems.append(
                    f"[semantic citation mismatch] {doc}: claim disappeared or drifted; expected a paragraph "
                    f"containing {', '.join(repr(marker) for marker in binding['claim_markers'])}"
                )
            continue
        if len(matches) != 1:
            problems.append(
                f"[semantic citation mismatch] {doc}: expected one paragraph containing the semantic citation claim; found {len(matches)}"
            )
            continue

        first, last = matches[0]
        claim_citations = [
            citation for citation in citations
            if citation.doc == doc and first <= citation.doc_line <= last
        ]
        missing = [
            requirement
            for requirement in binding["requirements"]
            if not _requirement_met(requirement, claim_citations, source_lines)
        ]
        if missing:
            wanted = "; ".join(
                f"{requirement['target']} containing {', '.join(requirement['source_markers'])}"
                for requirement in missing
            )
            problems.append(
                f"[semantic citation mismatch] {doc}: the claim containing "
                f"{', '.join(repr(marker) for marker in binding['claim_markers'])} must cite "
                f"at an implementation range of at most {SEMANTIC_MAX_LINES} lines in each of: {wanted}"
            )
    return problems


def _requirement_met(
    requirement: dict,
    claim_citations: list[Citation],
    source_lines: Callable[[str], list[str]],
) -> bool:
    """Whether some citation of the claim's paragraph carries this requirement.

    A citation satisfies a requirement when it names the required file, stays
    inside `SEMANTIC_MAX_LINES`, and its range contains every source marker. The
    markers are what make this a semantic check: a range that merely overlaps
    the right neighbourhood is not evidence that it says the thing.
    """
    for citation in claim_citations:
        if citation.target != requirement["target"]:
            continue
        if citation.end - citation.start + 1 > SEMANTIC_MAX_LINES:
            continue
        cited_text = "\n".join(source_lines(citation.target)[citation.start - 1:citation.end])
        if all(marker in cited_text for marker in requirement["source_markers"]):
            return True
    return False


_file_cache: dict[str, list[str]] = {}


def read_lines(rel: str) -> list[str]:
    if rel not in _file_cache:
        with open(os.path.join(REPO, rel), "r", encoding="utf-8", errors="replace") as fh:
            _file_cache[rel] = fh.read().splitlines()
    return _file_cache[rel]


def digest_of(cit: Citation) -> tuple[str | None, str, str]:
    """返回 (摘要哈希 or None, 首个非空行的片段, 出错说明)。"""
    lines = read_lines(cit.target)
    if cit.end > len(lines):
        return None, "", f"行 {cit.end} 越界 ({cit.target} 只有 {len(lines)} 行)"
    body = [ln.strip() for ln in lines[cit.start - 1 : cit.end]]
    digest = hashlib.sha256("\n".join(body).encode("utf-8")).hexdigest()[:12]
    snippet = next((b for b in body if b), "")
    if len(snippet) > SNIPPET_LEN:
        snippet = snippet[: SNIPPET_LEN - 1] + "…"
    return digest, snippet, ""


def collect() -> tuple[dict[str, dict], list[str]]:
    """把引用聚合成 key -> {digest, snippet, lines, cited_by}。"""
    basenames = build_basename_index()
    citations, problems = parse_docs(basenames)
    documents = {}
    for binding in SEMANTIC_BINDINGS:
        doc = binding["doc"]
        path = os.path.join(REPO, doc)
        if os.path.isfile(path):
            with open(path, "r", encoding="utf-8") as handle:
                documents[doc] = handle.read()
    # The required live binding belongs to the full project checkout. Minimal
    # repositories built by relocation self-tests intentionally carry a
    # claimless threat-model document and exercise ordinary citation behavior.
    require_live_bindings = os.path.isfile(os.path.join(REPO, "Cargo.toml"))
    problems.extend(
        semantic_binding_problems(
            citations, documents, read_lines, require_claims=require_live_bindings
        )
    )

    entries: dict[str, dict] = {}
    for cit in citations:
        digest, snippet, err = digest_of(cit)
        if digest is None:
            problems.append(f"{cit.where}: 引用 `{cit.raw}` -> {cit.key} {err}")
            continue
        entry = entries.setdefault(
            cit.key,
            {"digest": digest, "snippet": snippet, "lines": cit.end - cit.start + 1, "cited_by": []},
        )
        entry["cited_by"].append(cit.where)
    return entries, problems


LOCK_HEADER = """\
# docs-dev/citations.lock — 文档出处引用的内容锚点
#
# 由 scripts/check-citation-drift.py 生成。每条引用记录的是【被引行的内容摘要】,
# 不是行号是否越界 —— 代码被编辑后行号还在、内容已经不是当初那段, 就是这里要抓的漂移。
#
# 格式 (每条两行):
#   <被引文件>:<行范围>  <SHA-256 前12位>  lines=<行数>
#     <- <引用它的文档位置>, ...  :: <被引范围内首个非空行的前 60 字符>
#
# 摘要算法: 取该行范围, 逐行去掉首尾空白, 用换行连接, 取 SHA-256 前 12 位。
# (纯缩进调整不报警; 任何实质内容变化都会变哈希。)
#
# 默认模式只校验、不写这个文件。要接受一次真实变化, 显式跑:
#   python3 scripts/check-citation-drift.py --update
"""


def render_lock(entries: dict[str, dict]) -> str:
    out = [LOCK_HEADER]
    for key in sorted(entries, key=sort_key):
        e = entries[key]
        out.append(f"{key}  {e['digest']}  lines={e['lines']}")
        out.append(f"  <- {', '.join(e['cited_by'])}  :: {e['snippet']}")
    out.append("")
    return "\n".join(out)


def sort_key(key: str):
    target, _, spans = key.rpartition(":")
    first = int(spans.split("-")[0])
    return (target, first)


def parse_lock(text: str) -> dict[str, dict]:
    entries: dict[str, dict] = {}
    pending: str | None = None
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith("  <- "):
            if pending is None:
                continue
            body = line[len("  <- ") :]
            cited, _, snippet = body.partition("  :: ")
            entries[pending]["cited_by"] = [c.strip() for c in cited.split(",") if c.strip()]
            entries[pending]["snippet"] = snippet
            pending = None
            continue
        parts = line.split()
        if len(parts) < 3:
            die(f"lockfile 格式无法解析: {line!r}")
        key, digest = parts[0], parts[1]
        nlines = int(parts[2].split("=", 1)[1])
        entries[key] = {"digest": digest, "lines": nlines, "snippet": "", "cited_by": []}
        pending = key
    return entries


def cmd_update(entries: dict[str, dict], problems: list[str]) -> int:
    if problems:
        print("[citation-lock] 存在无法解析的引用, 拒绝生成 lockfile:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1
    with open(LOCK_PATH, "w", encoding="utf-8") as fh:
        fh.write(render_lock(entries))
    total = sum(len(e["cited_by"]) for e in entries.values())
    print(f"[citation-lock] 已写入 {os.path.relpath(LOCK_PATH, REPO)}: "
          f"{len(entries)} 个锚点 / {total} 处引用")
    return 0


def cmd_check(entries: dict[str, dict], problems: list[str]) -> int:
    if not os.path.exists(LOCK_PATH):
        die(f"lockfile 不存在: {os.path.relpath(LOCK_PATH, REPO)} —— 先跑 --update 生成", code=2)
    with open(LOCK_PATH, "r", encoding="utf-8") as fh:
        locked = parse_lock(fh.read())

    failures: list[str] = []

    for p in problems:
        if p.startswith("[semantic citation mismatch]"):
            failures.append(p)
        else:
            failures.append(f"[引用无法解析] {p}")

    for key in sorted(entries, key=sort_key):
        cur = entries[key]
        where = ", ".join(cur["cited_by"])
        if key not in locked:
            failures.append(
                f"[引用漂到了未锁定的位置] {where} 引用 {key}\n"
                f"    lockfile 里没有这个锚点。当前那几行是: {cur['snippet']}\n"
                f"    要么引用被改错了位置, 要么文档新增了引用还没 --update。"
            )
            continue
        exp = locked[key]
        if cur["digest"] != exp["digest"]:
            msg = (
                f"[被引内容变了] {where} 引用 {key}\n"
                f"    原本指向 ({exp['digest']}): {exp['snippet']}\n"
                f"    现在指向 ({cur['digest']}): {cur['snippet']}"
            )
            if cur["snippet"] == exp["snippet"] and cur["lines"] > 1:
                msg += (
                    f"\n    首行没变, 变化发生在这 {cur['lines']} 行范围的内部 —— "
                    f"去 {key} 逐行看, 确认文档那句话是否还成立。"
                )
            failures.append(msg)

    for key in sorted(locked, key=sort_key):
        if key not in entries:
            failures.append(
                f"[锚点已无人引用] {key} ({locked[key]['snippet']})\n"
                f"    lockfile 里锁着它, 但当前文档里没有任何引用指向它。"
            )

    total = sum(len(e["cited_by"]) for e in entries.values())
    if failures:
        print(f"[citation-lock] 校验失败: {len(failures)} 处问题 "
              f"(共 {len(entries)} 个锚点 / {total} 处引用)", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        print("\n如果这是一次有意的改动, 人工确认引用仍然对得上之后再跑 --update。", file=sys.stderr)
        return 1

    print(f"[citation-lock] OK: {len(entries)} 个锚点 / {total} 处引用, 内容摘要全部与 lockfile 一致")
    return 0


def cmd_list(entries: dict[str, dict], problems: list[str]) -> int:
    for key in sorted(entries, key=sort_key):
        e = entries[key]
        print(f"{key}  {e['digest']}  lines={e['lines']}  <- {', '.join(e['cited_by'])}")
    for p in problems:
        print(f"!! {p}")
    return 1 if problems else 0


def main() -> int:
    ap = argparse.ArgumentParser(description="校验文档出处引用是否漂移")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--update", action="store_true", help="重新生成 docs-dev/citations.lock (显式, 默认不做)")
    g.add_argument("--list", action="store_true", help="只打印解析出的引用")
    args = ap.parse_args()

    entries, problems = collect()
    if args.update:
        return cmd_update(entries, problems)
    if args.list:
        return cmd_list(entries, problems)
    return cmd_check(entries, problems)


if __name__ == "__main__":
    sys.exit(main())
