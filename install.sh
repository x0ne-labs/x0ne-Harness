#!/bin/sh
# x0ne installer: downloads a release of x0ne-Harness, verifies it, installs it.
# It installs a published release. Where there is none to download (a version
# or a platform without an archive), the way in is the source build
# (docs/install.md, "Install").
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/x0ne-labs/x0ne-Harness/main/install.sh | sh
#   sh install.sh [--uninstall | --help]
#
# Environment:
#   X0NE_VERSION       release tag to install (v0.2.0 or 0.2.0); default: latest
#   X0NE_INSTALL_DIR   where the binaries go; default: ~/.local/bin
#   X0NE_RELEASES_URL  releases base URL, for a mirror (https only); default: GitHub
#
# It detects the OS and the CPU, downloads the archive and SHA256SUMS of the
# release, refuses on a missing or mismatching checksum, verifies the build
# provenance with `gh attestation verify` when gh 2.56+ is logged in (else it
# says so), then copies three binaries into the install directory. No sudo.
# It never edits a shell startup file. Running it again replaces the binaries.

set -eu

REPO="x0ne-labs/x0ne-Harness"
RELEASES_URL="${X0NE_RELEASES_URL:-https://github.com/${REPO}/releases}"
INSTALL_DIR="${X0NE_INSTALL_DIR:-${HOME}/.local/bin}"
CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/x0ne"
BINS="x0ne x0ne-runtime x0ne-orchestrator"

say() { printf '%s\n' "$*"; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<EOF
x0ne installer

  sh install.sh               install or update x0ne
  sh install.sh --uninstall   remove the binaries this script installs
  sh install.sh --help        this help

Environment: X0NE_VERSION (default: latest release), X0NE_INSTALL_DIR
(default: ~/.local/bin), X0NE_RELEASES_URL (default: GitHub releases).
EOF
}

# ── Platform and tools ───────────────────────────────────────────────────────

detect_target() {
    case "$(uname -s)" in
        Linux) os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) die "unsupported OS: $(uname -s) (supported: Linux, macOS)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) die "unsupported CPU: $(uname -m) (supported: x86_64, aarch64)" ;;
    esac
    TARGET="${arch}-${os}"
    ASSET="x0ne-${TARGET}.tar.gz"
}

detect_tools() {
    # Not wget: GNU wget 1.25 follows an https to http redirect even with
    # --https-only, which applies to recursive mode only (measured 2026-11-02).
    command -v curl >/dev/null 2>&1 || die "curl is required"
    for tool in tar gzip awk; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done
    if command -v sha256sum >/dev/null 2>&1; then
        SHA256="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        SHA256="shasum -a 256"
    else
        die "sha256sum or shasum is required to verify the download"
    fi
}

# fetch <url> <file>: fails on any HTTP error, and on any URL or redirect
# that is not https: the checksum comes from the same place as the archive.
# Prints the last HTTP status, 000 when no response came back.
fetch() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -w '%{http_code}' "$1" -o "$2"
}

# ── Download and verification ────────────────────────────────────────────────

release_url() {
    if [ -n "${X0NE_VERSION:-}" ]; then
        case "$X0NE_VERSION" in
            v*) tag="$X0NE_VERSION" ;;
            *) tag="v$X0NE_VERSION" ;;
        esac
        printf '%s/download/%s/%s' "$RELEASES_URL" "$tag" "$1"
    else
        printf '%s/latest/download/%s' "$RELEASES_URL" "$1"
    fi
}

download() {
    say "Downloading $(release_url "$ASSET")"
    if ! status=$(fetch "$(release_url "$ASSET")" "${TMP}/${ASSET}"); then
        if [ "$status" = 404 ]; then
            die "no release archive for ${TARGET} (${X0NE_VERSION:-latest}): no release is published yet, or none for this version or platform. Build from source instead: see docs/install.md in https://github.com/${REPO}"
        fi
        die "download failed (HTTP status ${status:-000})"
    fi
    fetch "$(release_url SHA256SUMS)" "${TMP}/SHA256SUMS" >/dev/null ||
        die "no SHA256SUMS for this release: refusing to install an unverified archive"
}

verify_checksum() {
    expected=$(awk -v f="$ASSET" '$2 == f || $2 == "*" f { print $1 }' "${TMP}/SHA256SUMS")
    [ -n "$expected" ] || die "${ASSET} is not listed in SHA256SUMS: refusing to install"
    actual=$($SHA256 "${TMP}/${ASSET}" | awk '{ print $1 }')
    [ "$expected" = "$actual" ] ||
        die "checksum MISMATCH for ${ASSET}: expected ${expected}, got ${actual}. Nothing was installed."
    say "Checksum verified (SHA-256)"
}

# gh_unusable: prints why gh cannot verify the provenance, nothing if it can.
# gh has `attestation` from 2.49 and --signer-workflow from 2.51; 2.51 to 2.55
# cannot read the current Sigstore trusted root (measured 2026-11-05).
gh_unusable() {
    if ! command -v gh >/dev/null 2>&1; then
        say "the GitHub CLI (gh) is not installed (gh 2.56 or later verifies it)"
        return 0
    fi
    v=$(gh --version 2>/dev/null | awk 'NR == 1 { print $3 }')
    major=${v%%.*}
    minor=${v#*.}
    minor=${minor%%.*}
    case "${major}:${minor}" in
        *[!0-9:]* | :* | *:) ;; # unreadable (a dev build): the help decides
        *)
            if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 56 ]; }; then
                say "gh ${v} is too old (gh 2.56 or later verifies it)"
                return 0
            fi
            ;;
    esac
    if ! gh attestation verify --help 2>&1 | grep -q -e '--signer-workflow'; then
        say "gh ${v:-(unknown version)} has no 'gh attestation verify --signer-workflow' (gh 2.56 or later has it)"
        return 0
    fi
    gh auth status >/dev/null 2>&1 || say "gh is not logged in (gh auth login)"
}

# The checksum only proves the download intact; the attestation proves the
# release workflow built it from the tagged source. A gh that cannot verify
# counts as no gh; a gh that verifies and fails stops the install.
verify_provenance() {
    why=$(gh_unusable)
    if [ -n "$why" ]; then
        say "Provenance NOT verified: ${why}."
        say "Only the SHA-256 checksum was checked, against a SHA256SUMS from the same server."
        say "To verify a release by hand: https://github.com/${REPO}/blob/main/docs/install.md#from-a-release"
        return 0
    fi
    gh attestation verify "${TMP}/${ASSET}" --repo "$REPO" \
        --signer-workflow "${REPO}/.github/workflows/release.yml" >/dev/null ||
        die "provenance verification FAILED for ${ASSET}. Nothing was installed."
    say "Provenance verified (gh attestation verify)"
}

# ── Install and uninstall ────────────────────────────────────────────────────

install_files() {
    tar -xzf "${TMP}/${ASSET}" -C "$TMP"
    src="${TMP}/x0ne-${TARGET}"
    [ -d "$src" ] || die "unexpected archive layout: no x0ne-${TARGET}/ in ${ASSET}"
    mkdir -p "$INSTALL_DIR"
    for bin in $BINS; do
        [ -f "${src}/${bin}" ] || die "${bin} is missing from ${ASSET}"
        # Copy, then rename: a running binary is replaced, never rewritten.
        cp "${src}/${bin}" "${INSTALL_DIR}/.${bin}.new"
        chmod 0755 "${INSTALL_DIR}/.${bin}.new"
        mv -f "${INSTALL_DIR}/.${bin}.new" "${INSTALL_DIR}/${bin}"
        say "Installed ${INSTALL_DIR}/${bin}"
    done
    # Sample configuration, copied only when absent: an edited file is kept.
    if [ -d "${src}/config" ]; then
        mkdir -p "$CONFIG_DIR"
        for cfg in "${src}"/config/*; do
            [ -f "$cfg" ] || continue
            dst="${CONFIG_DIR}/$(basename "$cfg")"
            if [ -e "$dst" ]; then
                say "Kept ${dst}"
            else
                cp "$cfg" "$dst"
                say "Installed ${dst}"
            fi
        done
    fi
}

check_path() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            say ""
            say "${INSTALL_DIR} is not in your PATH. This script edits no shell file;"
            say "add this line to your shell startup file (~/.profile, ~/.bashrc or ~/.zshrc):"
            say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
    esac
}

uninstall() {
    removed=0
    for bin in $BINS; do
        if [ -e "${INSTALL_DIR}/${bin}" ]; then
            rm -f "${INSTALL_DIR:?}/${bin}"
            say "Removed ${INSTALL_DIR}/${bin}"
            removed=1
        fi
    done
    [ "$removed" = 1 ] || say "Nothing to remove in ${INSTALL_DIR}"
    say "Kept ${CONFIG_DIR} and ~/.x0ne (configuration and data); remove them by hand if you wish."
}

main() {
    case "${1:-}" in
        -h | --help)
            usage
            exit 0
            ;;
        --uninstall)
            uninstall
            exit 0
            ;;
        "") ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
    detect_target
    detect_tools
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    trap 'exit 130' INT TERM
    download
    verify_checksum
    verify_provenance
    install_files
    "${INSTALL_DIR}/x0ne" --version || die "x0ne was installed but does not run on this machine"
    check_path
}

main "$@"
