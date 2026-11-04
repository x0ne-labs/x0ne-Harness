#!/usr/bin/env bash
# Emits a marker, then writes forever. Nothing downstream can read "all" of it.
# The unique tokens let a grader tell apart "saw the start" from "waited for an
# end that never comes".
echo "FLOOD-START-c4f00d"
i=0
while :; do
  printf 'line %d payload aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n' "$i"
  i=$((i + 1))
done
