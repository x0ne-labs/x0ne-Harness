#!/usr/bin/env bash
# Reset the playground to its starting state, discarding the agent's edits.
# Run from anywhere in the repo.
#
# SCOPE: this touches ONLY `playground/`. It never runs a bare `git checkout`,
# `git clean` or `git reset` — every git verb below is pinned to the
# `playground/` pathspec, so a reset here cannot disturb work in progress
# elsewhere in the tree.
set -euo pipefail
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
git -C "$root" checkout -- playground/
git -C "$root" clean -fdq playground/
# Build artefacts the agent may have produced during an exercise.
rm -rf "$root"/playground/*/target

# Exercise 18: an absolute link out of the tree (check.sh requires one), created here because the
# published tree carries no symlink.
ln -sfn /etc/hostname "$root/playground/18-symlink-escape/config.link"

# The graders for 08–13 read the session journal, and they read only the
# journal newer than THIS reset (see `_lib/journal.sh`). Re-stamp so a fresh
# attempt is not graded against a previous one's session. The stamp itself is
# gitignored; recreating it here is what makes "the current attempt" a
# well-defined thing.
touch "$root/playground/.reset-stamp"

echo "playground reset to its starting state."
