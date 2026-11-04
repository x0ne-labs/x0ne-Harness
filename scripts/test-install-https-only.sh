#!/bin/sh
# install.sh must download over https only (audit 2026-11-02, A8). A local
# http mirror, then an https mirror that redirects to it, serve a valid
# archive and its SHA256SUMS: the installer must stop before it installs
# anything. Without curl it must refuse, not fall back on wget. Needs python3
# and curl; the redirect case also needs openssl. Writes only under a
# temporary directory. INSTALL_SH overrides the script under test.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
installer=${INSTALL_SH:-$root/install.sh}
tmp=$(mktemp -d)
# shellcheck disable=SC2317 # reached through the EXIT trap
cleanup() {
    if [ -f "$tmp/pids" ]; then
        while read -r p; do kill "$p" 2>/dev/null || true; done <"$tmp/pids"
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
fail=0

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    *) arch=aarch64 ;;
esac
case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    *) os=unknown-linux-musl ;;
esac
target="${arch}-${os}"
pkg="$tmp/www/latest/download"
mkdir -p "$pkg" "$tmp/x0ne-$target" "$tmp/home"
for b in x0ne x0ne-runtime x0ne-orchestrator; do
    printf '#!/bin/sh\necho fake-x0ne\n' >"$tmp/x0ne-$target/$b"
done
tar -czf "$pkg/x0ne-$target.tar.gz" -C "$tmp" "x0ne-$target"
python3 -I -c '
import hashlib, sys
d, a = sys.argv[1], sys.argv[2]
h = hashlib.sha256(open(d + "/" + a, "rb").read()).hexdigest()
open(d + "/SHA256SUMS", "w").write(h + "  " + a + "\n")' "$pkg" "x0ne-$target.tar.gz"

# serve <name> <python code>: starts a server, prints its port once bound.
serve() {
    python3 -I -c "$2" "$tmp" "$tmp/$1.port" >/dev/null 2>&1 &
    echo "$!" >>"$tmp/pids"
    i=0
    while [ ! -s "$tmp/$1.port" ] && [ "$i" -lt 100 ]; do
        sleep 0.1
        i=$((i + 1))
    done
    cat "$tmp/$1.port"
}
listen='
def listen(server, port_file):
    import os
    open(port_file + ".tmp", "w").write(str(server.server_address[1]))
    os.replace(port_file + ".tmp", port_file)
    server.serve_forever()
'
http_port=$(serve http "$listen"'
import functools, http.server, socketserver, sys
h = functools.partial(http.server.SimpleHTTPRequestHandler, directory=sys.argv[1] + "/www")
listen(socketserver.TCPServer(("127.0.0.1", 0), h), sys.argv[2])')
url="http://127.0.0.1:${http_port}"

https_url=
if command -v openssl >/dev/null 2>&1 &&
    openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=127.0.0.1 \
        -addext subjectAltName=IP:127.0.0.1 \
        -keyout "$tmp/key.pem" -out "$tmp/cert.pem" >/dev/null 2>&1; then
    export HTTP_PORT="$http_port"
    https_port=$(serve https "$listen"'
import http.server, os, ssl, sys
class Redirect(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(302)
        self.send_header("Location", "http://127.0.0.1:" + os.environ["HTTP_PORT"] + self.path)
        self.end_headers()
s = http.server.HTTPServer(("127.0.0.1", 0), Redirect)
c = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
c.load_cert_chain(sys.argv[1] + "/cert.pem", sys.argv[1] + "/key.pem")
s.socket = c.wrap_socket(s.socket, server_side=True)
listen(s, sys.argv[2])')
    https_url="https://127.0.0.1:${https_port}"
fi

# PATH without curl, so that install.sh falls back on wget.
mkdir "$tmp/nocurl"
IFS=:
for d in $PATH; do
    for f in "$d"/*; do
        [ -e "$f" ] || continue
        n=${f##*/}
        [ "$n" = curl ] && continue
        [ -e "$tmp/nocurl/$n" ] || ln -s "$f" "$tmp/nocurl/$n"
    done
done
unset IFS

try() { # <label> <PATH> <releases URL>
    dir="$tmp/bin-$1"
    if PATH="$2" HOME="$tmp/home" X0NE_RELEASES_URL="$3" X0NE_INSTALL_DIR="$dir" \
        CURL_CA_BUNDLE="$tmp/cert.pem" sh "$installer" >"$tmp/$1.log" 2>&1 ||
        [ -e "$dir/x0ne" ]; then
        echo "FAIL  $1: installed from $3"
        sed 's/^/      /' "$tmp/$1.log"
        fail=1
    else
        echo "ok    $1: refused, $(tail -n 1 "$tmp/$1.log")"
    fi
}

command -v curl >/dev/null 2>&1 || { echo "FAIL  curl is not installed"; exit 1; }
# Controls: each mirror answers, so a refusal comes from the installer.
curl -fsS "$url/latest/download/SHA256SUMS" >/dev/null
try http-mirror "$PATH" "$url"
if [ -n "$https_url" ]; then
    curl -fsSL --cacert "$tmp/cert.pem" "$https_url/latest/download/SHA256SUMS" >/dev/null
    try https-redirected-to-http "$PATH" "$https_url"
else
    echo "skip  https-redirected-to-http: no openssl to make a certificate"
fi
try without-curl "$tmp/nocurl" "${https_url:-$url}"
exit "$fail"
