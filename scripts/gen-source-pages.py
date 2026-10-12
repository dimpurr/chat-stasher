#!/usr/bin/env python3
"""gen-source-pages.py — one public doc page per archived source, plus the index.

The pages under `docs/sources/` are TWO documents in one file: a generated
header block between `<!-- BEGIN GENERATED -->` and `<!-- END GENERATED -->`
that states the facts the tool's own data already holds, and a hand-written
body with fixed headings that the generator never touches. The header and the
index (`docs/sources/README.md`, generated whole) are DERIVED artifacts, and
this script is the only thing that writes them.

Three families of source get a page, and each family's facts come from the
same inputs the support matrix is rendered from (`gen-support-matrix.py`, whose
loaders this script reuses — one input, two renderings, no third answer):

  * every local harness and agent platform in
    `crates/chat-stasher/data/harness-registry-v1.json` — `<id>.md`;
  * every web platform in `apps/extension/lib/contract.ts`'s `ALL_PLATFORMS` —
    `web-<id>.md`;
  * every official-export platform the import seam offers
    (`crates/chat-stasher/src/import.rs`'s `TakeoutPlatform` enum) —
    `import-<id>.md`.

Filenames. Harnesses keep their registry id verbatim. Web platforms and
import parsers carry a family prefix, so the same id never lands on two pages:
the registry's `grok` (the CLI) and the extension's `grok` (grok.com) are
different sources with different pages, and `claude` (claude.ai) must not be
mistakable for `claude-code`. The id is still the tail of the filename, so the
running id-rename audit stays a mechanical regeneration, and each page's
generated header states its id in plain text.

Where each header fact comes from:

  id            the family's own id field, verbatim.
  title         display name: `display_name` (harnesses), the first origin's
                host (web), the recorded display name (imports).
  surface       `harness_surface` in `scripts/source-pages/source-pages-data-v1.json`
                for harnesses — an editorial choice, validated as a closed set
                against the registry, so a new harness fails the check until a
                surface is recorded for it; the fixed word `web` for platforms
                and `export` for imports. A harness that declares
                `dimensions.surfaces` in the registry must have every declared
                word present in its recorded surface, or the check fails.
  status        exactly support.md's rules: `gen-support-matrix.harness_status`
                for harnesses and `web_status` for platforms. Imports claim no
                scan status at all; their header shows the parser state read
                out of `import.rs` (`shipped`, `written, not yet offered by
                the command`, or `no parser in this build`) instead — an import
                has no path to support, so the support vocabulary would be a
                borrowed promise.
  last verified the same `verified` / `lastVerified` record support.md renders,
                through the same freshness rule (older than 90 days renders as
                `needs re-check (DATE)`). Imports have no such record anywhere
                in this repository, so their page says "not yet verified end
                to end" — a date is never invented. Machine names are never
                rendered: the records carry at most an OS scope.
  OS cells      the registry's per-OS cells (template, format, confidence,
                status), per source root where the registry declares roots.

The hand-written body's fixed headings, in order: *What it is*, *What we
collect*, *Coverage and completeness*, *Quirks*, *Known issues*, *How to check
it yourself*, *Changelog*. The check requires all seven, in order, after the
generated block; a skeleton page ships with "Not written yet." under each. The
body may say anything else beside those headings — that is the half of the page
this script will not touch.

Usage:
    python3 scripts/gen-source-pages.py                     # print the index
    python3 scripts/gen-source-pages.py --apply            # create/refresh pages and index
    python3 scripts/gen-source-pages.py --check            # verify; exit 1 if stale
    python3 scripts/gen-source-pages.py --selftest        # prove the check can fail

Exit codes: 0 = ok · 1 = stale, missing or unrecognized page(s) · 2 = input error.
"""

from __future__ import annotations

import argparse
from datetime import date
import importlib.util
import json
import os
import re
import sys
import tempfile
from typing import Any
from urllib.parse import urlparse


def _load_support_matrix():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "gen-support-matrix.py")
    spec = importlib.util.spec_from_file_location("support_matrix_gen", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sm = _load_support_matrix()


class SourcePagesError(Exception):
    """A problem that makes the pages unrenderable — never guessed past."""


# --------------------------------------------------------------------------
# Paths and constants
# --------------------------------------------------------------------------

PAGE_DIR_REL = os.path.join("docs", "sources")
INDEX_REL = os.path.join(PAGE_DIR_REL, "README.md")
DATA_REL = os.path.join("scripts", "source-pages", "source-pages-data-v1.json")
IMPORT_RS_REL = os.path.join("crates", "chat-stasher", "src", "import.rs")
IMPORT_MODULE_REL = os.path.join("crates", "chat-stasher", "src", "import")

BEGIN = "<!-- BEGIN GENERATED -->"
END = "<!-- END GENERATED -->"

APPLY_FLAG = "--apply"
CHECK_FLAG = "--check"

# The one flag of this CLI that rewrites the artifacts. Failure texts print
# it, and the self-test reads those texts back and asserts the flag exists —
# advice a CLI does not accept is a dead end (the same guard
# gen-support-matrix.py carries for its own flags).
FIXTURE_FLAGS = {APPLY_FLAG}

FIXED_HEADINGS = (
    "What it is",
    "What we collect",
    "Coverage and completeness",
    "Quirks",
    "Known issues",
    "How to check it yourself",
    "Changelog",
)
NOT_WRITTEN = "Not written yet."
NOT_VERIFIED = "not yet verified end to end"

# The noun every import page's header ends with, read from the Rust side.
PARSER_SHIPPED = "shipped in this build (`chat-stasher import --platform {id}` parses it)"
PARSER_WRITTEN = "parser written, not yet offered by the command"
PARSER_NONE = "no parser in this build"

# Registry `dimensions.surfaces` values and the words this page renders for
# them. The registry spells `app` where a reader sees a desktop application;
# the mapping is closed so an unknown declared word is an error, never a guess.
SURFACE_DIMENSION_WORDS = {
    "cli": "cli",
    "ide": "ide",
    "app": "desktop app",
    "web": "web",
}


def default_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


# --------------------------------------------------------------------------
# Inputs: data file, import seam
# --------------------------------------------------------------------------


def load_page_data(root: str) -> dict[str, Any]:
    path = os.path.join(root, DATA_REL)
    if not os.path.isfile(path):
        raise SourcePagesError(f"source-pages data file not found: {DATA_REL}")
    with open(path, "r", encoding="utf-8") as fh:
        try:
            data = json.load(fh)
        except json.JSONDecodeError as exc:
            raise SourcePagesError(f"{DATA_REL} is not valid JSON: {exc}") from exc
    if data.get("schema_version") != 1:
        raise SourcePagesError(f"{DATA_REL}: schema_version must be 1")
    surfaces = data.get("harness_surface")
    if not isinstance(surfaces, dict) or not surfaces:
        raise SourcePagesError(f"{DATA_REL} has no `harness_surface` object")
    for key, value in surfaces.items():
        if not isinstance(key, str) or not key:
            raise SourcePagesError(f"{DATA_REL}: a `harness_surface` key is empty")
        if not isinstance(value, str) or not value.strip() or "\n" in value:
            raise SourcePagesError(
                f"{DATA_REL}: harness `{key}` surface must be one non-empty line"
            )
    imports = data.get("import_sources")
    if not isinstance(imports, list) or not imports:
        raise SourcePagesError(f"{DATA_REL} has no `import_sources` list")
    seen_imports: set[str] = set()
    for item in imports:
        if not isinstance(item, dict):
            raise SourcePagesError(f"{DATA_REL}: an `import_sources` entry is not an object")
        ident = item.get("id")
        if not isinstance(ident, str) or not ident:
            raise SourcePagesError(f"{DATA_REL}: an `import_sources` entry has no string `id`")
        if ident in seen_imports:
            raise SourcePagesError(f"{DATA_REL}: import source `{ident}` appears twice")
        seen_imports.add(ident)
        if not isinstance(item.get("display_name"), str) or not item["display_name"].strip():
            raise SourcePagesError(
                f"{DATA_REL}: import source `{ident}` has no `display_name`"
            )
        export_file = item.get("export_file")
        if export_file is not None and (
            not isinstance(export_file, str) or not export_file.strip() or "\n" in export_file
        ):
            raise SourcePagesError(
                f"{DATA_REL}: import source `{ident}` `export_file` must be one non-empty line or absent"
            )
    return data


# `TakeoutPlatform` is the closed set the import command offers, and
# `parser_for` is the closed set it can actually parse. Both are load-bearing
# for the import pages, so this reads them out of import.rs rather than keeping
# a third list here. The shape is line-oriented on purpose — the same approach
# `gen-support-matrix.parse_contract_platforms` takes on contract.ts — and a
# file that stopped matching the shape is an input error (exit 2), never a
# silently partial page set.
_ENUM_OPEN_RE = re.compile(r"pub enum TakeoutPlatform \{")
_ENUM_VARIANT_RE = re.compile(r"^\s{4}([A-Z][A-Za-z0-9]*),\s*$")
_SLUG_ARM_RE = re.compile(r"^\s+Self::([A-Za-z0-9]+) => \"([a-z0-9-]+)\",\s*$")
_PARSER_FN_OPEN_RE = re.compile(r"fn parser_for\(")
_PARSER_ARM_RE = re.compile(r"^\s*TakeoutPlatform::([A-Za-z0-9]+)(.*)$")


def parse_import_platforms(root: str) -> list[dict[str, Any]]:
    path = os.path.join(root, IMPORT_RS_REL)
    if not os.path.isfile(path):
        raise SourcePagesError(f"import seam not found: {IMPORT_RS_REL}")
    with open(path, "r", encoding="utf-8") as fh:
        lines = fh.read().splitlines()

    # The enum's variants, in declaration order (the order the pages inherit).
    start = next((i for i, ln in enumerate(lines) if _ENUM_OPEN_RE.search(ln)), None)
    if start is None:
        raise SourcePagesError(f"{IMPORT_RS_REL}: no `pub enum TakeoutPlatform` declaration found")
    variants: list[str] = []
    depth = 0
    for raw in lines[start:]:
        if "{" in raw:
            depth += raw.count("{")
        if "}" in raw:
            depth -= raw.count("}")
            if depth <= 0:
                break
        match = _ENUM_VARIANT_RE.match(raw)
        if match:
            variants.append(match.group(1))
    if not variants:
        raise SourcePagesError(f"{IMPORT_RS_REL}: `TakeoutPlatform` declares no variants")

    def slug_arms() -> dict[str, str]:
        """variant -> platform slug, from the `slug()` match arms."""
        slugs: dict[str, str] = {}
        in_fn = False
        for raw in lines:
            if "pub fn slug(" in raw and "match self" not in raw:
                in_fn = True
                continue
            if not in_fn:
                continue
            if raw.strip() == "}":
                in_fn = False
                continue
            match = _SLUG_ARM_RE.match(raw)
            if match:
                slugs[match.group(1)] = match.group(2)
        if len(slugs) != len(variants):
            missing = [v for v in variants if v not in slugs]
            raise SourcePagesError(
                f"{IMPORT_RS_REL}: `slug()` does not name every TakeoutPlatform variant: {missing}"
            )
        return slugs

    def parser_arms() -> dict[str, bool]:
        """variant -> a parser is wired to the command (`parser_for`)."""
        wired: dict[str, bool] = {}
        fn_start = next(
            (i for i, ln in enumerate(lines) if _PARSER_FN_OPEN_RE.search(ln)), None
        )
        if fn_start is None:
            raise SourcePagesError(f"{IMPORT_RS_REL}: no `fn parser_for` found")
        in_match = False
        for raw in lines[fn_start:]:
            stripped = raw.strip()
            if stripped.startswith("match platform"):
                in_match = True
                continue
            if not in_match:
                continue
            if stripped == "}":
                break
            match = _PARSER_ARM_RE.match(raw)
            if not match:
                continue
            # The arm's variant list: the head variant is the regex's own
            # first group; anything piped beside it (`A | B => None`) still
            # sits in the tail before the arrow.
            named = [match.group(1)] + re.findall(
                r"TakeoutPlatform::([A-Za-z0-9]+)", match.group(2).split("=>")[0]
            )
            arrow = "=> Some(" if "=> Some(" in raw else (
                "=> None" if "=> None" in raw else None
            )
            if arrow is None:
                continue
            for variant in named:
                wired[variant] = arrow == "=> Some("
        for variant in variants:
            if variant not in wired:
                raise SourcePagesError(
                    f"{IMPORT_RS_REL}: `parser_for` does not name variant `{variant}`"
                )
        return wired

    slugs = slug_arms()
    wired = parser_arms()
    out: list[dict[str, Any]] = []
    for variant in variants:
        ident = slugs[variant]
        module_path = os.path.join(root, IMPORT_MODULE_REL, f"{ident}.rs")
        if wired[variant]:
            parser = PARSER_SHIPPED.format(id=ident)
        elif os.path.isfile(module_path):
            parser = PARSER_WRITTEN
        else:
            parser = PARSER_NONE
        out.append({"id": ident, "variant": variant, "parser": parser})
    return out


# --------------------------------------------------------------------------
# The source model
# --------------------------------------------------------------------------


def esc(text: Any) -> str:
    return str(text).replace("|", "\\|").replace("\n", " ")


def code(text: Any) -> str:
    return "`" + str(text).replace("`", "'") + "`"


def url_title(origins: list[str]) -> str:
    host = urlparse(origins[0]).netloc
    if host.startswith("www."):
        host = host[len("www."):]
    return f"{host} (web)"


def verified_detail(record: dict[str, Any] | None, as_of: date) -> str:
    """The page header's full "last verified" sentence.

    A missing record is written out in words, not as a placeholder: the task
    these pages answer is "when was this last verified", and `-` answers a
    different question ("is anything recorded in this cell"). The scope is the
    human-readable evidence for the date, so it travels with it.
    """
    if record is None:
        return NOT_VERIFIED
    cell = sm.last_verified_cell(record, as_of)
    if record.get("platform"):
        cell += f" ({sm.OS_LABEL[record['platform']]})"
    if record.get("version"):
        cell += f" ({record['version']})"
    return f"{cell}: {record['scope']}"


def index_verified_cell(record: dict[str, Any] | None, as_of: date) -> str:
    if record is None:
        return NOT_VERIFIED
    cell = sm.last_verified_cell(record, as_of)
    if record.get("platform"):
        cell += f" ({sm.OS_LABEL[record['platform']]})"
    return cell


def validated_harness_surface(
    h: dict[str, Any], surfaces: dict[str, str], owner: str
) -> str:
    ident = h["id"]
    surface = surfaces.get(ident)
    if surface is None:
        raise SourcePagesError(
            f"{owner}: harness `{ident}` has no `harness_surface` entry in "
            f"{DATA_REL} — record one (any non-empty phrase, e.g. `cli`) and "
            "re-derive the pages"
        )
    dimensions = (h.get("dimensions") or {}).get("surfaces")
    if isinstance(dimensions, list) and dimensions:
        for declared in dimensions:
            word = SURFACE_DIMENSION_WORDS.get(str(declared))
            if word is None:
                raise SourcePagesError(
                    f"{owner}: harness `{ident}` declares unknown `dimensions.surfaces` "
                    f"value `{declared}` (this check knows: {sorted(SURFACE_DIMENSION_WORDS)})"
                )
            if word not in surface:
                raise SourcePagesError(
                    f"{owner}: harness `{ident}` records surface `{surface}` but the "
                    f"registry declares `{declared}` — the page must not claim less "
                    "than the registry"
                )
    return surface


def harness_os_table(h: dict[str, Any]) -> list[str]:
    verified = sm.harness_verified(h)
    roots = sm.harness_root_entries(h)
    multi = bool(roots) and any(name for name, _ in roots)
    header = (
        ["| Source root | OS | Session path template | Format | Confidence | Status |"]
        if multi
        else ["| OS | Session path template | Format | Confidence | Status |"]
    )
    # The separator row carries one `---|` per column; counting pipes off the
    # header line keeps the two expressions from disagreeing about the width.
    columns = header[0].count("|") - 1
    header.append("|" + "---|" * columns)
    rows: list[str] = []
    for root_name, paths in roots:
        for os_name in sm.OS_ORDER:
            cell = paths.get(os_name)
            status = sm.cell_status(cell, verified, os_name)
            prefix = [f"{esc(root_name)}"] if multi else []
            if cell is None:
                rows.append(
                    "| "
                    + " | ".join(prefix + [sm.OS_LABEL[os_name], sm.ABSENT, sm.ABSENT, sm.ABSENT, esc(status)])
                    + " |"
                )
                continue
            rows.append(
                "| "
                + " | ".join(
                    prefix
                    + [
                        sm.OS_LABEL[os_name],
                        code(cell.get("template", "") or sm.ABSENT),
                        esc(cell.get("format", sm.ABSENT) or sm.ABSENT),
                        esc(cell.get("confidence", sm.ABSENT) or sm.ABSENT),
                        esc(status),
                    ]
                )
                + " |"
            )
    return header + rows


def build_header(fam: str, src: dict[str, Any]) -> str:
    out: list[str] = []
    out.append(f"# {src['title']}")
    out.append("")
    if fam == "local":
        verified = sm.harness_verified(src["harness"])
        rows = [
            ("Id", code(src["id"])),
            ("Surface", esc(src["surface"])),
            ("Status", esc(sm.harness_status(src["harness"]))),
            (
                "Last verified end to end",
                esc(verified_detail(verified, src["as_of"])),
            ),
        ]
    elif fam == "web":
        rows = [
            ("Id", code(src["id"])),
            ("Surface", "web"),
            ("Origins", esc(", ".join(src["origins"]))),
            ("Channel", esc(src["channel"])),
            ("Capture credibility", esc(src["credibility"])),
            ("Status", esc(sm.web_status(src["platform"]))),
            (
                "Last verified end to end",
                esc(verified_detail(src["platform"].get("lastVerified"), src["as_of"])),
            ),
        ]
    else:
        rows = [
            ("Id", code(src["id"])),
            ("Surface", "export"),
        ]
        if src["export_file"]:
            rows.append(("Export file", code(src["export_file"])))
        rows.append(("Parser", esc(src["parser"])))
        rows.append(("Last verified end to end", NOT_VERIFIED))
    out.append("| Field | Value |")
    out.append("|---|---|")
    for name, value in rows:
        out.append(f"| {name} | {value} |")
    if fam == "local" and src["extra_table"]:
        out.append("")
        out.extend(src["extra_table"])
    if fam == "web":
        out.append("")
        out.append(src["extra_table"])
    return "\n".join(out)


def build_sources(root: str, as_of: date | None = None) -> list[dict[str, Any]]:
    """Every source that must have a page, in its family's own order."""
    registry_rel = sm.REGISTRY_REL
    harnesses = sm.load_harnesses(root)
    platforms = sm.parse_contract_platforms(root)
    imports = parse_import_platforms(root)
    data = load_page_data(root)
    as_of = sm.resolve_as_of(as_of)

    seen_harness_ids: set[str] = set()
    for h in harnesses:
        if h["id"] in seen_harness_ids:
            raise SourcePagesError(f"{registry_rel}: harness `{h['id']}` appears twice")
        seen_harness_ids.add(h["id"])
    surfaces = data["harness_surface"]
    surface_owner = os.path.join("scripts", "source-pages")
    for key in surfaces:
        if key not in seen_harness_ids:
            raise SourcePagesError(
                f"{DATA_REL}: `harness_surface` names harness `{key}`, "
                "which the registry does not carry (remove the entry)"
            )
    import_ids = {item["id"] for item in imports}
    for item in data["import_sources"]:
        if item["id"] not in import_ids:
            raise SourcePagesError(
                f"{DATA_REL}: `import_sources` names `{item['id']}`, which "
                f"{IMPORT_RS_REL}'s `TakeoutPlatform` does not offer (remove the entry)"
            )
    for ident in sorted(import_ids):
        if ident not in {item["id"] for item in data["import_sources"]}:
            raise SourcePagesError(
                f"{DATA_REL}: import source `{ident}` is offered by "
                f"{IMPORT_RS_REL} but has no `import_sources` entry — a page "
                "must exist for every platform the command names"
            )
    import_by_id = {item["id"]: item for item in data["import_sources"]}

    sources: list[dict[str, Any]] = []
    for h in harnesses:
        src = {
            "family": "local",
            "id": h["id"],
            "page": f"{h['id']}.md",
            "title": h.get("display_name") or h["id"],
            "surface": validated_harness_surface(h, surfaces, surface_owner),
            "harness": h,
            "as_of": as_of,
            "extra_table": harness_os_table(h),
        }
        src["header"] = build_header("local", src)
        sources.append(src)
    for p in platforms:
        src = {
            "family": "web",
            "id": p["id"],
            "page": f"web-{p['id']}.md",
            "title": url_title(p["origins"]),
            "surface": "web",
            "origins": p["origins"],
            "channel": p["channel"],
            "credibility": p["credibility"],
            "platform": p,
            "as_of": as_of,
            # Per-OS availability is not a fact this row could carry: it comes
            # from the browser registration table, which is a different claim
            # by different data. The pages point there instead of inventing
            # three cells that would all say the same thing.
            "extra_table": (
                "OS availability comes from the browser registration table, not from "
                "this row: see the browser grid in [the support tables](../support.md)."
            ),
        }
        src["header"] = build_header("web", src)
        sources.append(src)
    for imported in imports:
        meta = import_by_id[imported["id"]]
        src = {
            "family": "import",
            "id": imported["id"],
            "page": f"import-{imported['id']}.md",
            "title": meta["display_name"],
            "surface": "export",
            "export_file": meta.get("export_file"),
            "parser": imported["parser"],
            "as_of": as_of,
            "extra_table": [],
        }
        src["header"] = build_header("import", src)
        sources.append(src)

    pages: set[str] = set()
    for src in sources:
        if src["page"] in pages:
            raise SourcePagesError(f"two sources map to the same page file: {src['page']}")
        pages.add(src["page"])
    return sources


# --------------------------------------------------------------------------
# Rendering: pages and index
# --------------------------------------------------------------------------


def skeleton_body() -> str:
    parts: list[str] = []
    for name in FIXED_HEADINGS:
        parts.append(f"## {name}")
        parts.append("")
        parts.append(NOT_WRITTEN)
        parts.append("")
    return "\n".join(parts).rstrip("\n")


def render_page_file(src: dict[str, Any], existing_text: str | None) -> str:
    header = src["header"]
    if existing_text is None:
        return f"{BEGIN}\n{header}\n{END}\n\n{skeleton_body()}\n"
    return sm.replace_between(existing_text, BEGIN, END, header)


def render_index(sources: list[dict[str, Any]], as_of: date | None = None) -> str:
    as_of = sm.resolve_as_of(as_of)
    out: list[str] = []
    out.append(
        "<!-- This index is generated by `python3 scripts/gen-source-pages.py --apply` and "
        "must not be edited by hand. -->"
    )
    out.append("")
    out.append("# Sources")
    out.append("")
    out.append(
        "One page per source chat-stasher can archive: what it is, what we collect from "
        "it, how complete that is, its quirks, and when it was last verified end to end. "
        "The block at the top of every page is generated from the same registry and "
        "platform tables as the support tables in [docs/support.md](../support.md), by "
        "the same rules, so a page can never claim more than the tables do. The prose "
        "under the fixed headings is written and reviewed by hand, one page at a time; a "
        "page whose every section says \"Not written yet.\" only carries its generated "
        "facts."
    )
    out.append("")

    def section(heading: str, rows: list[str], legend_note: str = "") -> None:
        out.append(heading)
        out.append("")
        out.append("| Page | Surface | Status | Last verified |")
        out.append("|---|---|---|---|")
        out.extend(rows)
        out.append("")
        if legend_note:
            out.append(legend_note)
            out.append("")

    local_rows: list[str] = []
    web_rows: list[str] = []
    import_rows: list[str] = []
    for src in sources:
        link = f"[{esc(src['title'])}]({src['page']})"
        if src["family"] == "local":
            local_rows.append(
                "| {link} | {surface} | {status} | {when} |".format(
                    link=link,
                    surface=esc(src["surface"]),
                    status=esc(sm.harness_status(src["harness"])),
                    when=esc(index_verified_cell(sm.harness_verified(src["harness"]), as_of)),
                )
            )
        elif src["family"] == "web":
            web_rows.append(
                "| {link} | web | {status} | {when} |".format(
                    link=link,
                    status=esc(sm.web_status(src["platform"])),
                    when=esc(index_verified_cell(src["platform"].get("lastVerified"), as_of)),
                )
            )
        else:
            import_rows.append(
                "| {link} | export | {parser} | {not_verified} |".format(
                    link=link,
                    parser=esc(src["parser"]),
                    not_verified=NOT_VERIFIED,
                )
            )

    section(
        "## Local harnesses and agent platforms",
        local_rows,
        "The Status column uses exactly the words and rules of "
        "[docs/support.md](../support.md): `verified end-to-end (DATE)` is a real session "
        "archived on a real machine with the date recorded, `supported` claims the path "
        "or route and not the end to end run.",
    )
    section(
        "## Web AI chats (browser extension)",
        web_rows,
        "These are the platforms the browser extension captures; the row for each is "
        "read from the extension's own platform table. `Last verified` for a platform is "
        "the date of a live acceptance run of the build the record names.",
    )
    section(
        "## Official exports (import)",
        import_rows,
        "An official export (a takeout you request from the platform) is the platform's "
        "own statement of what an account holds. The Parser column is read from the "
        "import seam itself: `shipped` means `chat-stasher import --platform <id>` "
        "parses the export in this build, and no row claims a scan or capture. No "
        "official export has a recorded end-to-end run yet, so each page says so rather "
        "than carrying a date.",
    )
    out.append(
        f"A recorded verification has a shelf life: a date older than {sm.RECHECK_DAYS} days "
        f"is shown as `{sm.RECHECK_LABEL} (DATE)`, exactly as in the support tables, and "
        "the same day the generator fails its own check until the pages are re-derived. "
        "A source with no record says `"
        + NOT_VERIFIED
        + "`, which is a statement about "
        "what is recorded, never about whether the source works."
    )
    out.append("")
    return "\n".join(out)


# --------------------------------------------------------------------------
# Check and apply
# --------------------------------------------------------------------------


def page_paths(root: str) -> str:
    return os.path.join(root, PAGE_DIR_REL)


def check(root: str, as_of: date | None = None) -> tuple[list[str], list[str]]:
    """Return (failures, notes). A failure is always an exit-1 condition."""
    sources = build_sources(root, as_of)
    by_page = {src["page"]: src for src in sources}
    failures: list[str] = []
    notes: list[str] = []

    index_path = os.path.join(root, INDEX_REL)
    if not os.path.isfile(index_path):
        failures.append(f"{INDEX_REL}: index is missing (run {APPLY_FLAG})")
    else:
        with open(index_path, "r", encoding="utf-8") as fh:
            actual = fh.read()
        if sm.normalize(actual) != sm.normalize(render_index(sources, as_of)):
            failures.append(f"{INDEX_REL}: index is stale - regenerate with {APPLY_FLAG}")

    directory = page_paths(root)
    on_disk: set[str] = set()
    if os.path.isdir(directory):
        for name in sorted(os.listdir(directory)):
            # README.md is the index, checked separately above; every other
            # Markdown file in this directory must be a page some source owns.
            if name.endswith(".md") and name != os.path.basename(INDEX_REL):
                on_disk.add(name)
    expected = set(by_page)
    for page in sorted(expected - on_disk):
        failures.append(f"{os.path.join(PAGE_DIR_REL, page)}: page is missing (run {APPLY_FLAG})")
    for page in sorted(on_disk - expected):
        failures.append(
            f"{os.path.join(PAGE_DIR_REL, page)}: no source owns this page (the registry, "
            f"the extension contract and the import seam name no such source; delete the file - "
            "the generator never deletes by itself)"
        )
    for page in sorted(expected & on_disk):
        rel = os.path.join(PAGE_DIR_REL, page)
        path = os.path.join(directory, page)
        with open(path, "r", encoding="utf-8") as fh:
            text = fh.read()
        src = by_page[page]
        body_offset = text.find(END)
        first_line = next((ln for ln in text.splitlines() if ln.strip()), "")
        if first_line.strip() != BEGIN:
            failures.append(f"{rel}: the generated block does not open the page (move prose below {END})")
        content, problem = sm.extract_between(text, BEGIN, END)
        if problem is not None:
            failures.append(f"{rel}: {problem}")
            continue
        if content is None:
            failures.append(f"{rel}: no {BEGIN} / {END} pair at all (run {APPLY_FLAG})")
            continue
        if sm.normalize(content) != sm.normalize(src["header"]):
            failures.append(
                f"{rel}: generated header is stale - regenerate with {APPLY_FLAG}"
            )
        if body_offset == -1:
            continue
        body = text[body_offset + len(END):]
        positions: list[int] = []
        for name in FIXED_HEADINGS:
            at = body.find(f"\n## {name}\n")
            if at == -1 and body.startswith(f"## {name}\n"):
                at = 0
            if at == -1:
                failures.append(f"{rel}: fixed heading `## {name}` is missing from the body")
                positions.append(-1)
                continue
            positions.append(at)
        known = [p for p in positions if p != -1]
        if len(known) > 1 and known != sorted(known):
            failures.append(f"{rel}: the fixed headings are out of order")
    return failures, notes


def run_check(root: str, as_of: date | None = None) -> int:
    try:
        failures, notes = check(root, as_of)
    except SourcePagesError as exc:
        print(f"[source-pages] cannot render the pages: {exc}", file=sys.stderr)
        return 2
    for note in notes:
        print(f"[source-pages] note: {note}")
    if failures:
        print(f"[source-pages] FAILED: {len(failures)} page problem(s)", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        print(
            "[source-pages] the registry, the extension contract, the import seam or the "
            f"hand-written bodies moved; re-derive with {APPLY_FLAG} deliberately.",
            file=sys.stderr,
        )
        return 1
    print(
        "[source-pages] OK: every source page, its generated header and the index match "
        "the registry, the extension contract and the import seam"
    )
    return 0


def apply(root: str, as_of: date | None = None) -> int:
    try:
        sources = build_sources(root, as_of)
    except SourcePagesError as exc:
        print(f"[source-pages] cannot render the pages: {exc}", file=sys.stderr)
        return 2
    directory = page_paths(root)
    os.makedirs(directory, exist_ok=True)
    for src in sources:
        path = os.path.join(directory, src["page"])
        rel = os.path.join(PAGE_DIR_REL, src["page"])
        existing = None
        if os.path.isfile(path):
            with open(path, "r", encoding="utf-8") as fh:
                existing = fh.read()
            if src["id"] not in existing:
                # A file is not the page its name claims: the hand-written body
                # may belong to a different source entirely, so refuse rather
                # than graft a mismatched header onto it.
                print(
                    f"[source-pages] refusing {rel}: the file exists but does not name "
                    f"source `{src['id']}` - resolve by hand "
                    "(rename the file to match its content, or update the registry id)",
                    file=sys.stderr,
                )
                return 2
        new_text = render_page_file(src, existing)
        if existing == new_text:
            print(f"[source-pages] {rel}: already up to date")
            continue
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(new_text)
        print(
            f"[source-pages] wrote {rel}"
            if existing is None
            else f"[source-pages] refreshed the generated header in {rel}"
        )
    index_path = os.path.join(root, INDEX_REL)
    with open(index_path, "w", encoding="utf-8") as fh:
        fh.write(render_index(sources, as_of))
    print(f"[source-pages] wrote {INDEX_REL}")
    return 0


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

SELFTEST_AS_OF = date(2026, 10, 12)
SELFTEST_STALE = "2026-07-01"  # 103 days before SELFTEST_AS_OF: past the threshold
SELFTEST_FRESH = "2026-09-15"

_SELFTEST_REGISTRY = {
    "schema_version": 1,
    "generated": "2026-01-01",
    "browsers": [
        {
            "id": "b-chrome",
            "display_name": "B-Chrome",
            "source": "https://example.com/b-chrome",
            "tiers": {
                "macos": {"tier": "supported"},
                "linux": {"tier": "supported"},
                "windows": {"tier": "supported"},
            },
        }
    ],
    "harnesses": [
        {
            "id": "alpha",
            "display_name": "Alpha",
            "dev_priority": "normal",
            "verified": {
                "date": SELFTEST_FRESH,
                "version": "9.9.9",
                "scope": "one real session archived end to end on a real machine",
            },
            "paths": {
                "macos": {
                    "template": "~/.alpha/<uuid>.jsonl",
                    "format": "jsonl",
                    "confidence": "source-confirmed",
                    "source": "https://example.com/alpha",
                }
            },
        },
        {
            # The collision the filename rule exists for: a harness whose id
            # is also a web platform's id gets the bare name; the platform
            # gets the `web-` prefix, so two sources never share one page.
            "id": "p",
            "display_name": "P Harness",
            "paths": {
                "macos": {
                    "template": "~/.p/",
                    "format": "sqlite",
                    "confidence": "official-docs",
                    "source": "https://example.com/p",
                }
            },
        },
        {
            "id": "scoped",
            "display_name": "Scoped",
            "verified": {
                "date": SELFTEST_FRESH,
                "platform": "macos",
                "scope": "verified on macOS only",
            },
            "source_roots": [
                {
                    "id": "one",
                    "paths": {
                        "macos": {
                            "template": "~/scoped/one/",
                            "format": "jsonl",
                            "confidence": "measured-locally",
                            "source": "measured",
                        }
                    },
                },
                {
                    "id": "two",
                    "paths": {
                        "macos": {
                            "template": "~/scoped/two/",
                            "format": "jsonl",
                            "confidence": "unascertained",
                            "source": "not read",
                        }
                    },
                },
            ],
        },
    ],
}

_SELFTEST_CONTRACT = """\
export const ALL_PLATFORMS: readonly ChatPlatform[] = [
  {
    id: 'p',
    origins: ['https://p.example'],
    devPriority: 'normal',
    lastVerified: {
      date: '2026-09-15',
      version: 'ext 0.1.0.1',
      scope: 'one real conversation on a real page',
    },
    credibility: 'from-source',
    channel: 'stable',
  },
  {
    id: 'stale-web',
    origins: ['https://www.stale.example'],
    lastVerified: {
      date: '2026-07-01',
      scope: 'an old acceptance run',
    },
    credibility: 'from-source',
    channel: 'stable',
  },
  {
    id: 'plain',
    origins: ['https://plain.example'],
    credibility: 'unverified',
    channel: 'experimental',
  },
];
"""

_SELFTEST_IMPORT_RS = """\
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TakeoutPlatform {
    X,
    Y,
    Z,
}

impl TakeoutPlatform {
    pub fn slug(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::Y => "y",
            Self::Z => "z",
        }
    }
    pub fn has_parser(self) -> bool {
        parser_for(self).is_some()
    }
}

fn parser_for(platform: TakeoutPlatform) -> Option<fn(&[u8]) -> Result<ParsedExport, ImportError>> {
    match platform {
        TakeoutPlatform::X => Some(parse_x_export),
        TakeoutPlatform::Y | TakeoutPlatform::Z => None,
    }
}
"""

# Y has a parser module on disk but no wired arm: "written, not yet offered".
# Z has neither: "no parser in this build". X is wired: "shipped".

_SELFTEST_DATA = {
    "schema_version": 1,
    "harness_surface": {"alpha": "cli", "p": "cli", "scoped": "cli"},
    "import_sources": [
        {"id": "x", "display_name": "X (official export)", "export_file": "x.json"},
        {"id": "y", "display_name": "Y (official export)", "export_file": "y.json"},
        {"id": "z", "display_name": "Z (official export)", "export_file": None},
    ],
}


def _scaffold(root: str) -> None:
    reg_path = os.path.join(root, sm.REGISTRY_REL)
    os.makedirs(os.path.dirname(reg_path), exist_ok=True)
    with open(reg_path, "w", encoding="utf-8") as fh:
        json.dump(_SELFTEST_REGISTRY, fh, indent=2)
    con_path = os.path.join(root, sm.CONTRACT_REL)
    os.makedirs(os.path.dirname(con_path), exist_ok=True)
    with open(con_path, "w", encoding="utf-8") as fh:
        fh.write(_SELFTEST_CONTRACT)
    rs_path = os.path.join(root, IMPORT_RS_REL)
    os.makedirs(os.path.dirname(rs_path), exist_ok=True)
    with open(rs_path, "w", encoding="utf-8") as fh:
        fh.write(_SELFTEST_IMPORT_RS)
    module_dir = os.path.join(root, IMPORT_MODULE_REL)
    os.makedirs(module_dir, exist_ok=True)
    with open(os.path.join(module_dir, "y.rs"), "w", encoding="utf-8") as fh:
        fh.write("//! unwired parser module\n")
    data_path = os.path.join(root, DATA_REL)
    os.makedirs(os.path.dirname(data_path), exist_ok=True)
    with open(data_path, "w", encoding="utf-8") as fh:
        json.dump(_SELFTEST_DATA, fh, indent=2)


def selftest() -> int:
    failures = 0
    total = 0

    def probe(name: str, ok: bool) -> None:
        nonlocal failures, total
        total += 1
        if ok:
            print(f"[selftest]   PASS · {name}")
        else:
            print(f"[selftest]   FAIL · {name}")
            failures += 1

    def tree_check(root: str) -> int:
        return run_check(root, SELFTEST_AS_OF)

    with tempfile.TemporaryDirectory(prefix="source-pages-selftest-") as tmp:
        _scaffold(tmp)
        sources = build_sources(tmp, SELFTEST_AS_OF)
        by_id = {(s["family"], s["id"]): s for s in sources}

        probe(
            "every family contributes its own pages",
            sorted(s["page"] for s in sources)
            == sorted(
                [
                    "alpha.md",
                    "p.md",
                    "scoped.md",
                    "web-p.md",
                    "web-stale-web.md",
                    "web-plain.md",
                    "import-x.md",
                    "import-y.md",
                    "import-z.md",
                ]
            ),
        )
        probe(
            "a harness id and a web platform id never share one page",
            by_id["local", "p"]["page"] == "p.md"
            and by_id["web", "p"]["page"] == "web-p.md",
        )
        probe(
            "the import family page names carry the platform slugs",
            [by_id["import", k]["page"] for k in ("x", "y", "z")]
            == ["import-x.md", "import-y.md", "import-z.md"],
        )
        probe(
            "parser states follow parser_for and module presence",
            "shipped in this build" in by_id["import", "x"]["header"]
            and "not yet offered by the command" in by_id["import", "y"]["header"]
            and "no parser in this build" in by_id["import", "z"]["header"],
        )
        probe(
            "a page without an export file renders no Export file row",
            "Export file" not in by_id["import", "z"]["header"],
        )
        probe(
            "verified detail carries version and scope",
            "2026-09-15 (9.9.9): one real session archived end to end on a real machine"
            in by_id["local", "alpha"]["header"],
        )
        probe(
            "an OS-scoped record names its OS",
            "(macOS): verified on macOS only" in by_id["local", "scoped"]["header"],
        )
        probe(
            "a source with no record says so in words, never a placeholder",
            f"| Last verified end to end | {NOT_VERIFIED} |" in by_id["local", "p"]["header"],
        )
        probe(
            "a stale verification renders the re-check label with its date",
            f"{sm.RECHECK_LABEL} ({SELFTEST_STALE})" in by_id["web", "stale-web"]["header"],
        )
        probe(
            "the multi-root harness renders a Source root column",
            "| Source root | OS |" in by_id["local", "scoped"]["header"]
            and "| two | macOS |" in by_id["local", "scoped"]["header"],
        )
        probe(
            "the web title derives from the first origin's host",
            by_id["web", "stale-web"]["title"] == "stale.example (web)",
        )

        expected_pages = {
            "alpha.md", "p.md", "scoped.md", "web-p.md", "web-stale-web.md",
            "web-plain.md", "import-x.md", "import-y.md", "import-z.md",
        }

        # --apply creates every page and the index, and the check then passes.
        probe("apply creates the tree", apply(tmp, SELFTEST_AS_OF) == 0)
        probe(
            "every expected page landed on disk",
            {
                name
                for name in os.listdir(page_paths(tmp))
                if name.endswith(".md") and name != "README.md"
            }
            == expected_pages,
        )
        probe("fresh pages pass the check", tree_check(tmp) == 0)

        # Applying twice must change nothing: idempotence is what keeps the
        # hand-written half safe week after week.
        before = {
            name: open(os.path.join(page_paths(tmp), name), "rb").read()
            for name in expected_pages
        }
        probe("second apply is a no-op", apply(tmp, SELFTEST_AS_OF) == 0)
        after = {
            name: open(os.path.join(page_paths(tmp), name), "rb").read()
            for name in expected_pages
        }
        probe("no byte changed on the second apply", before == after)

        # A hand-written body survives regeneration untouched.
        page = os.path.join(page_paths(tmp), "alpha.md")
        with open(page, "r", encoding="utf-8") as fh:
            text = fh.read()
        marker = "## What it is\n\nThis is a hand-written sentence that must survive.\n"
        hand_body = text[text.find(END) + len(END):]
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(text.replace("## What it is\n\nNot written yet.\n", marker))
        probe("apply refreshes the header", apply(tmp, SELFTEST_AS_OF) == 0)
        with open(page, "r", encoding="utf-8") as fh:
            text2 = fh.read()
        probe(
            "the hand-written body survives regeneration byte for byte",
            marker in text2 and "Not written yet.\n\n## What we collect" not in text2
            and text2[text2.find(END) + len(END):] == hand_body.replace(
                "## What it is\n\nNot written yet.\n", marker
            ),
        )
        probe("the refreshed tree still passes", tree_check(tmp) == 0)

        # Every way a page can be wrong must fail the check, and name the file.
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(text2.replace("| Id | `alpha` |", "| Id | `changed` |"))
        probe("a corrupted header fails", tree_check(tmp) == 1)
        os.remove(page)
        probe("a missing page fails and is named", any("alpha.md" in f for f in check(tmp, SELFTEST_AS_OF)[0]))
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(text2)
        with open(os.path.join(page_paths(tmp), "orphan.md"), "w", encoding="utf-8") as fh:
            fh.write("# Orphan\n")
        failures_orphan = check(tmp, SELFTEST_AS_OF)[0]
        probe(
            "a page no source owns fails and is named",
            any("orphan.md" in f for f in failures_orphan),
        )
        os.remove(os.path.join(page_paths(tmp), "orphan.md"))
        with open(page, "r", encoding="utf-8") as fh:
            whole = fh.read()
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(whole.replace("## Quirks\n", "## Renamed section\n", 1))
        probe("a missing fixed heading fails", tree_check(tmp) == 1)
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(whole)
        # Out of order: move the Changelog heading above What it is.
        reordered = whole.replace("## Changelog\n", "", 1)
        reordered = reordered.replace("## What it is\n", "## Changelog\n\nMoved.\n\n## What it is\n", 1)
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(reordered)
        probe("reordered fixed headings fail", tree_check(tmp) == 1)
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(f"Prose before the block.\n\n{BEGIN}\nwrong\n{END}\n\n")
            fh.write(skeleton_body() + "\n")
        probe("prose above the generated block fails", tree_check(tmp) == 1)
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(f"{BEGIN}\nno end marker\n")
        probe("a half marker pair fails loudly", tree_check(tmp) == 1)
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(whole)

        # A stale index and a moved input both fail.
        index_path = os.path.join(tmp, INDEX_REL)
        with open(index_path, "r", encoding="utf-8") as fh:
            index_text = fh.read()
        with open(index_path, "w", encoding="utf-8") as fh:
            fh.write(index_text.replace("| cli |", "| CLI |", 1))
        probe("a stale index fails", tree_check(tmp) == 1)
        with open(index_path, "w", encoding="utf-8") as fh:
            fh.write(index_text)
        changed = json.loads(json.dumps(_SELFTEST_REGISTRY))
        changed["harnesses"][0]["paths"]["macos"]["template"] = "~/.changed/<uuid>.jsonl"
        with open(os.path.join(tmp, sm.REGISTRY_REL), "w", encoding="utf-8") as fh:
            json.dump(changed, fh, indent=2)
        probe("a changed registry fails the check", tree_check(tmp) == 1)
        with open(os.path.join(tmp, sm.REGISTRY_REL), "w", encoding="utf-8") as fh:
            json.dump(_SELFTEST_REGISTRY, fh, indent=2)
        probe("restored, the check passes again", tree_check(tmp) == 0)

        # Input errors are exit 2, never a rendered guess.
        bad_data = json.loads(json.dumps(_SELFTEST_DATA))
        del bad_data["harness_surface"]["p"]
        with open(os.path.join(tmp, DATA_REL), "w", encoding="utf-8") as fh:
            json.dump(bad_data, fh, indent=2)
        probe("a harness without a recorded surface is an error", tree_check(tmp) == 2)
        bad_data = json.loads(json.dumps(_SELFTEST_DATA))
        bad_data["import_sources"].append(
            {"id": "no-such-platform", "display_name": "Ghost (official export)", "export_file": "g.json"}
        )
        with open(os.path.join(tmp, DATA_REL), "w", encoding="utf-8") as fh:
            json.dump(bad_data, fh, indent=2)
        probe("an import source the command does not offer is an error", tree_check(tmp) == 2)
        bad_data = json.loads(json.dumps(_SELFTEST_DATA))
        bad_data["import_sources"].pop(0)
        with open(os.path.join(tmp, DATA_REL), "w", encoding="utf-8") as fh:
            json.dump(bad_data, fh, indent=2)
        probe("a platform the command offers without a data entry is an error", tree_check(tmp) == 2)
        with open(os.path.join(tmp, DATA_REL), "w", encoding="utf-8") as fh:
            json.dump(_SELFTEST_DATA, fh, indent=2)
        scoped_dimensions = json.loads(json.dumps(_SELFTEST_REGISTRY))
        scoped_dimensions["harnesses"][1]["dimensions"] = {"surfaces": ["app"]}
        with open(os.path.join(tmp, sm.REGISTRY_REL), "w", encoding="utf-8") as fh:
            json.dump(scoped_dimensions, fh, indent=2)
        probe(
            "a recorded surface missing a registry-declared word is an error",
            tree_check(tmp) == 2,
        )
        with open(os.path.join(tmp, sm.REGISTRY_REL), "w", encoding="utf-8") as fh:
            json.dump(_SELFTEST_REGISTRY, fh, indent=2)
        with open(os.path.join(tmp, IMPORT_RS_REL), "w", encoding="utf-8") as fh:
            fh.write(_SELFTEST_IMPORT_RS.replace('Self::X => "x",', ""))
        probe("an import seam the parser cannot read is an error", tree_check(tmp) == 2)
        os.remove(os.path.join(tmp, IMPORT_RS_REL))
        probe("a missing import seam is an error", tree_check(tmp) == 2)
        _scaffold(tmp)
        probe("restored inputs pass the check again", tree_check(tmp) == 0)

        # apply refuses to graft a mismatched header onto someone else's page.
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(f"{BEGIN}\n# Beta\n\n| Field | Value |\n|---|---|\n| Id | `beta` |\n{END}\n\n")
            fh.write(skeleton_body() + "\n")
        probe("apply refuses a page that names another source", apply(tmp, SELFTEST_AS_OF) == 2)
        # Restore the real page before the flags probe, above: the tree the
        # flags probe corrupts must otherwise be the tree the check is happy
        # with again once restored.
        with open(page, "w", encoding="utf-8") as fh:
            fh.write(whole)
        probe("the restored tree passes the check", tree_check(tmp) == 0)

        # Advice a failure text prints is only advice if the CLI accepts it.
        _scaffold(tmp)
        apply(tmp, SELFTEST_AS_OF)
        os.remove(page)
        with open(os.path.join(page_paths(tmp), "orphan.md"), "w", encoding="utf-8") as fh:
            fh.write("# Orphan\n")
        printed_flags = {
            flag
            for text in check(tmp, SELFTEST_AS_OF)[0]
            for flag in re.findall(r"--[a-z][a-z-]*", text)
        }
        probe(
            "the failure texts reach every remediation flag",
            APPLY_FLAG in printed_flags,
        )
        probe(
            "every remediation flag the check prints is a flag the CLI accepts",
            printed_flags <= cli_option_strings(),
        )

    if failures:
        print(f"[selftest] FAILED: {failures}/{total} probe(s) did not hold")
        return 1
    print(f"[selftest] PASS: all {total} probes held")
    return 0


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="Render and check the per-source doc pages under docs/sources/"
    )
    ap.add_argument("--root", default=default_root(), help="repository root (default: this checkout)")
    ap.add_argument("--apply", action="store_true", help="create missing pages, refresh headers, rewrite the index")
    ap.add_argument("--check", action="store_true", help="verify committed pages; exit 1 if stale")
    ap.add_argument("--selftest", action="store_true", help="prove the check can fail")
    return ap


def cli_option_strings() -> set[str]:
    return {s for action in build_parser()._actions for s in action.option_strings}


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    root = os.path.abspath(args.root)
    try:
        if args.selftest:
            return selftest()
        if args.apply:
            return apply(root)
        if args.check:
            return run_check(root)
        sys.stdout.write(
            render_index(build_sources(root), None)
        )
        return 0
    except SourcePagesError as exc:
        print(f"[source-pages] {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
