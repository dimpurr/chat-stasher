#!/usr/bin/env bash
# Rebase this worktree onto a base and relocate its citation anchors.
set -u

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "[rebase-citations] run this inside a git worktree" >&2
  exit 2
}
cd "$ROOT" || exit 2

usage() {
  echo "Usage: bash scripts/dev/rebase-onto-main.sh [--onto <ref>]" >&2
  exit 2
}

ONTO=main
while [ "$#" -gt 0 ]; do
  case "$1" in
    --onto)
      [ "$#" -ge 2 ] || usage
      ONTO=$2
      shift 2
      ;;
    *) usage ;;
  esac
done

if [ -n "$(git status --porcelain=v1)" ]; then
  echo "[rebase-citations] refusing: worktree is dirty" >&2
  exit 1
fi

PRE_SHA="$(git rev-parse HEAD)" || exit 1
ONTO_SHA="$(git rev-parse --verify "${ONTO}^{commit}")" || {
  echo "[rebase-citations] cannot resolve onto ref: $ONTO" >&2
  exit 1
}

ROLLBACK=1
rollback() {
  rc=$?
  if [ "$ROLLBACK" -eq 1 ]; then
    if [ -d "$(git rev-parse --git-path rebase-merge)" ] || [ -d "$(git rev-parse --git-path rebase-apply)" ]; then
      git rebase --abort >/dev/null 2>&1 || true
    fi
    git reset --hard "$PRE_SHA" >/dev/null 2>&1 || true
    echo "[rebase-citations] restored original SHA $PRE_SHA" >&2
  fi
  exit "$rc"
}
trap rollback EXIT

# Compare documentation at the branch tip and onto ref after replacing only
# citation line ranges with a stable marker. Any remaining difference is prose.
python3 - "$ONTO_SHA" "$PRE_SHA" <<'PY'
import re
import subprocess
import sys

base, head = sys.argv[1:]
paths = subprocess.run(
    ["git", "diff", "--name-only", f"{base}...{head}", "--", "README.md", "docs-dev"],
    check=True, text=True, capture_output=True,
).stdout.splitlines()
paths = [p for p in paths if p.endswith(".md") and p != "docs-dev/citations.lock"]
number = re.compile(
    r"(?<![\w./-])(?P<path>[A-Za-z0-9_./-]+\.[A-Za-z0-9]+)?"
    r":(?P<ranges>\d+(?:-\d+)?(?:,\d+(?:-\d+)?)*)(?![\d./A-Za-z-])"
)

def normalized(commit, path):
    result = subprocess.run(
        ["git", "show", f"{commit}:{path}"], text=True,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    if result.returncode:
        return None
    # Citation ranges are written inside Markdown code spans. Replacing just
    # their coordinates keeps filenames and surrounding prose significant.
    return re.sub(r"`([^`\n]*)`", lambda m: "`" + number.sub(
        lambda n: (n.group("path") or "") + ":<lines>", m.group(1)
    ) + "`", result.stdout)

prose = [p for p in paths if normalized(base, p) != normalized(head, p)]
if prose:
    print("[rebase-citations] refusing: branch changes documentation prose; re-apply these files manually:", file=sys.stderr)
    for path in prose:
        print(f"  {path}", file=sys.stderr)
    sys.exit(1)
PY
if [ "$?" -ne 0 ]; then exit 1; fi

git rebase "$ONTO_SHA"
rebase_rc=$?
while [ "$rebase_rc" -ne 0 ]; do
  unmerged="$(git diff --name-only --diff-filter=U)"
  [ -n "$unmerged" ] || {
    echo "[rebase-citations] rebase failed without resolvable conflicts" >&2
    exit 1
  }
  bad=0
  while IFS= read -r path; do
    case "$path" in
      README.md|docs-dev/*.md|docs-dev/citations.lock) ;;
      *)
        echo "[rebase-citations] code or unsupported conflict; aborting: $path" >&2
        bad=1
        ;;
    esac
  done <<EOF
$unmerged
EOF
  [ "$bad" -eq 0 ] || exit 1

  while IFS= read -r path; do
    [ -n "$path" ] || continue
    git checkout --ours -- "$path" || exit 1
    git add -- "$path" || exit 1
  done <<EOF
$unmerged
EOF
  GIT_EDITOR=true git rebase --continue
  rebase_rc=$?
done

python3 scripts/relocate-citations.py --old "$ONTO_SHA"
relocate_rc=$?
if [ "$relocate_rc" -eq 1 ]; then
  ROLLBACK=0
  trap - EXIT
  echo "[rebase-citations] relocation needs human review; tree remains rebased and uncommitted" >&2
  exit 1
elif [ "$relocate_rc" -ne 0 ]; then
  echo "[rebase-citations] citation relocation failed (exit $relocate_rc)" >&2
  exit "$relocate_rc"
fi

python3 scripts/check-citation-drift.py || {
  echo "[rebase-citations] citation drift check failed" >&2
  exit 1
}

git add -- README.md docs-dev
git commit -m "Relocate citations after rebasing onto main" || exit 1
ROLLBACK=0
trap - EXIT
printf '[rebase-citations] rebased %s onto %s; relocation commit %s\n' \
  "$PRE_SHA" "$ONTO_SHA" "$(git rev-parse --short HEAD)"
