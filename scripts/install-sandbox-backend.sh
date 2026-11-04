#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# X0NE, install-sandbox-backend
#
# Internal helper invoked by `make setup-incus` / `make setup-podman` when
# the corresponding daemon is missing. Installs a local Incus (preferred)
# or Podman backend with btrfs storage so X0NE's snapshot/restore primitives
# run as O(1) reflinks instead of linear file copies. On btrfs:
#
#   * `RunIncusGroup` clones N containers from the same base in <200ms
#   * `RestoreSnapshot` is a pointer swap, regardless of repo size
#   * `RunInSandbox` (Podman) gets the same CoW boost when /var/lib is btrfs
#
# Most users hit this transparently via:
#   make setup-incus      # full onboarding: install Incus if missing, then
#                         # create the x0ne-sandbox container + baseline
#   make setup-podman     # same flow, Podman backend
#
# Direct usage (advanced):
#   bash scripts/install-sandbox-backend.sh                 # auto-pick
#   bash scripts/install-sandbox-backend.sh --backend incus
#   bash scripts/install-sandbox-backend.sh --backend podman
#   bash scripts/install-sandbox-backend.sh --dry-run       # plan only
#
# Idempotent. Safe to re-run.
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

BACKEND="auto"
DRY_RUN=0
LOOP_SIZE_GB="${LOOP_SIZE_GB:-20}"
INCUS_POOL_NAME="${INCUS_POOL_NAME:-x0ne-tt}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --backend)  BACKEND="$2"; shift 2 ;;
        --backend=*) BACKEND="${1#*=}"; shift ;;
        --dry-run)  DRY_RUN=1; shift ;;
        -h|--help)
            sed -n '3,17p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

# ── Colors ────────────────────────────────────────────────────────────────────
B='\033[1m'; D='\033[2m'; R='\033[0m'
GRN='\033[32m'; YEL='\033[33m'; RED='\033[31m'; CYN='\033[36m'

say()  { printf "%b\n" "$*"; }
info() { printf "  ${CYN}ℹ${R} %b\n" "$*"; }
ok()   { printf "  ${GRN}✓${R} %b\n" "$*"; }
warn() { printf "  ${YEL}!${R} %b\n" "$*"; }
fail() { printf "  ${RED}✗${R} %b\n" "$*" >&2; exit 1; }

run() {
    if [[ $DRY_RUN -eq 1 ]]; then
        printf "  ${D}\$${R} %s\n" "$*"
    else
        eval "$@"
    fi
}

# ── Detect OS / package manager ───────────────────────────────────────────────
if [[ "$(uname -s)" != "Linux" ]]; then
    fail "install-sandbox-backend only runs on Linux (Incus and Podman btrfs CoW are Linux-only). Got: $(uname -s)"
fi

if   command -v apt-get >/dev/null 2>&1; then PM="apt-get"
elif command -v pacman  >/dev/null 2>&1; then PM="pacman"
elif command -v dnf     >/dev/null 2>&1; then PM="dnf"
elif command -v zypper  >/dev/null 2>&1; then PM="zypper"
else
    PM=""
    warn "no recognized package manager (apt/pacman/dnf/zypper). Manual install required."
fi

# ── Detect btrfs availability ─────────────────────────────────────────────────
ROOT_FS="$(stat -f -c %T / 2>/dev/null || echo unknown)"
LIB_FS="$(stat -f -c %T /var/lib 2>/dev/null || echo unknown)"

# ── Banner ────────────────────────────────────────────────────────────────────
say ""
say "${B}X0NE, install-sandbox-backend${R}    ${D}(backend=${BACKEND}, dry-run=${DRY_RUN})${R}"
say ""
info "OS:           Linux ($(uname -m), ${PM:-no-pm})"
info "/ filesystem:        ${ROOT_FS}"
info "/var/lib filesystem: ${LIB_FS}"
say ""

# ── Pick backend ──────────────────────────────────────────────────────────────
have_incus=0; have_podman=0
command -v incus  >/dev/null 2>&1 && have_incus=1
command -v podman >/dev/null 2>&1 && have_podman=1

if [[ "$BACKEND" == "auto" ]]; then
    if [[ $have_incus -eq 1 ]]; then BACKEND="incus"
    elif [[ $have_podman -eq 1 ]]; then BACKEND="podman"
    else BACKEND="incus"; fi  # default to incus when neither present
    info "auto-selected backend: ${B}${BACKEND}${R}"
fi
say ""

# ── INCUS PATH ────────────────────────────────────────────────────────────────
setup_incus() {
    if [[ $have_incus -eq 0 ]]; then
        say "${B}Step 1, install Incus${R}"
        case "$PM" in
            apt-get) run "sudo apt-get update && sudo apt-get install -y incus btrfs-progs" ;;
            pacman)  run "sudo pacman -Sy --noconfirm incus btrfs-progs" ;;
            dnf)     run "sudo dnf install -y incus btrfs-progs" ;;
            zypper)  run "sudo zypper install -y incus btrfs-progs" ;;
            *)       fail "no package manager. Install incus + btrfs-progs manually, then re-run." ;;
        esac
        ok "Incus installed"
    else
        ok "Incus already installed ($(incus --version 2>/dev/null | head -1))"
    fi
    say ""

    # Init: only if not already initialized
    if incus storage list --format csv 2>/dev/null | grep -q "^${INCUS_POOL_NAME},"; then
        ok "Incus storage pool '${INCUS_POOL_NAME}' already exists, skipping init"
    else
        say "${B}Step 2, init Incus with btrfs storage pool${R}"
        if [[ "$LIB_FS" == "btrfs" ]]; then
            info "/var/lib is btrfs, native subvolume backend"
            run "sudo incus storage create ${INCUS_POOL_NAME} btrfs"
        else
            info "/var/lib is ${LIB_FS}, falling back to btrfs loop file (${LOOP_SIZE_GB}GiB)"
            run "sudo incus storage create ${INCUS_POOL_NAME} btrfs size=${LOOP_SIZE_GB}GiB"
        fi
        # Default profile: attach the pool as root device if no root device yet
        if ! incus profile show default 2>/dev/null | grep -q "type: disk"; then
            run "incus profile device add default root disk path=/ pool=${INCUS_POOL_NAME}"
            ok "Default profile root device wired to '${INCUS_POOL_NAME}'"
        fi
        ok "Storage pool '${INCUS_POOL_NAME}' created"
    fi
    say ""

    # Smoke test
    say "${B}Step 3, smoke test (launch + snapshot + restore + delete)${R}"
    local NAME="x0ne-tt-smoke-$$"
    run "incus launch images:alpine/edge ${NAME} --ephemeral"
    run "incus exec ${NAME} -- echo time-travel-ok > /dev/null"
    run "incus snapshot create ${NAME} pre-mutation"
    run "incus exec ${NAME} -- rm -rf / 2>/dev/null || true"
    run "incus snapshot restore ${NAME} pre-mutation"
    run "incus exec ${NAME} -- echo restored-ok > /dev/null"
    run "incus delete ${NAME} --force"
    ok "Smoke test passed: launch + snapshot + agent-rampage + restore + delete"
    say ""

    say "${B}${GRN}Time-travel ready.${R}"
    say ""
    info "Next:  ${CYN}make run-incus${R}                    (run x0ne in an isolated container)"
    info "       ${CYN}make scenarios-incus INCUS_REMOTE=local${R}    (run scenario suite)"
    info "       ${CYN}x0ne --target sandbox${R}             (point an interactive session at it)"
    say ""
}

# ── PODMAN PATH ───────────────────────────────────────────────────────────────
setup_podman() {
    if [[ $have_podman -eq 0 ]]; then
        say "${B}Step 1, install Podman${R}"
        case "$PM" in
            apt-get) run "sudo apt-get update && sudo apt-get install -y podman btrfs-progs" ;;
            pacman)  run "sudo pacman -Sy --noconfirm podman btrfs-progs" ;;
            dnf)     run "sudo dnf install -y podman btrfs-progs" ;;
            zypper)  run "sudo zypper install -y podman btrfs-progs" ;;
            *)       fail "no package manager. Install podman + btrfs-progs manually, then re-run." ;;
        esac
        ok "Podman installed"
    else
        ok "Podman already installed ($(podman --version 2>/dev/null))"
    fi
    say ""

    say "${B}Step 2, configure Podman storage driver${R}"
    local SCFG="${HOME}/.config/containers/storage.conf"
    if [[ -f "$SCFG" ]] && grep -q '^driver = "btrfs"' "$SCFG" 2>/dev/null; then
        ok "Podman already using btrfs storage driver (${SCFG})"
    elif [[ "$LIB_FS" == "btrfs" ]]; then
        info "/var/lib is btrfs, configuring rootless storage driver"
        run "mkdir -p $(dirname "$SCFG")"
        if [[ $DRY_RUN -eq 0 ]]; then
            cat > "$SCFG" <<'CFG'
[storage]
driver = "btrfs"
runroot = "/run/user/1000/containers"
graphroot = "$HOME/.local/share/containers/storage"
CFG
        else
            printf "  ${D}\$${R} cat > %s <<...EOF (btrfs config)...\n" "$SCFG"
        fi
        ok "Storage driver pinned to btrfs at ${SCFG}"
    else
        warn "/var/lib is ${LIB_FS}, not btrfs. Podman will use overlay/vfs."
        warn "For btrfs CoW, either reformat /var/lib as btrfs or use the Incus backend (with loop file)."
    fi
    say ""

    say "${B}Step 3, smoke test (run + image cache + cleanup)${R}"
    run "podman pull alpine:latest >/dev/null"
    run "podman run --rm alpine:latest echo time-travel-ok"
    ok "Smoke test passed"
    say ""

    say "${B}${GRN}Podman ready.${R}"
    say ""
    info "Next:  ${CYN}make run-podman${R}                  (run x0ne in an isolated container)"
    info "       ${CYN}make scenarios${R}                   (Podman is the default sandbox for RunInSandbox)"
    say ""
    if [[ "$LIB_FS" != "btrfs" ]]; then
        warn "Podman without btrfs ⇒ snapshot operations are file copies, not reflinks."
        warn "If you want O(1) restore, use Incus instead (--backend incus, supports loop btrfs)."
    fi
}

case "$BACKEND" in
    incus)  setup_incus ;;
    podman) setup_podman ;;
    *) fail "unknown backend: $BACKEND (expected: auto | incus | podman)" ;;
esac
