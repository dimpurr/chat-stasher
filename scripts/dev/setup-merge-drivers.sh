#!/usr/bin/env bash
# Register the derived-file merge drivers for this checkout.
#
# Merge drivers live in each clone's `.git/config`; git does not read them out
# of a checkout by itself, exactly like the commit-msg hook in scripts/hooks.
# Run once per clone (the CI/scratch clone used to prove the fix is where they
# take effect). Idempotent — re-running just overwrites the same two lines.
#
# Without this, the two derived files fall back to a textual merge and a branch
# that regenerated them conflicts on every merge (the W257/W269/W271 pattern).
set -u

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "[setup-merge-drivers] run this inside a git checkout" >&2
  exit 2
}
cd "$ROOT" || exit 2

DRIVER="$ROOT/scripts/dev/regenerate-merge-drivers.py"
[ -f "$DRIVER" ] || {
  echo "[setup-merge-drivers] driver missing: $DRIVER" >&2
  exit 1
}

git config merge.regenerate-citations.driver \
  "python3 $DRIVER citations %O %A %B"
git config merge.regenerate-citations.name \
  "regenerate docs-dev/citations.lock from the merged tree"
git config merge.regenerate-inventory.driver \
  "python3 $DRIVER inventory %O %A %B"
git config merge.regenerate-inventory.name \
  "regenerate docs-dev/output-inventory.txt from the merged tree"

echo "[setup-merge-drivers] registered regenerate-citations and regenerate-inventory"
echo "[setup-merge-drivers]   (delete with:"
echo "      git config --unset merge.regenerate-citations.driver"
echo "      git config --unset merge.regenerate-inventory.driver )"