#!/usr/bin/env bash
# Register the derived-file merge drivers for this clone.
#
# Merge drivers live in a clone's `.git/config`; git does not read them out of a
# checkout by itself, exactly like the commit-msg hook in scripts/hooks. Run once
# per clone (scripts/dev/ is not enough on its own: a fresh clone, a fresh
# `git clone` of the farm base, and the bootstrap's init.sh must each do it).
# Idempotent — re-running just overwrites the same lines.
#
# `.git/config` is shared by every linked worktree of a clone, so ONE run covers
# the clone and all its worktrees.
#
# WHY THE DRIVER IS INLINE, NOT A SCRIPT PATH. An earlier version registered
# `python3 <toplevel>/scripts/dev/regenerate-merge-drivers.py ...`, with
# <toplevel> taken from `git rev-parse --show-toplevel`. Run from a linked
# worktree that is the worktree's path, and the merge train deletes the worktree
# of every branch it lands: the shared config then pointed at a file that no
# longer existed, `python3` failed, and git treats a failing driver as a conflict
# — so every later merge in the clone conflicted on the two derived files again
# while `git config` still looked correctly registered. A relative path would not
# be safe either (a branch cut before the script existed has no such file). The
# driver therefore needs no file at all: it is a one-line shell command.
#
# WHAT THE DRIVER DOES. `docs-dev/citations.lock` and `docs-dev/output-inventory.txt`
# are RECORDINGS of derived state; two branches that each regenerated them
# disagree on every line, so no textual 3-way mix of %O/%A/%B is ever right. The
# only correct value is a regeneration from the MERGED source tree, which a
# per-file driver cannot see (git runs it before the merged sources are in the
# working tree). So the driver never stops the merge: exiting 0 keeps %A (the
# current side, disposable by construction), and the integration step
# regenerates both files before committing — scripts/dev/rebase-onto-main.sh,
# the merge train's end-of-train regeneration, or CONTRIBUTING.md "Resolving a
# merge" for a direct merge.
set -u

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "[setup-merge-drivers] run this inside a git checkout" >&2
  exit 2
}
cd "$ROOT" || exit 2

# Report success only for a key that was actually set. `set -e` would be the
# shorter way to fail, but it would also make a failed `git config` (a read-only
# .git/config, say) exit without naming which key could not be written — and the
# failure this guards against is a script that prints "registered" and leaves a
# clone without a driver.
register() { # key value
  git config "$1" "$2" || {
    echo "[setup-merge-drivers] could not set $1" >&2
    exit 1
  }
}

driver() { # kind — the whole driver, inline; ': %O %A %B' only consumes the args
  printf '%s' "echo '[merge-regenerate] $1: kept the current side; regenerate from the merged tree before committing' >&2; : %O %A %B"
}

register merge.regenerate-citations.driver "$(driver citations)"
register merge.regenerate-citations.name \
  "regenerate docs-dev/citations.lock from the merged tree"
register merge.regenerate-inventory.driver "$(driver inventory)"
register merge.regenerate-inventory.name \
  "regenerate docs-dev/output-inventory.txt from the merged tree"
# Older versions also wrote a stray `lock = driver` key; it was never read.
git config --unset merge.regenerate-citations.lock 2>/dev/null || true

echo "[setup-merge-drivers] registered regenerate-citations and regenerate-inventory"
echo "[setup-merge-drivers]   (delete with:"
echo "      git config --unset merge.regenerate-citations.driver"
echo "      git config --unset merge.regenerate-inventory.driver )"
