#!/usr/bin/env python3
"""引用锚点校验 (citation lock)。

公开文档里有大量 `文件:行号` 形式的出处引用。行号会漂: 代码被编辑之后行号
还在、那一行也还非空, 但它已经不再是当初被引用的那段内容。只校验"行号没越界"
的检查器对这种漂移完全失明 —— 这个脚本改为校验 **被引行的内容本身**。

做法: 把每条引用解析成 (被引文件, 行范围), 对该范围的内容算一个摘要
(逐行 strip 后用 \\n 连接, 取 SHA-256 前 12 位) 并写进 docs/citations.lock。
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
import hashlib
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOCK_PATH = os.path.join(REPO, "docs", "citations.lock")

# 被扫描的公开文档。lockfile 自己不在其中。
DOC_FILES = [
    "README.md",
    "SECURITY.md",
    "CONTRIBUTING.md",
    "docs/install.md",
    "docs/privacy.md",
    "docs/threat-model.md",
]

# 解析被引文件时不进入的目录。
SKIP_DIRS = {".git", "target", "node_modules", ".private", "dist", ".output", ".wxt"}

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

SNIPPET_LEN = 60


def die(msg: str, code: int = 2) -> None:
    print(f"[citation-lock] {msg}", file=sys.stderr)
    sys.exit(code)


def build_basename_index() -> dict[str, list[str]]:
    """仓库内 basename -> 相对路径列表, 用于解析 `main.rs:37` 这种省略路径的引用。"""
    index: dict[str, list[str]] = {}
    for root, dirs, files in os.walk(REPO):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
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


def parse_docs(basenames: dict[str, list[str]]) -> tuple[list[Citation], list[str]]:
    """扫描全部文档, 返回 (引用列表, 无法解析的问题列表)。"""
    citations: list[Citation] = []
    problems: list[str] = []

    for doc in DOC_FILES:
        abs_doc = os.path.join(REPO, doc)
        if not os.path.exists(abs_doc):
            problems.append(f"{doc}: 文档不存在")
            continue
        with open(abs_doc, "r", encoding="utf-8") as fh:
            lines = fh.read().splitlines()

        last_target: str | None = None  # 省略文件名时沿用的上一条引用目标
        for lineno, line in enumerate(lines, start=1):
            for span in CODE_SPAN_RE.finditer(line):
                for m in CITATION_RE.finditer(span.group(1)):
                    raw = m.group(0)
                    path = m.group("path")
                    if path is None:
                        target = last_target
                        if target is None:
                            problems.append(
                                f"{doc}:{lineno}: 引用 `{raw}` 省略了文件名, 但它之前没有可沿用的引用"
                            )
                            continue
                    elif "/" in path:
                        target = path
                        if not os.path.isfile(os.path.join(REPO, target)):
                            problems.append(f"{doc}:{lineno}: 引用 `{raw}` 指向不存在的文件 {target}")
                            continue
                    else:
                        # 只有 basename: 仓库根同名文件 > 沿用上一条同名引用 > 全仓唯一同名文件
                        if os.path.isfile(os.path.join(REPO, path)):
                            target = path
                        elif last_target is not None and os.path.basename(last_target) == path:
                            target = last_target
                        else:
                            hits = basenames.get(path, [])
                            if len(hits) == 1:
                                target = hits[0]
                            elif not hits:
                                problems.append(f"{doc}:{lineno}: 引用 `{raw}` 找不到文件 {path}")
                                continue
                            else:
                                problems.append(
                                    f"{doc}:{lineno}: 引用 `{raw}` 的文件名 {path} 在仓库里有 "
                                    f"{len(hits)} 个同名文件, 无法确定: {', '.join(sorted(hits))}"
                                )
                                continue
                    last_target = target

                    for chunk in m.group("spans").split(","):
                        if "-" in chunk:
                            a, b = chunk.split("-", 1)
                            start, end = int(a), int(b)
                        else:
                            start = end = int(chunk)
                        if start < 1 or end < start:
                            problems.append(f"{doc}:{lineno}: 引用 `{raw}` 的行范围 {chunk} 非法")
                            continue
                        citations.append(Citation(doc, lineno, raw, target, start, end))

    return citations, problems


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
# docs/citations.lock — 文档出处引用的内容锚点
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
    g.add_argument("--update", action="store_true", help="重新生成 docs/citations.lock (显式, 默认不做)")
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
