#!/bin/bash
# Move the x0ne-confidential pin to a commit of the public Engine repository.
# Cargo.toml pins x0ne-confidential by rev; Cargo.lock repeats that rev in the package's source line and
# records the package version. A public clone has only the public Engine history, so a rev outside it
# cannot be fetched and `cargo --locked` fails. This rewrites the rev in both files and, from ENGINE_GIT,
# the locked version. It refuses when the crate's tree differs from the pinned commit's (the files that
# differ are listed: moving to other code is a reviewed change, not a repin) unless --accept-code-change
# says that review happened, and when the crate's dependencies changed, since only cargo can relock those.
# Usage: scripts/repin-public.sh --engine <40-hex> [--root <tree>] [--accept-code-change] [--check]
#        ENGINE_GIT: a clone holding that commit (required: the version and dependencies are read there).
#        ENGINE_PRIVATE_GIT: a clone holding the pinned commit, when ENGINE_GIT does not.
#        --check: after the rewrite, `cargo metadata --locked` must resolve the new pin (network, unless a
#        source replacement or CARGO_NET_OFFLINE=true serves it); the files are restored when it does not.
set -euo pipefail
die() { echo "repin-public: $*" >&2; exit 2; }
ROOT=$(cd "$(dirname "$0")/.." && pwd); E=; ACCEPT=0; CHECK=0
USAGE="usage: repin-public.sh --engine <40-hex> [--root <tree>] [--accept-code-change] [--check]  (ENGINE_GIT=<clone> required)"
while [ $# -gt 0 ]; do
  case $1 in
    --engine) E=${2:-}; shift 2 ;;
    --root) ROOT=${2:-}; shift 2 ;;
    --accept-code-change) ACCEPT=1; shift ;;
    --check) CHECK=1; shift ;;
    *) die "$USAGE" ;;
  esac
done
[[ $E =~ ^[0-9a-f]{40}$ ]] || die "$USAGE"
[ -n "${ENGINE_GIT:-}" ] || die "ENGINE_GIT unset: $USAGE"
git -C "$ENGINE_GIT" cat-file -e "$E^{commit}" 2>/dev/null || die "$E is not a commit of ENGINE_GIT=$ENGINE_GIT"
TOML=$ROOT/Cargo.toml; LOCK=$ROOT/Cargo.lock
if [ ! -f "$TOML" ] || [ ! -f "$LOCK" ]; then die "$ROOT: Cargo.toml or Cargo.lock missing"; fi
URL=https://github.com/x0ne-labs/x0ne-Engine

# The pin as written today: exactly one line in each file.
n=$(grep -c "^x0ne-confidential = { git = \"$URL\", rev = \"[0-9a-f]\{40\}\" }$" "$TOML" || true)
[ "$n" = 1 ] || die "$TOML: $n x0ne-confidential pin line(s), one expected"
OLD=$(sed -n "s/^x0ne-confidential = { git = \"[^\"]*\", rev = \"\([0-9a-f]\{40\}\)\" }$/\1/p" "$TOML")
n=$(grep -c "^source = \"git+$URL?rev=$OLD#$OLD\"$" "$LOCK" || true)
[ "$n" = 1 ] || die "$LOCK: $n source line(s) for rev $OLD, one expected"
if [ "$OLD" = "$E" ]; then echo "repin-public: nothing to change, the pin already names $E"; exit 0; fi

# Same code: the x0ne-confidential tree of the new commit is the pinned commit's, object for object.
src=
for g in "$ENGINE_GIT" "${ENGINE_PRIVATE_GIT:-}"; do
  if [ -n "$g" ] && git -C "$g" cat-file -e "$OLD^{commit}" 2>/dev/null; then src=$g; break; fi
done
new_tree=$(git -C "$ENGINE_GIT" rev-parse --verify -q "$E:x0ne-confidential") || die "$E has no x0ne-confidential directory"
if [ -z "$src" ]; then
  [ "$ACCEPT" = 1 ] || die "pinned commit $OLD is in neither ENGINE_GIT nor ENGINE_PRIVATE_GIT: the trees cannot be compared (--accept-code-change to repin anyway)"
  echo "repin-public: pinned commit $OLD not found locally, tree comparison skipped (--accept-code-change)" >&2
else
  old_tree=$(git -C "$src" rev-parse --verify -q "$OLD:x0ne-confidential") || die "$OLD has no x0ne-confidential directory"
  if [ "$old_tree" != "$new_tree" ]; then
    listing() { git -C "$1" ls-tree -r --format='%(objectname) %(path)' "$2:x0ne-confidential"; }
    echo "repin-public: x0ne-confidential differs (blob and path; - pinned $OLD, + $E):" >&2
    diff <(listing "$src" "$OLD") <(listing "$ENGINE_GIT" "$E") | sed -n 's/^< /- /p; s/^> /+ /p' >&2 || true
    [ "$ACCEPT" = 1 ] || die "$E does not carry the pinned crate's code: review the change, then rerun with --accept-code-change (and build and test against it)"
    echo "repin-public: code change accepted (--accept-code-change): build and test against $E before publishing" >&2
  else
    echo "repin-public: x0ne-confidential tree unchanged ($new_tree)"
  fi
fi

# What the public commit declares: workspace version, and the crate's dependency names.
ver=$(git -C "$ENGINE_GIT" show "$E:Cargo.toml" | sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' | head -n1)
[ -n "$ver" ] || die "$E: no [workspace.package] version in Cargo.toml"
want=$(git -C "$ENGINE_GIT" show "$E:x0ne-confidential/Cargo.toml" | sed -n '/^\[dependencies\]/,/^\[/p' \
  | sed -n 's/^\([A-Za-z0-9_-]*\) *=.*/\1/p' | sort | tr '\n' ' ')
have=$(awk -v s="source = \"git+$URL?rev=$OLD#$OLD\"" '
  $0 == s { inb = 1; next }
  inb && /^dependencies = \[/ { ind = 1; next }
  inb && ind && /^\]/ { exit }
  inb && ind { gsub(/[ ",]/, ""); split($0, a, " "); print a[1] }
  inb && /^$/ { exit }' "$LOCK" | sort | tr '\n' ' ')
[ "$want" = "$have" ] || die "x0ne-confidential dependencies changed (locked: ${have:-none}; $E: ${want:-none}): rewrite the pin, then run cargo update -p x0ne-confidential with network"

cp -p "$TOML" "$TOML.repin-bak"; cp -p "$LOCK" "$LOCK.repin-bak"
restore() { mv -f "$TOML.repin-bak" "$TOML"; mv -f "$LOCK.repin-bak" "$LOCK"; }
sed -i "s|^x0ne-confidential = { git = \"$URL\", rev = \"$OLD\" }$|x0ne-confidential = { git = \"$URL\", rev = \"$E\" }|" "$TOML"
# The version line sits right above the source line in the package block.
awk -v s="source = \"git+$URL?rev=$OLD#$OLD\"" -v ns="source = \"git+$URL?rev=$E#$E\"" -v v="version = \"$ver\"" '
  { line[NR] = $0 }
  END {
    for (i = 1; i <= NR; i++) {
      if (line[i] == s) { line[i] = ns; if (line[i-1] ~ /^version = /) line[i-1] = v }
    }
    for (i = 1; i <= NR; i++) print line[i]
  }' "$LOCK" > "$LOCK.repin"
cat "$LOCK.repin" > "$LOCK"; rm -f "$LOCK.repin"
grep -q "^x0ne-confidential = { git = \"$URL\", rev = \"$E\" }$" "$TOML" || { restore; die "$TOML: rewrite failed"; }
grep -q "^source = \"git+$URL?rev=$E#$E\"$" "$LOCK" || { restore; die "$LOCK: rewrite failed"; }
[ "$(grep -c "$OLD" "$TOML" "$LOCK" | awk -F: '{ s += $2 } END { print s }')" = 0 ] || { restore; die "the old rev $OLD is still cited"; }
echo "$TOML: x0ne-confidential rev $OLD -> $E"
echo "$LOCK: x0ne-confidential $ver at $E"
if [ "$CHECK" = 1 ]; then
  if (cd "$ROOT" && cargo metadata --locked --format-version 1 > /dev/null); then
    echo "repin-public: cargo metadata --locked resolves the new pin"
  else
    restore; die "cargo metadata --locked does not resolve $E (not fetchable, or the lock needs more than the pin): files restored"
  fi
fi
rm -f "$TOML.repin-bak" "$LOCK.repin-bak"
[ "$CHECK" = 1 ] || echo "repin-public: next, with network: cargo metadata --locked --format-version 1 >/dev/null && cargo build --locked"
