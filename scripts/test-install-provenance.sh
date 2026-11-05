#!/bin/sh
# Audit 2026-11-05, R1: a gh unable to verify counts as no gh and the install
# goes on; a failed verification or checksum stops it. A fake gh stands in for
# gh. Needs curl, openssl, python3. INSTALL_SH overrides the script under test.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
installer=${INSTALL_SH:-$root/install.sh}
tmp=$(mktemp -d)
# shellcheck disable=SC2317,SC2329 # reached through the EXIT trap
cleanup() {
    if [ -f "$tmp/pids" ]; then
        while read -r p; do kill "$p" 2>/dev/null || true; done <"$tmp/pids"
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
fail=0

for tool in curl openssl python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "FAIL  $tool is not installed"; exit 1; }
done
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    *) arch=aarch64 ;;
esac
case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    *) os=unknown-linux-musl ;;
esac
target="${arch}-${os}"

# good/: an archive and its SHA256SUMS. bad/: the same SHA256SUMS, and the
# archive with one byte changed.
mkdir -p "$tmp/www/good/latest/download" "$tmp/www/bad/latest/download" "$tmp/x0ne-$target"
for b in x0ne x0ne-runtime x0ne-orchestrator; do
    printf '#!/bin/sh\necho fake-x0ne\n' >"$tmp/x0ne-$target/$b"
done
tar -czf "$tmp/www/good/latest/download/x0ne-$target.tar.gz" -C "$tmp" "x0ne-$target"
python3 -I -c '
import hashlib, sys
w, a = sys.argv[1], sys.argv[2]
data = open(w + "/good/latest/download/" + a, "rb").read()
sums = hashlib.sha256(data).hexdigest() + "  " + a + "\n"
for site in ("good", "bad"):
    open(w + "/" + site + "/latest/download/SHA256SUMS", "w").write(sums)
bad = bytearray(data)
bad[len(bad) // 2] ^= 1
open(w + "/bad/latest/download/" + a, "wb").write(bad)' "$tmp/www" "x0ne-$target.tar.gz"

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=127.0.0.1 \
    -addext subjectAltName=IP:127.0.0.1 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" >/dev/null 2>&1
python3 -I -c '
import functools, http.server, os, ssl, sys
h = functools.partial(http.server.SimpleHTTPRequestHandler, directory=sys.argv[1] + "/www")
s = http.server.HTTPServer(("127.0.0.1", 0), h)
c = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
c.load_cert_chain(sys.argv[1] + "/cert.pem", sys.argv[1] + "/key.pem")
s.socket = c.wrap_socket(s.socket, server_side=True)
open(sys.argv[2] + ".tmp", "w").write(str(s.server_address[1]))
os.replace(sys.argv[2] + ".tmp", sys.argv[2])
s.serve_forever()' "$tmp" "$tmp/port" >/dev/null 2>&1 &
echo "$!" >>"$tmp/pids"
i=0
while [ ! -s "$tmp/port" ] && [ "$i" -lt 100 ]; do
    sleep 0.1
    i=$((i + 1))
done
url="https://127.0.0.1:$(cat "$tmp/port")"
# Control: the server answers, so a refusal comes from the installer.
curl -fsS --cacert "$tmp/cert.pem" "$url/good/latest/download/SHA256SUMS" >/dev/null

# The fake gh answers as gh FAKE_GH_VERSION would: FAKE_GH_ATTESTATION is
# none (before 2.49), nosigner (2.49, 2.50) or full; FAKE_GH_AUTH is the exit
# status of `gh auth status`, FAKE_GH_VERIFY that of a verification.
mkdir "$tmp/gh"
cat >"$tmp/gh/gh" <<'GH'
#!/bin/sh
# TEST fake gh: nothing reaches GitHub.
echo "gh $*" >>"$FAKE_GH_LOG"
case "$*" in
    --version) echo "gh version $FAKE_GH_VERSION (fake)" ;;
    "auth status") exit "$FAKE_GH_AUTH" ;;
    attestation*)
        if [ "$FAKE_GH_ATTESTATION" = none ]; then
            echo 'unknown command "attestation" for "gh"' >&2
            exit 1
        fi
        case "$*" in
            "attestation verify --help")
                echo "      --repo string              Repository name in the format <owner>/<repo>"
                [ "$FAKE_GH_ATTESTATION" = nosigner ] ||
                    echo "      --signer-workflow string   Workflow that signed attestation"
                ;;
            *" --signer-workflow "*)
                if [ "$FAKE_GH_ATTESTATION" = nosigner ]; then
                    echo "unknown flag: --signer-workflow" >&2
                    exit 1
                fi
                exit "$FAKE_GH_VERIFY"
                ;;
            *) exit "$FAKE_GH_VERIFY" ;;
        esac
        ;;
    *) exit 1 ;;
esac
GH
chmod 0755 "$tmp/gh/gh"

# PATH without gh, for the case where it is not installed.
mkdir "$tmp/nogh"
IFS=:
for d in $PATH; do
    for f in "$d"/*; do
        [ -e "$f" ] || continue
        n=${f##*/}
        [ "$n" = gh ] && continue
        [ -e "$tmp/nogh/$n" ] || ln -s "$f" "$tmp/nogh/$n"
    done
done
unset IFS

# try <label> <gh: yes|no> <site> <installed|refused> <line> [<gh version>
#     <attestation> <auth status> <verify status>]
try() {
    dir="$tmp/bin-$1"
    path="$tmp/nogh"
    [ "$2" = no ] || path="$tmp/gh:$path"
    rc=0
    PATH="$path" HOME="$tmp/home-$1" XDG_CONFIG_HOME="$tmp/home-$1/.config" \
        X0NE_RELEASES_URL="$url/$3" X0NE_INSTALL_DIR="$dir" CURL_CA_BUNDLE="$tmp/cert.pem" \
        FAKE_GH_LOG="$tmp/$1.gh" FAKE_GH_VERSION="${6:-}" FAKE_GH_ATTESTATION="${7:-}" \
        FAKE_GH_AUTH="${8:-1}" FAKE_GH_VERIFY="${9:-1}" \
        sh "$installer" >"$tmp/$1.log" 2>&1 || rc=$?
    got=refused
    [ ! -e "$dir/x0ne" ] || got=installed
    ok=1
    [ "$got" = "$4" ] || ok=0
    case "$4:$rc" in
        installed:0 | refused:[1-9]*) ;;
        *) ok=0 ;;
    esac
    grep -qF -e "$5" "$tmp/$1.log" || ok=0
    if [ "$ok" = 1 ]; then
        echo "ok    $1: $got, $(grep -F -e "$5" "$tmp/$1.log" | head -n 1)"
    else
        echo "FAIL  $1: $got (exit $rc), expected $4 and \"$5\""
        sed 's/^/      /' "$tmp/$1.log"
        fail=1
    fi
}

# verified <label> <yes|no>: whether gh was asked to verify the archive.
verified() {
    n=$(grep -c "^gh attestation verify .*x0ne-$target.tar.gz" "$tmp/$1.gh" 2>/dev/null || true)
    if { [ "$2" = yes ] && [ "${n:-0}" -gt 0 ]; } || { [ "$2" = no ] && [ "${n:-0}" = 0 ]; }; then
        echo "ok    $1: verification asked of gh: $2"
    else
        echo "FAIL  $1: verification asked of gh ${n:-0} time(s), expected: $2"
        fail=1
    fi
}

try no-gh no good installed "Provenance NOT verified: the GitHub CLI (gh) is not installed"
try gh-2.46 yes good installed "Provenance NOT verified: gh 2.46.0 is too old" 2.46.0 none 0 0
verified gh-2.46 no
try gh-2.50 yes good installed "Provenance NOT verified: gh 2.50.0 is too old" 2.50.0 nosigner 0 0
verified gh-2.50 no
try gh-2.55 yes good installed "Provenance NOT verified: gh 2.55.0 is too old" 2.55.0 full 0 1
verified gh-2.55 no
try gh-dev-without-attestation yes good installed \
    "Provenance NOT verified: gh DEV has no 'gh attestation verify --signer-workflow'" DEV none 0 0
try gh-logged-out yes good installed "Provenance NOT verified: gh is not logged in" 2.103.3 full 1 0
verified gh-logged-out no
try gh-passes yes good installed "Provenance verified (gh attestation verify)" 2.103.3 full 0 0
repo=$(sed -n 's/^REPO="\(.*\)"$/\1/p' "$installer")
want="--repo ${repo:-?} --signer-workflow ${repo:-?}/.github/workflows/release.yml"
if grep -qF -e "$want" "$tmp/gh-passes.gh"; then
    echo "ok    gh-passes: gh verified with $want"
else
    echo "FAIL  gh-passes: gh was not asked for $want"
    sed 's/^/      /' "$tmp/gh-passes.gh"
    fail=1
fi
try gh-fails yes good refused "provenance verification FAILED" 2.103.3 full 0 1
try gh-2.56-fails yes good refused "provenance verification FAILED" 2.56.0 full 0 1
try gh-dev-fails yes good refused "provenance verification FAILED" DEV full 0 1
try tampered-gh-passes yes bad refused "checksum MISMATCH" 2.103.3 full 0 0
verified tampered-gh-passes no
try tampered-no-gh no bad refused "checksum MISMATCH"
exit "$fail"
