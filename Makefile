.DEFAULT_GOAL := help

# SECURITY (audit 2026-09-11, 04-F05): bash with pipefail, so a recipe that
# pipes cargo into grep/awk/tail FAILS when cargo fails. Under the default
# shell, `test-all` printed "✓ All done." over red tests.
SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c

CARGO_HOME ?= $(HOME)/.cargo
# Cargo's target directory, exported to every recipe. Default: target/ in the
# tree. A path under /tmp is refused: 23 GiB of targets filled the 30 GiB
# tmpfs on 2026-10-05. Put an out-of-tree target under ~/.cache.
CARGO_TARGET_DIR ?= $(CURDIR)/target
ifneq (,$(filter /tmp /tmp/%,$(CARGO_TARGET_DIR)))
$(error CARGO_TARGET_DIR=$(CARGO_TARGET_DIR) is on tmpfs; use $(HOME)/.cache/x0ne-build/harness-target)
endif
export CARGO_TARGET_DIR
TARGET     := $(CARGO_TARGET_DIR)
# Every cargo invocation runs inside its own memory-capped cgroup
# (scripts/capped, X0NE_MEMCAP defaults to 16G). Two machine crashes on
# 2026-08-15/16 came from test binaries eating all RAM inside the user's
# session cgroup; a scope with MemoryMax lets the kernel kill only the culprit.
CAP        := $(CURDIR)/scripts/capped
CARGO      := $(CAP) env CARGO_HOME=$(CARGO_HOME) cargo
# Tests and scenarios get a HOME of their own (2026-09-18). Several
# crates resolve `~/.x0ne/...` from HOME and their tests write there: a run of
# the suite left `~/.x0ne/artifacts` and `~/.x0ne/functions` on the owner's
# machine — fixture debris under the REAL name, next to real sessions.
# CARGO_HOME and RUSTUP_HOME are pinned to the real ones, or cargo would look
# for its toolchain in the empty home.
RUSTUP_HOME ?= $(HOME)/.rustup
TEST_HOME   ?= $(TARGET)/test-home
$(shell mkdir -p $(TEST_HOME))
CARGO_TEST  := $(CAP) env CARGO_HOME=$(CARGO_HOME) RUSTUP_HOME=$(RUSTUP_HOME) HOME=$(TEST_HOME) XDG_CONFIG_HOME=$(TEST_HOME)/.config XDG_DATA_HOME=$(TEST_HOME)/.local/share cargo
BIN        := $(TARGET)/release
X0NE       := PATH="$(BIN):$(PATH)"

# Auto-load .env.dev if present (OLLAMA_BASE_URL, OLLAMA_API_KEY, etc.)
-include .env.dev
export

# ── Targets ────────────────────────────────────────────────────────────────────

.PHONY: help build build-dev clean hooks third-party loop \
        x0ne-runtime x0ne-orchestrator x0ne-code \
        check-runtime check-orchestrator check-cli \
        test-runtime test-orchestrator test-cli \
        install install-runtime install-orchestrator install-cli uninstall \
        enclave-binaries \
        setup-incus run run-server run-orchestrator run-tui run-matrix run-desktop run-mcp audit-mcp \
        mcp-codex codex-add codex-remove \
        sandbox-image sandbox-image-podman sandbox-image-incus test-sandbox-image \
        run-incus run-incus-runtime run-incus-tui run-incus-matrix run-incus-dirty \
        sync-incus sync-incus-musl force-sync-incus-musl \
        snapshot-incus reset-incus rebuild-baseline recover-sandbox-artifacts \
        setup-ssh-aliases test-ssh-aliases \
        diagnose-incus test-mock-incus dev demo \
        setup-podman sync-podman-musl force-sync-podman-musl \
        snapshot-podman reset-podman rebuild-baseline-podman \
        run-podman run-podman-runtime run-podman-tui run-podman-matrix run-podman-dirty test-run-sandboxed \
        sandbox-egress-image test-sandbox-egress \
        diagnose-podman test-mock-podman \
        test test-all test-all-incus test-matrix test-desktop \
        check fix clean probe scenarios scenarios-verbose scenarios-incus \
        demo-undo self-test demo-incus \
        stress-test stress-pick stress-dry \
        claude-add claude-remove dataset dataset-dpo dataset-index dataset-query \
        dataset-refresh dataset-sft demo-undo-fast test-demo-undo eval export-schema install-browser \
        install-vm x0ne-browser x0ne-browser-test x0ne-vm x0ne-vm-control x0ne-vm-demo \
        x0ne-vm-doctor x0ne-vm-firefox-setup x0ne-vm-mission-control \
        x0ne-vm-mission-control-stop x0ne-vm-mouse-tricks x0ne-vm-reset x0ne-vm-scenario \
        x0ne-vm-session x0ne-vm-setup x0ne-vm-test x0ne-vm-test-advanced x0ne-vm-watch

# Incus container config — override in .env.dev if needed.
#
# All sandbox commands target `$(INCUS_REMOTE):$(INCUS_CONTAINER)`. The
# default is `local:x0ne-sandbox` so a fresh clone works out of the box
# against the laptop's Incus daemon. Override INCUS_REMOTE=<remote-name>
# to push to a remote (set up via `incus remote add`); we use
# `prod-infra` for our own staging fleet — yours will differ.
INCUS_REMOTE        ?= local
INCUS_CONTAINER     ?= x0ne-sandbox
INCUS_TARGET        ?= $(INCUS_REMOTE):$(INCUS_CONTAINER)
# Path of the x0ne-runtime binary INSIDE the Incus container (after sync-incus).
# `INCUS_CORE_BIN` is still honoured as a backward-compatible alias for users
# whose `.env.dev` predates the x0ne-core → x0ne-runtime rename.
INCUS_RUNTIME_BIN   ?= $(or $(INCUS_CORE_BIN),/opt/x0ne-runtime)
INCUS_CORE_BIN      := $(INCUS_RUNTIME_BIN)
INCUS_SNAPSHOT_NAME ?= x0ne-baseline

# ── Auth-forward allowlist ────────────────────────────────────────────────────
#
# Names of host env vars that should be forwarded **into** the sandboxed
# x0ne-runtime via `incus exec --env` / `podman exec --env`. Only listed
# names are forwarded; only those that are actually set on the host become
# `--env` arguments (empty/unset vars are silently skipped, no leak).
#
# Used by `LlmCall { auth: "$ENV:ANTHROPIC_API_KEY" }`, `OllamaApi` against
# managed proxies, and any tool inside the sandbox that needs to reach
# Claude / OpenAI / a remote Ollama. Override per session with e.g.
#   X0NE_FORWARD_ENV="ANTHROPIC_API_KEY OLLAMA_HOST" make run-incus
#
# Names only, never values: a value on argv shows in `ps` and `make -n`
# (audit 2026-10-05, P0). Podman reads `--env NAME` from its own environment;
# Incus gets a 0600 file pushed on stdin (scripts/sandbox-env.sh).
X0NE_FORWARD_ENV    ?= OLLAMA_HOST ANTHROPIC_API_KEY OPENAI_API_KEY X0NE_OPENAI_COMPAT_KEY X0NE_LLM_PROVIDER X0NE_LLM_MODEL

forward_env_args     = $(foreach v,$(X0NE_FORWARD_ENV),$(if $($(v)),--env $(v),))

# A reference to MAKE inside a compound line runs even under `make -n`.
SUBMAKE := $(MAKE)

# Environment passed into the sandbox via `incus exec --env`.
# X0NE_ALLOW_LOOPBACK=1 lets the agent hit 127.0.0.1 / ::1 to probe
# services it launched inside the container — the sandbox *is* the
# isolation boundary, so loopback-internal traffic is safe. Cloud
# metadata endpoints and other private ranges remain blocked.
INCUS_RUN_ENV       ?= --env X0NE_ALLOW_LOOPBACK=1
# Sources the pushed env file, then execs the runtime.
INCUS_RUNTIME_WRAP  ?= /root/x0ne-runtime-env.sh

# ── Podman sandbox config — mirror of the Incus block above ────────────────────
#
# The orchestrator auto-detects the container runtime from the first token of
# `X0NE_SANDBOX_CMD` (see `container_runtime::detect_from_sandbox_cmd`), so the
# `run-podman*` targets reuse the same canvas/bridge plumbing as `run-incus*`.
# Baseline re-creation is via `podman commit` → `podman rm -f && podman run`
# rather than the btrfs-backed snapshot Incus uses: slightly slower (~1–2 s vs
# <1 s) but runs on any Podman install without special storage config.
PODMAN_CONTAINER     ?= x0ne-sandbox-podman
PODMAN_IMAGE_BASE    ?= debian:12
PODMAN_BASELINE_IMG  ?= localhost/x0ne-sandbox-baseline:latest
PODMAN_RUNTIME_BIN   := /opt/x0ne-runtime
# Same loopback-only relaxation as Incus — applied via `podman exec --env`.
# Auth-forward allowlist (see X0NE_FORWARD_ENV above) is appended so the
# in-container runtime can resolve `$ENV:ANTHROPIC_API_KEY` etc.
PODMAN_RUN_ENV       ?= --env X0NE_ALLOW_LOOPBACK=1 $(forward_env_args)
# Podman requires `-i` to attach the caller's stdin to the containerised
# process — without it the stdio-mode x0ne-runtime reads EOF immediately,
# the bridge's ListActions health check times out, and the session dies
# with "x0ne-runtime returned an empty response line". `incus exec` wires
# stdin up by default, so we don't need the equivalent on the Incus path.
PODMAN_EXEC_FLAGS    ?= -i

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*##' $(firstword $(MAKEFILE_LIST)) | \
		awk 'BEGIN{FS=":.*##"} {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

build: ## Build the default members (release; vm, browser, pointer: make x0ne-vm / x0ne-browser)
	$(CARGO) build --release

build-dev: ## Build the default members (debug; for X0NE contributors)
	$(CARGO) build

clean: ## Remove build artifacts ($(TARGET)) — keeps registry/git cache
	$(CARGO) clean

hooks: ## Use scripts/hooks for this clone (commit-msg check, pre-push guard)
	git config core.hooksPath scripts/hooks

third-party: ## Regenerate THIRD-PARTY.md from Cargo.lock with cargo-about (offline; same flags as the committed file)
	@command -v cargo-about >/dev/null 2>&1 || { printf '%b' "  ✗ cargo-about is missing. Install it yourself: cargo install cargo-about --locked\n"; exit 1; }
	$(CARGO) about generate --offline --locked --workspace --fail -o THIRD-PARTY.md about.hbs

# ── Per-crate build targets ───────────────────────────────────────────────────
# Faster iteration when you only touch one half of the workspace. Default to
# release; pass `--profile dev` directly via cargo if you need debug for one
# crate (or use `make build-dev` for the whole workspace).

x0ne-runtime: ## Build x0ne-runtime crate only (release)
	$(CARGO) build -p x0ne-runtime --release

x0ne-orchestrator: ## Build x0ne-orchestrator crate only (release) — lib + x0ne-orchestrator bin
	$(CARGO) build -p x0ne-orchestrator --release

x0ne-code: ## Build x0ne-code crate only (release) — the `x0ne` interactive binary
	$(CARGO) build -p x0ne-code --release

check-runtime: ## Fast type-check x0ne-runtime only
	$(CARGO) check -p x0ne-runtime

check-orchestrator: ## Fast type-check x0ne-orchestrator only
	$(CARGO) check -p x0ne-orchestrator

check-cli: ## Fast type-check x0ne-code only
	$(CARGO) check -p x0ne-code

test-runtime: ## Run tests for x0ne-runtime only (lib + integration)
	$(CARGO_TEST) test -p x0ne-runtime --no-fail-fast

test-orchestrator: ## Run tests for x0ne-orchestrator only (lib + integration)
	$(CARGO_TEST) test -p x0ne-orchestrator --no-fail-fast

test-cli: ## Run tests for x0ne-code only
	$(CARGO_TEST) test -p x0ne-code --no-fail-fast

# ── Install ───────────────────────────────────────────────────────────────────

install: build ## Install all release binaries + XDG config into ~/.cargo/bin and ~/.config/x0ne/
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne-runtime $(HOME)/.cargo/bin/
	@install -m 755 $(BIN)/x0ne-orchestrator $(HOME)/.cargo/bin/
	@install -m 755 $(BIN)/x0ne $(HOME)/.cargo/bin/
	@# XDG config — the runtime TOMLs that `x0ne` loads in sandbox mode.
	@# (`x0ne-runtime-incus-only.toml` is gone: container management moved
	@# orchestrator-side, no host-side runtime to launch.)
	@mkdir -p "$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne"
	@# NEVER clobber a config the operator has configured. `install` used to
	@# overwrite unconditionally, so every `make install` silently reset their
	@# allowed paths, allowed commands and policy mode — including mid-session
	@# while they were testing (field report, 2026-08-12). Ship the reference
	@# only when there is nothing there; otherwise leave it alone and say so.
	@cfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne"; \
	for f in x0ne-runtime.toml x0ne-runtime-sandbox.toml; do \
	  if [ -e "$$cfg/$$f" ]; then \
	    echo "  · kept your $$f (reference at x0ne-runtime/$$f)"; \
	  else \
	    install -m 644 "x0ne-runtime/$$f" "$$cfg/"; \
	  fi; \
	done
	@# SECURITY: the MCP profile. `x0ne-runtime.toml` above is the CLI
	@# reference — `policy.mode = "auto"`, and that is correct there because a
	@# human is sitting in front of `x0ne`. An MCP client is not a human, and
	@# until 2026-08-15 `.mcp.json` launched `x0ne-runtime --mode mcp` with no
	@# `--config` at all, so it silently inherited that CLI reference and every
	@# MCP client had unattended `run_command` on the host. The hardened
	@# profile existed in this repo the whole time and nothing pointed at it.
	@# It is now installed under its own name, and `.mcp.json` / `claude-add`
	@# name it explicitly. Same never-clobber rule as above.
	@cfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne"; \
	if [ -e "$$cfg/x0ne-runtime-mcp.toml" ]; then \
	  echo "  · kept your x0ne-runtime-mcp.toml (reference at x0ne-runtime-mcp-readonly.toml)"; \
	else \
	  install -m 644 "x0ne-runtime-mcp-readonly.toml" "$$cfg/x0ne-runtime-mcp.toml"; \
	fi
	@echo "  ✓ x0ne + x0ne-orchestrator + x0ne-runtime installed to ~/.cargo/bin/"
	@echo "  ✓ XDG config deployed to $${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/"
	@echo "  ✓ MCP policy: $${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/x0ne-runtime-mcp.toml (readonly, no commands)"

install-runtime: x0ne-runtime ## Install x0ne-runtime binary only
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne-runtime $(HOME)/.cargo/bin/
	@echo "  ✓ x0ne-runtime installed to ~/.cargo/bin/"

install-orchestrator: x0ne-orchestrator ## Install x0ne-orchestrator binary only (headless HTTP/SSE)
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne-orchestrator $(HOME)/.cargo/bin/
	@echo "  ✓ x0ne-orchestrator installed to ~/.cargo/bin/"

install-cli: x0ne-code ## Install x0ne binary only (interactive CLI/TUI/Matrix)
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne $(HOME)/.cargo/bin/
	@echo "  ✓ x0ne installed to ~/.cargo/bin/"

# ── Enclave binaries (the payload `x0ne cc` uploads into the VM) ──────────────
#
# NOT part of `make install`, and not a dependency of anything: the container
# build has its own cache and pays for it (measured 2026-08-15 on this
# workstation: ~6 min cold, 22 s once `target/enclave-build` is warm), and only
# a run that targets the enclave has to. Type it when you are about to
# `x0ne cc --machine full`.
#
# WHY IT EXISTS, measured on run 8 (2026-08-15): `InstallX0ne` copied the host's
# `~/.cargo/bin/x0ne` into the Ubuntu 22.04 VM, and `x0ne --version` came back
# exit 1. `objdump -T` on the host binary named the reason — it requires
# GLIBC_2.39 (this workstation is Debian 13, glibc 2.41) and jammy ships 2.35,
# so the dynamic loader refuses to start it. A ~30 min pipeline at 3.72 $/h,
# attestation and all, died on its last step for a link-time detail. An ELF can
# run on any glibc AT LEAST as new as the one it was linked against and no
# older, so the fix is to build against an OLD one: bullseye has 2.31, comfortably
# under jammy's 2.35, and the resulting binaries also load on anything newer.
#
# `x0ne cc` picks these up by itself: it looks for `target/enclave/` from the
# directory you run it in and upwards (see `resolve_x0ne_binaries`), and the
# plan line names the two files it is about to send. `X0NE_ENCLAVE_BINARIES=<dir>`
# overrides the search.
#
# The rust image is pinned to the bullseye FAMILY, not to a version: what
# matters is the glibc it links against, and the recipe checks that property on
# the output rather than trusting the tag. Verified 2026-08-15 —
# `rust:1-bullseye` carries rustc 1.97.1 (same as the host) and glibc 2.31, and
# the workspace builds in it unmodified.
#
# The known alternative, and why it is not here: `sync-incus-musl` /
# `sync-podman-musl` already solve the same problem with
# `cargo zigbuild --target x86_64-unknown-linux-gnu.2.35` (they are misnamed —
# that is versioned glibc, not musl), and it is faster than a container. It was
# not chosen because it INSTALLS on the operator's machine — `cargo-zigbuild`
# plus `pip install ziglang --break-system-packages` — where podman only pulls
# an image. The trade-off stays open, together with the real static
# (musl) build that would make the payload a single measurable artifact.
ENCLAVE_IMAGE     ?= docker.io/library/rust:1-bullseye
# The VM image `x0ne cc` provisions is Ubuntu 22.04 (jammy) → glibc 2.35.
# Raise this only together with the image the pipeline boots.
ENCLAVE_MAX_GLIBC ?= 2.35
# A target dir of its own: a container build writes root-owned-in-userns
# artifacts compiled against another libc, and letting those land in
# `target/release/` would silently poison the next `make install`.
ENCLAVE_TARGET    := target/enclave-build
ENCLAVE_OUT       := target/enclave

enclave-binaries: ## Build x0ne + x0ne-runtime against an old glibc (podman, ~6 min cold) into target/enclave/ — what `x0ne cc` uploads
	@command -v podman >/dev/null 2>&1 || { \
		echo "  ✗ podman is required to build enclave binaries (image: $(ENCLAVE_IMAGE))"; exit 1; }
	@podman image exists $(ENCLAVE_IMAGE) 2>/dev/null || { \
		echo "  → pulling $(ENCLAVE_IMAGE)…"; podman pull $(ENCLAVE_IMAGE) >/dev/null || exit 1; }
	@echo "  → building x0ne + x0ne-runtime in $(ENCLAVE_IMAGE) (glibc ≤ $(ENCLAVE_MAX_GLIBC) target)…"
	@podman run --rm \
		-v "$(CURDIR)":/src \
		-w /src \
		-e CARGO_TARGET_DIR=/src/$(ENCLAVE_TARGET) \
		-e CARGO_HOME=/src/$(ENCLAVE_TARGET)/cargo-home \
		$(ENCLAVE_IMAGE) \
		cargo build --release -p x0ne-code --bin x0ne -p x0ne-runtime --bin x0ne-runtime
	@mkdir -p $(ENCLAVE_OUT)
	@install -m 755 $(ENCLAVE_TARGET)/release/x0ne         $(ENCLAVE_OUT)/x0ne
	@install -m 755 $(ENCLAVE_TARGET)/release/x0ne-runtime $(ENCLAVE_OUT)/x0ne-runtime
	@# FAIL-CLOSED, and this is the whole point of the target: the build
	@# succeeding proves nothing about what the enclave's loader will accept.
	@# Run inside the image (its objdump is guaranteed present, the host's is
	@# not), on the COPIED files, so what is checked is what `x0ne cc` sends.
	@# `sort -Vu | tail -1` is the highest version tag the ELF demands; a
	@# binary asking for more than the VM has is a step that fails after the
	@# money is spent.
	@podman run --rm -v "$(CURDIR)":/src -w /src $(ENCLAVE_IMAGE) sh -c '\
		set -e; \
		for b in x0ne x0ne-runtime; do \
		  max=$$(objdump -T $(ENCLAVE_OUT)/$$b | grep -o "GLIBC_[0-9.]*" | sort -Vu | tail -1); \
		  ver=$${max#GLIBC_}; \
		  if [ "$$(printf "%s\n%s\n" "$$ver" "$(ENCLAVE_MAX_GLIBC)" | sort -V | tail -1)" != "$(ENCLAVE_MAX_GLIBC)" ]; then \
		    echo "  ✗ $$b requires $$max, more than the enclave image has (glibc $(ENCLAVE_MAX_GLIBC))."; \
		    echo "    Its loader would refuse it — exactly the run 8 failure this target exists to prevent."; \
		    echo "    Build with an older ENCLAVE_IMAGE, or raise ENCLAVE_MAX_GLIBC together with the VM image."; \
		    exit 1; \
		  fi; \
		  echo "  ✓ $$b needs at most $$max (enclave has glibc $(ENCLAVE_MAX_GLIBC))"; \
		done'
	@echo "  ✓ enclave binaries in $(ENCLAVE_OUT)/ — 'x0ne cc' will prefer them over the running x0ne"

# ── x0ne-vm (libvirt + VNC agent MCP server) ──────────────────────────────────

x0ne-vm: ## Build x0ne-vm-mcp only (release)
	$(CARGO) build -p x0ne-vm --release --bin x0ne-vm-mcp

install-vm: x0ne-vm ## Install x0ne-vm-mcp to ~/.cargo/bin/
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne-vm-mcp $(HOME)/.cargo/bin/
	@echo "  ✓ x0ne-vm-mcp installed to ~/.cargo/bin/"

x0ne-vm-setup: x0ne-vm ## Provision a libvirt test VM (~50MB ISO, zero sudo, uses qemu:///session)
	@bash scripts/x0ne-vm-setup.sh

x0ne-vm-demo: ## Run the visual round-trip demo (vm_list / vm_screenshot / vm_key / xdg-open)
	@bash scripts/x0ne-vm-demo.sh

x0ne-vm-test: ## Run unit + integration tests (with --nocapture for the demo replay)
	$(CARGO_TEST) test -p x0ne-vm --no-fail-fast
	$(CARGO_TEST) test -p x0ne-vm --test integration_demo -- --nocapture

x0ne-vm-watch: ## Quasi-live browser view of the VM via polled screenshots (no VNC viewer contention)
	@bash scripts/x0ne-vm-watch.sh

x0ne-vm-scenario: ## Run the narrated agent scenario (typing, login, commands) producing per-step screenshots
	@bash scripts/x0ne-vm-agent-scenario.sh

x0ne-vm-test-advanced: ## Run the 21-check end-to-end test against the running VM (lifecycle, snapshots, stress, errors)
	@bash scripts/x0ne-vm-test-advanced.sh

x0ne-vm-control: ## Run the 24-step end-to-end control test (every tool cross-verified)
	@bash scripts/x0ne-vm-control-test.sh

x0ne-vm-doctor: ## System diagnosis — what's installed, what's running, what's missing
	@bash scripts/x0ne-vm-doctor.sh

x0ne-vm-firefox-setup: ## Provision the Debian + Xorg + Firefox VM (one-shot, ~5min apt install via cloud-init)
	@bash scripts/x0ne-vm-firefox-setup.sh

x0ne-vm-mouse-tricks: ## Draw circle / spiral / lissajous patterns in the cursor (visual)
	@bash scripts/x0ne-vm-mouse-tricks.sh

x0ne-vm-session: ## One-command "give the agent its body" — restore golden + open watcher + brief
	@bash scripts/x0ne-vm-session

x0ne-vm-reset: ## Quick reset of the VM to the golden state (~2s)
	@bash scripts/x0ne-vm-session --no-watch

x0ne-vm-mission-control: ## Spin up the pool daemon + web dashboard (http://localhost:7901)
	@bash scripts/x0ne-vm-control-start.sh

x0ne-vm-mission-control-stop: ## Stop the pool daemon + destroy all pool VMs
	@bash scripts/x0ne-vm-control-stop.sh

# ── x0ne-browser (CDP-driven Chromium tools for frontend dev agents) ─────

x0ne-browser: ## Build x0ne-browser-mcp only (release)
	$(CARGO) build -p x0ne-browser --release --bin x0ne-browser-mcp

install-browser: x0ne-browser ## Install x0ne-browser-mcp to ~/.cargo/bin/
	@mkdir -p "$(HOME)/.cargo/bin"
	@install -m 755 $(BIN)/x0ne-browser-mcp $(HOME)/.cargo/bin/
	@echo "  ✓ x0ne-browser-mcp installed to ~/.cargo/bin/"

x0ne-browser-test: ## Run x0ne-browser unit + integration tests (no Chromium needed)
	$(CARGO_TEST) test -p x0ne-browser --no-fail-fast

uninstall: ## Remove all installed binaries + XDG config + MCP registrations (preserves ~/.x0ne/ user data)
	@echo "  $(shell tput setaf 3 2>/dev/null)Removing X0NE from this system$(shell tput sgr0 2>/dev/null)"
	@# Best-effort MCP unregister (won't fail if claude/codex CLIs missing)
	@command -v claude >/dev/null 2>&1 && { \
		claude mcp remove x0ne         >/dev/null 2>&1 && echo "  ✓ unregistered 'x0ne' from Claude Code"         || true; \
		claude mcp remove x0ne-gateway >/dev/null 2>&1 && echo "  ✓ unregistered 'x0ne-gateway' from Claude Code" || true; \
	} || echo "  · claude CLI not in PATH (skipping Claude Code unregister)"
	@if [ -f "$(HOME)/.codex/config.toml" ]; then \
		tmp="$$(mktemp)"; \
		awk 'BEGIN{skip=0} \
		     /^\[mcp_servers\.x0ne\]/{skip=1; next} \
		     /^\[mcp_servers\.x0ne-gateway\]/{skip=1; next} \
		     /^\[mcp_servers\.x0ne\]/{skip=1; next} \
		     /^\[mcp_servers\.x0ne-gateway\]/{skip=1; next} \
		     /^\[/{skip=0} \
		     !skip{print}' "$(HOME)/.codex/config.toml" > "$$tmp"; \
		mv "$$tmp" "$(HOME)/.codex/config.toml"; \
		echo "  ✓ unregistered 'x0ne' / 'x0ne-gateway' from Codex config"; \
	fi
	@# Binaries
	@for bin in x0ne x0ne-runtime x0ne-orchestrator; do \
		if [ -f "$(HOME)/.cargo/bin/$$bin" ]; then \
			rm -f "$(HOME)/.cargo/bin/$$bin" && echo "  ✓ removed ~/.cargo/bin/$$bin"; \
		fi; \
	done
	@# XDG config
	@cfg_dir="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne"; \
	if [ -d "$$cfg_dir" ] && [ "$(PURGE)" = 1 ]; then \
		echo "  → PURGE=1, removing:"; find "$$cfg_dir" | sed 's/^/      /'; \
		rm -rf "$$cfg_dir" && echo "  ✓ removed $$cfg_dir/"; \
	elif [ -d "$$cfg_dir" ]; then \
		echo "  ℹ  kept $$cfg_dir/ (your configuration). Remove it with: make uninstall PURGE=1"; \
	fi
	@echo ""
	@echo "  $(shell tput setaf 2 2>/dev/null)Uninstall complete.$(shell tput sgr0 2>/dev/null)"
	@echo "  ℹ  User data preserved at ~/.x0ne/ (sessions, snapshots, goals, memory, artifacts)."
	@echo "     To wipe it too, run:  rm -rf ~/.x0ne/"

claude-add: install-runtime install-orchestrator ## Install runtime + orchestrator + register both as Claude Code MCP servers (runtime alone + gateway with goal/plan/simulate)
	@command -v claude >/dev/null 2>&1 || { \
		echo ""; \
		echo "  ✗ The 'claude' CLI is not in PATH."; \
		echo "    Install Claude Code first: https://claude.ai/code"; \
		echo ""; \
		exit 1; \
	}
	@# Entry 1: 'x0ne' = runtime only (70 MCP tools, measured 2026-10-16; no orchestrator features)
	@#
	@# SECURITY: `--config` is not optional here. Registered without it, the
	@# runtime falls through `find_and_load` onto the CLI reference profile
	@# (policy.mode = "auto", default_effect = "allow"), and an MCP client with
	@# no human behind it gets unattended host command execution — measured
	@# 2026-08-15, `run_command {"command":"id"}` returned the host uid. This
	@# entry has no orchestrator in front of it, so it gets the readonly
	@# profile. Want write/exec from an MCP client? Use 'x0ne-gateway' below,
	@# which puts the human gate back in the path.
	@mcpcfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/x0ne-runtime-mcp.toml"; \
	if [ ! -e "$$mcpcfg" ]; then \
		echo ""; \
		echo "  ✗ $$mcpcfg is missing. Run 'make install' first."; \
		echo ""; \
		exit 1; \
	fi
	@mcpcfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/x0ne-runtime-mcp.toml"; \
	if claude mcp list 2>/dev/null | grep -qE "^x0ne[: ]"; then \
		echo "  ℹ  'x0ne' MCP server already registered (skipping)"; \
	else \
		claude mcp add x0ne -- x0ne-runtime --mode mcp --config "$$mcpcfg" --log-level error >/dev/null && \
		echo "  ✓ Registered 'x0ne' (runtime, 70 tools, readonly profile)"; \
	fi
	@# Entry 2: 'x0ne-gateway' = orchestrator front-ending the runtime
	@# (the 70 runtime tools proxied + 22 native tools: x0ne_plan, x0ne_simulate,
	@#  x0ne_goal_*, x0ne_artifact_expose…). This is the recommended entry for
	@#  agent loops that benefit from goals + plans + simulate.
	@if claude mcp list 2>/dev/null | grep -qE "^x0ne-gateway[: ]"; then \
		echo "  ℹ  'x0ne-gateway' MCP server already registered (skipping)"; \
	else \
		claude mcp add x0ne-gateway -- x0ne-orchestrator --mcp --downstream-runtime x0ne-runtime --log-level warn >/dev/null && \
		echo "  ✓ Registered 'x0ne-gateway' (orchestrator, 92 tools incl. x0ne_goal_*, x0ne_plan, x0ne_simulate)"; \
	fi
	@echo ""
	@echo "  ── claude mcp list ──"
	@claude mcp list 2>&1 | grep -E "^x0ne" || echo "  (no x0ne entry yet — restart claude)"
	@echo ""
	@echo "  X0NE is now available in Claude Code:"
	@echo "    • mcp__x0ne__*          — 70 runtime tools (read_file, run_command, create_snapshot, …)"
	@echo "    • mcp__x0ne-gateway__*  — same 70 + 22 orchestrator tools (x0ne_goal_set, x0ne_plan, x0ne_simulate, …)"
	@echo "  Every call is policy-gated. MCP calls are not journaled by X0NE."
	@echo ""
	@echo "  Try, in a Claude Code session:"
	@echo "    \"use x0ne_goal_set to track 'finish the auth refactor and make tests pass'\""

claude-remove: ## Unregister 'x0ne' AND 'x0ne-gateway' from Claude Code (no uninstall of binaries)
	@claude mcp remove x0ne >/dev/null 2>&1 && echo "  ✓ Removed 'x0ne' from Claude Code" || echo "  ℹ  'x0ne' was not registered"
	@claude mcp remove x0ne-gateway >/dev/null 2>&1 && echo "  ✓ Removed 'x0ne-gateway' from Claude Code" || echo "  ℹ  'x0ne-gateway' was not registered"

mcp-codex: codex-add ## Alias: install runtime + orchestrator + register both with Codex

codex-add: install-runtime install-orchestrator ## Install runtime + orchestrator + register both as Codex MCP servers
	@mkdir -p "$(HOME)/.codex"
	@touch "$(HOME)/.codex/config.toml"
	@# SECURITY: same `--config` rule as `claude-add` above — without it the
	@# runtime refuses to start in MCP mode (`check_headless_posture`), and
	@# until 2026-09-11 this target registered it that way and printed
	@# "✓ Registered" (audit 04-F01).
	@mcpcfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/x0ne-runtime-mcp.toml"; \
	if [ ! -e "$$mcpcfg" ]; then \
		echo ""; \
		echo "  ✗ $$mcpcfg is missing. Run 'make install' first."; \
		echo ""; \
		exit 1; \
	fi
	@# Strip any existing x0ne blocks
	@# so the entries stay idempotent (re-running this target replaces, not duplicates).
	@mcpcfg="$${XDG_CONFIG_HOME:-$$HOME/.config}/x0ne/x0ne-runtime-mcp.toml"; \
	tmp="$$(mktemp)"; \
	awk 'BEGIN{skip=0} \
	     /^\[mcp_servers\.x0ne\]/{skip=1; next} \
	     /^\[mcp_servers\.x0ne-gateway\]/{skip=1; next} \
	     /^\[/{skip=0} \
	     !skip{print}' "$(HOME)/.codex/config.toml" > "$$tmp"; \
	mv "$$tmp" "$(HOME)/.codex/config.toml"; \
	{ \
		echo ""; \
		echo "[mcp_servers.x0ne]"; \
		echo 'command = "$(HOME)/.cargo/bin/x0ne-runtime"'; \
		echo 'args = ["--mode", "mcp", "--config", "'"$$mcpcfg"'", "--log-level", "error"]'; \
		echo ""; \
		echo "[mcp_servers.x0ne-gateway]"; \
		echo 'command = "$(HOME)/.cargo/bin/x0ne-orchestrator"'; \
		echo 'args = ["--mcp", "--downstream-runtime", "x0ne-runtime", "--log-level", "warn"]'; \
	} >> "$(HOME)/.codex/config.toml"
	@echo "  ✓ Registered 'x0ne' (runtime) + 'x0ne-gateway' (orchestrator) in ~/.codex/config.toml"
	@echo ""
	@echo "  ── ~/.codex/config.toml ──"
	@grep -A3 '^\[mcp_servers\.x0ne' "$(HOME)/.codex/config.toml" || true
	@echo ""
	@echo "  Restart Codex to load the new MCP servers into the tool context."

codex-remove: ## Unregister 'x0ne' AND 'x0ne-gateway' from Codex config (no uninstall of binaries)
	@tmp="$$(mktemp)"; \
	awk 'BEGIN{skip=0} \
	     /^\[mcp_servers\.x0ne\]/{skip=1; next} \
	     /^\[mcp_servers\.x0ne-gateway\]/{skip=1; next} \
	     /^\[mcp_servers\.x0ne\]/{skip=1; next} \
	     /^\[mcp_servers\.x0ne-gateway\]/{skip=1; next} \
	     /^\[/{skip=0} \
	     !skip{print}' "$(HOME)/.codex/config.toml" > "$$tmp"; \
	mv "$$tmp" "$(HOME)/.codex/config.toml"; \
	echo "  ✓ Removed 'x0ne' and 'x0ne-gateway' MCP servers from ~/.codex/config.toml if present"


setup-ssh-aliases: ## Write ~/.ssh/config.d/x0ne.conf: SSH multiplexing (ControlMaster) for `x0ne-*` aliases only
	@bash scripts/setup-ssh-aliases.sh

test-ssh-aliases: ## Check setup-ssh-aliases with `ssh -G`, in a throwaway container
	podman run --rm -v "$(CURDIR)":/src:ro docker.io/library/debian:bookworm-slim \
	  bash -c 'apt-get -qq update >/dev/null 2>&1; apt-get -qq install -y openssh-client make >/dev/null 2>&1; cp -r /src/Makefile /src/scripts /tmp/ && cd /tmp && scripts/test-setup-ssh-aliases.sh'


# ── Dataset / training pipeline ────────────────────────────────────────────────
# All six commands live behind one unified binary `x0ne-dataset` (build,
# index, query, export sft|dpo, eval, schema). The Makefile targets stay as
# convenience wrappers — same UX as before — but build only ONE binary.

DATASET_BIN := $(BIN)/x0ne-dataset

$(DATASET_BIN):
	@cargo build --release --bin x0ne-dataset

dataset: $(DATASET_BIN) ## Extract trajectories from ~/.x0ne/sessions/*.jsonl. Re-runnable; idempotent (incremental).
	@$(DATASET_BIN) build
	@echo ""
	@echo "  next: make dataset-index    # build the RAG index"
	@echo "        make dataset-query Q=\"make me a fun website\""

dataset-index: $(DATASET_BIN) ## Build the RAG index. Auto-detects ollama nomic-embed-text; falls back to TF-IDF.
	@$(DATASET_BIN) index
	@echo ""
	@echo "  upgrade quality: ollama pull nomic-embed-text   # then re-run this target"

dataset-query: $(DATASET_BIN) ## Smoke-test retrieval. Use Q=\"...\" to set the query, K=N for top-K (default 5).
	@$(DATASET_BIN) query "$(Q)" --k=$(or $(K),5)

export-schema: $(DATASET_BIN) ## Export Action enum → action_schema.json (JSON Schema) + action.gbnf (llama.cpp grammar).
	@$(DATASET_BIN) schema

dataset-sft: $(DATASET_BIN) ## Convert trajectories to ChatML SFT pairs (sft_v1.jsonl). Feed into axolotl/unsloth/TRL.
	@$(DATASET_BIN) export sft

dataset-dpo: $(DATASET_BIN) ## Auto-mine DPO preference pairs (dpo_v1.jsonl). Feed into TRL DPOTrainer.
	@$(DATASET_BIN) export dpo

eval: $(DATASET_BIN) ## Score how well a model emits valid Pipelines. Use MODEL=name HOLDOUT=N LOSS=1.
	@$(DATASET_BIN) eval --model=$(or $(MODEL),ollama/gemma4:31b) --holdout=$(or $(HOLDOUT),20) $(if $(LOSS),--estimate-loss,)

dataset-refresh: $(DATASET_BIN) ## Incremental rebuild of EVERYTHING — trajectories + index + SFT + DPO + schema. One bin, one build.
	@$(DATASET_BIN) build $(if $(FULL),--full,) | tail -3
	@$(DATASET_BIN) index | tail -3
	@$(DATASET_BIN) export sft | tail -2
	@$(DATASET_BIN) export dpo | tail -2
	@$(DATASET_BIN) schema | tail -2
	@echo ""
	@echo "  ✓ dataset refreshed. To wire into every x0ne session, add to ~/.bashrc:"
	@echo "      alias x0ne='x0ne && (cd $(PWD) && make dataset-refresh > /dev/null 2>&1 &)'"

run: build ## Build and run x0ne (CLI mode)
	$(X0NE) $(BIN)/x0ne

run-orchestrator: x0ne-orchestrator ## Build and run x0ne-orchestrator (headless HTTP/SSE)
	$(X0NE) $(BIN)/x0ne-orchestrator

# Backwards-compat alias — old name was `run-server`.
run-server: run-orchestrator

run-tui: build ## Build and run x0ne (TUI mode)
	$(X0NE) $(BIN)/x0ne --tui

run-desktop: build ## Run with --desktop (mouse/keyboard/screenshot actions)
	$(X0NE) $(BIN)/x0ne --desktop

run-matrix: build ## Run in matrix mode (LLM-controlled terminal canvas)
	$(X0NE) $(BIN)/x0ne --matrix

run-mcp: build ## Run x0ne-runtime in MCP mode (for Claude Code, Cursor…)
	$(X0NE) $(CARGO) run -p x0ne-runtime -- --mode mcp

audit-mcp: ## Print the current MCP tool surface, docs claim, and Claude registration status
	$(CARGO) run -q -p x0ne-runtime --bin x0ne-mcp-audit

# ── Incus container isolation ──────────────────────────────────────────────────

sync-incus: ## Push native x0ne-runtime binary + config into the remote Incus container
	@echo "  → building x0ne-runtime (native release)"
	$(CARGO) build -p x0ne-runtime --release
	@echo "  → stopping running x0ne-runtime (if any)"
	@incus exec $(INCUS_TARGET) -- killall x0ne-runtime >/dev/null 2>&1 || true
	@sleep 0.3
	@echo "  → pushing binary to '$(INCUS_TARGET)'"
	@incus file push $(CURDIR)/target/release/x0ne-runtime $(INCUS_TARGET)/opt/x0ne-runtime
	@incus exec $(INCUS_TARGET) -- chmod +x $(INCUS_CORE_BIN)
	@incus file push $(CURDIR)/x0ne-runtime/x0ne-runtime-sandbox.toml $(INCUS_TARGET)/root/x0ne-runtime.toml
	@incus exec $(INCUS_TARGET) -- mkdir -p /root/.x0ne/snapshots
	@echo "  ✓ x0ne-runtime ready in '$(INCUS_TARGET)'"

sync-incus-musl: ## Build glibc-2.35 compat binary + push to Incus container
	@# Nothing is installed on the host: a missing tool stops with the command to run.
	@which cargo-zigbuild > /dev/null 2>&1 || { echo "✗ cargo-zigbuild missing. Install it yourself: cargo install cargo-zigbuild"; exit 1; }
	@which zig > /dev/null 2>&1 || { echo "✗ zig missing. Install it yourself (distribution package, or: pip install --user ziglang)"; exit 1; }
	@rustup target list --installed 2>/dev/null | grep -qx x86_64-unknown-linux-gnu || { echo "✗ target missing. Add it yourself: rustup target add x86_64-unknown-linux-gnu"; exit 1; }
	$(CARGO) zigbuild -p x0ne-runtime --release --target x86_64-unknown-linux-gnu.2.35
	@# Skip the push when the container's binary is already newer than the
	@# freshly-built one. Saves ~15–30 s on every `make run-incus` when the
	@# x0ne-runtime sources haven't changed.
	@local_bin="$(TARGET)/x86_64-unknown-linux-gnu/release/x0ne-runtime"; \
	 local_mtime=$$(stat -c %Y "$$local_bin"); \
	 remote_mtime=$$(incus exec $(INCUS_TARGET) -- stat -c %Y $(INCUS_CORE_BIN) 2>/dev/null || echo 0); \
	 if [ "$$remote_mtime" -ge "$$local_mtime" ]; then \
	     echo "  ✓ x0ne-runtime in '$(INCUS_TARGET)' is up-to-date — skipping push"; \
	 else \
	     echo "  → stopping running x0ne-runtime (if any)"; \
	     incus exec $(INCUS_TARGET) -- killall x0ne-runtime >/dev/null 2>&1 || true; \
	     sleep 0.3; \
	     sz=$$(stat -c %s "$$local_bin" | numfmt --to=iec 2>/dev/null || stat -c %s "$$local_bin"); \
	     echo "  → pushing binary ($$sz) to '$(INCUS_TARGET)'…"; \
	     incus file push "$$local_bin" $(INCUS_TARGET)/opt/x0ne-runtime; \
	     incus exec $(INCUS_TARGET) -- chmod +x $(INCUS_CORE_BIN); \
	     incus file push $(CURDIR)/x0ne-runtime/x0ne-runtime-sandbox.toml $(INCUS_TARGET)/root/x0ne-runtime.toml; \
	     incus exec $(INCUS_TARGET) -- mkdir -p /root/.x0ne/snapshots; \
	     echo "  ✓ glibc-2.35 binary deployed to '$(INCUS_TARGET)'"; \
	     if incus snapshot list $(INCUS_TARGET) -f csv 2>/dev/null | awk -F, '{print $$1}' | grep -qxF $(INCUS_SNAPSHOT_NAME); then \
	         echo "  → binary changed — invalidating stale snapshot '$(INCUS_SNAPSHOT_NAME)'"; \
	         incus snapshot delete $(INCUS_TARGET) $(INCUS_SNAPSHOT_NAME) >/dev/null 2>&1 || true; \
	     fi; \
	 fi

force-sync-incus-musl: ## Force re-push x0ne-runtime to the Incus container (bypass mtime check)
	@incus exec $(INCUS_TARGET) -- rm -f $(INCUS_CORE_BIN) 2>/dev/null || true
	@$(MAKE) sync-incus-musl

setup-incus: build ## From-zero Incus onboarding: install daemon if missing (btrfs storage), create container, deploy x0ne-runtime, baseline snapshot
	@if ! command -v incus >/dev/null 2>&1; then \
		echo "  ✗ 'incus' CLI not found. Nothing was installed. To install it (sudo):"; \
		echo "      bash scripts/install-sandbox-backend.sh --backend incus"; \
		exit 1; \
	fi
	@echo "  → sandbox target: '$(INCUS_TARGET)'"
	@# Use `incus list` rather than `incus info` for existence checks —
	@# `incus info` returns non-zero on some perfectly valid states
	@# (stopped, in-transition, or when the CLI is pointing at a project
	@# the user can't fully introspect), which used to trigger a bogus
	@# `incus launch` and the noisy "Instance already exists" error.
	@if incus list $(INCUS_REMOTE): -f csv -c n 2>/dev/null | grep -qxF $(INCUS_CONTAINER); then \
		echo "  → container '$(INCUS_TARGET)' already exists"; \
		state=$$(incus list $(INCUS_REMOTE): -f csv -c ns $(INCUS_CONTAINER) 2>/dev/null | awk -F, '{print $$2}'); \
		if [ "$$state" != "RUNNING" ]; then \
			echo "  → starting container (was: $$state)…"; \
			incus start $(INCUS_TARGET) 2>/dev/null || true; \
			sleep 1; \
		fi; \
	else \
		echo "  → creating container '$(INCUS_TARGET)' (Debian 13: host-built binaries need glibc 2.39)…"; \
		if ! incus launch images:debian/13 $(INCUS_TARGET); then \
			echo "  ✗ incus launch failed — aborting"; exit 1; \
		fi; \
		echo "  → waiting for network…"; \
		sleep 3; \
		for i in 1 2 3 4 5; do \
			incus exec $(INCUS_TARGET) -- ping -c1 -W2 deb.debian.org >/dev/null 2>&1 && break; \
			echo "    retry $$i/5…"; sleep 2; \
		done; \
		echo "  → installing packages…"; \
		incus exec $(INCUS_TARGET) -- bash -c "apt-get update -qq && apt-get install -y -qq python3 python3-pip curl git jq procps net-tools >/dev/null 2>&1"; \
		echo "  ✓ packages installed"; \
	fi
	@$(MAKE) --no-print-directory sync-incus-musl
	@# Capture a baseline snapshot once the clean environment is fully wired.
	@# On the btrfs-backed prod-infra pool, `snapshot create`/`restore` are
	@# instant (< 1 s) — the foundation of our fresh-sandbox-every-run flow.
	@if ! incus snapshot list $(INCUS_TARGET) -f csv 2>/dev/null | awk -F, '{print $$1}' | grep -qxF $(INCUS_SNAPSHOT_NAME); then \
		echo "  → capturing baseline snapshot '$(INCUS_SNAPSHOT_NAME)'…"; \
		incus snapshot create $(INCUS_TARGET) $(INCUS_SNAPSHOT_NAME); \
		echo "  ✓ baseline ready — every 'make run-incus*' starts from a clean state"; \
	fi
	@echo ""
	@echo "  ✓ sandbox '$(INCUS_TARGET)' ready — python3, git, curl, jq installed"
	@echo "    run: make run-incus-tui"

# ── Snapshot-based fresh-start workflow ────────────────────────────────────────

recover-sandbox-artifacts: ## Pull artifacts stranded inside x0ne-sandbox out to ~/.x0ne/artifacts/ (for old containers from before the orchestrator router was the host-side entry point)
	@set -e ; \
	if ! incus list $(INCUS_REMOTE): -f csv -c n 2>/dev/null | grep -qxF $(INCUS_CONTAINER); then \
		echo "  ✗ container '$(INCUS_TARGET)' not found"; exit 1; \
	fi ; \
	if ! incus exec $(INCUS_TARGET) -- test -f /root/.x0ne/artifacts/registry.json; then \
		echo "  → no artifacts inside '$(INCUS_TARGET)' — nothing to recover"; exit 0; \
	fi ; \
	echo "  → pulling registry from $(INCUS_TARGET)…" ; \
	mkdir -p $(HOME)/.x0ne/artifacts ; \
	tmp=$$(mktemp -d) ; trap 'rm -rf "$$tmp"' EXIT ; \
	incus file pull $(INCUS_TARGET)/root/.x0ne/artifacts/registry.json $$tmp/registry.json ; \
	reg=$(HOME)/.x0ne/artifacts/registry.json ; \
	if [ -f $$reg ]; then cp -p $$reg $$reg.bak-$$(date +%Y%m%d%H%M%S) ; echo "  → previous registry kept as $$reg.bak-*" ; fi ; \
	echo "  → rewriting in-container paths to host paths…" ; \
	sed 's|/root/.x0ne/|$(HOME)/.x0ne/|g' $$tmp/registry.json > $$reg ; \
	echo "  → pulling artifact files…" ; \
	if incus exec $(INCUS_TARGET) -- test -d /root/.x0ne/artifacts/ephemeral; then \
		incus file pull --recursive $(INCUS_TARGET)/root/.x0ne/artifacts/ephemeral $$tmp/ephemeral ; \
		mkdir -p $(HOME)/.x0ne/artifacts/ephemeral ; \
		cp -an $$tmp/ephemeral/ephemeral/. $(HOME)/.x0ne/artifacts/ephemeral/ ; \
	fi ; \
	if incus exec $(INCUS_TARGET) -- test -d /root/.x0ne/artifacts/saved; then \
		incus file pull --recursive $(INCUS_TARGET)/root/.x0ne/artifacts/saved $$tmp/saved ; \
		mkdir -p $(HOME)/.x0ne/artifacts/saved ; \
		cp -an $$tmp/saved/saved/. $(HOME)/.x0ne/artifacts/saved/ ; \
	fi ; \
	count=$$(grep -c '"id":' $(HOME)/.x0ne/artifacts/registry.json 2>/dev/null || echo 0) ; \
	echo "  ✓ recovered $$count artifact(s) — start x0ne and they're served at http://localhost:7800/a/<id>/"

snapshot-incus: ## Re-create the '$(INCUS_SNAPSHOT_NAME)' baseline from the CURRENT container state
	@if incus snapshot list $(INCUS_TARGET) -f csv 2>/dev/null | awk -F, '{print $$1}' | grep -qxF $(INCUS_SNAPSHOT_NAME); then \
		echo "  → deleting old snapshot '$(INCUS_SNAPSHOT_NAME)'…"; \
		incus snapshot delete $(INCUS_TARGET) $(INCUS_SNAPSHOT_NAME); \
	fi
	@echo "  → creating snapshot '$(INCUS_SNAPSHOT_NAME)' of '$(INCUS_TARGET)'…"
	@incus snapshot create $(INCUS_TARGET) $(INCUS_SNAPSHOT_NAME)
	@echo "  ✓ new baseline captured"

reset-incus: ## Restore '$(INCUS_TARGET)' from the '$(INCUS_SNAPSHOT_NAME)' snapshot — guarantees a fresh state
	@if ! incus snapshot list $(INCUS_TARGET) -f csv 2>/dev/null | awk -F, '{print $$1}' | grep -qxF $(INCUS_SNAPSHOT_NAME); then \
		echo "  ⚠ no baseline snapshot found — capturing current state first"; \
		$(SUBMAKE) --no-print-directory snapshot-incus; \
	else \
		echo "  → restoring '$(INCUS_TARGET)' from '$(INCUS_SNAPSHOT_NAME)' (fresh state)…"; \
		incus stop $(INCUS_TARGET) --force >/dev/null 2>&1 || true; \
		incus snapshot restore $(INCUS_TARGET) $(INCUS_SNAPSHOT_NAME); \
		incus start $(INCUS_TARGET) >/dev/null 2>&1 || true; \
		for i in 1 2 3 4 5 6 7 8 9 10; do \
			incus exec $(INCUS_TARGET) -- true >/dev/null 2>&1 && break; \
			sleep 0.3; \
		done; \
		echo "  ✓ sandbox restored to pristine state"; \
	fi

rebuild-baseline: setup-incus ## Delete & re-create the baseline snapshot from a freshly-set-up container
	@$(MAKE) --no-print-directory snapshot-incus

run-incus: build ## Run x0ne inside a throwaway Incus container (binaries + ~/x0ne-work only)
	@X0NE_BIN_DIR=$(BIN) bash scripts/run-sandboxed.sh incus $(ARGS)

run-podman: build ## Same guarantees as run-incus, on Podman
	@X0NE_BIN_DIR=$(BIN) bash scripts/run-sandboxed.sh podman $(ARGS)

sandbox-image: sandbox-image-podman sandbox-image-incus ## Build the tooled sandbox images once (rust, python, build-essential, git, tmux; no nmap)

sandbox-image-podman: ## Tooled image for run-podman: localhost/x0ne-sbx-tools:trixie
	podman build -t localhost/x0ne-sbx-tools:trixie -f scripts/sandbox-image/Containerfile scripts/sandbox-image

sandbox-egress-image: ## Egress proxy of run-incus/run-podman: localhost/x0ne-sbx-egress:trixie
	podman build -t localhost/x0ne-sbx-egress:trixie -f scripts/sandbox-egress/Containerfile scripts/sandbox-egress

test-sandbox-egress: build ## Attack check of the sandbox egress allowlist, both backends
	bash scripts/test-sandbox-egress-allowlist.sh
	X0NE_BIN_DIR=$(BIN) bash scripts/test-sandbox-egress.sh podman
	X0NE_BIN_DIR=$(BIN) bash scripts/test-sandbox-egress.sh incus

sandbox-image-incus: ## Tooled image for run-incus: local alias x0ne-sbx-tools (debian/13)
	bash scripts/sandbox-image/build-incus.sh

test-sandbox-image: build ## Start the binaries in the pinned sandbox images (glibc, env forwarding)
	X0NE_BIN_DIR=$(BIN) bash scripts/test-sandbox-image.sh incus
	X0NE_BIN_DIR=$(BIN) bash scripts/test-sandbox-image.sh podman

test-run-sandboxed: build ## Attack check of run-incus/run-podman (canaries, fake key)
	X0NE_BIN_DIR=$(BIN) bash scripts/test-run-sandboxed.sh incus
	X0NE_BIN_DIR=$(BIN) bash scripts/test-run-sandboxed.sh podman

run-incus-runtime: build ## CLI on the host, x0ne-runtime in the Incus sandbox (fresh snapshot)
	@$(MAKE) --no-print-directory setup-incus
	@$(MAKE) --no-print-directory reset-incus
	@X0NE_FORWARD_ENV="$(X0NE_FORWARD_ENV)" bash scripts/sandbox-env.sh $(INCUS_TARGET)
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="incus exec $(INCUS_RUN_ENV) $(INCUS_TARGET) -- $(INCUS_RUNTIME_WRAP) $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne

run-incus-tui: build ## Build, restore snapshot, run TUI (fresh sandbox every time)
	@$(MAKE) --no-print-directory setup-incus
	@$(MAKE) --no-print-directory reset-incus
	@X0NE_FORWARD_ENV="$(X0NE_FORWARD_ENV)" bash scripts/sandbox-env.sh $(INCUS_TARGET)
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="incus exec $(INCUS_RUN_ENV) $(INCUS_TARGET) -- $(INCUS_RUNTIME_WRAP) $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne --tui

run-incus-matrix: build ## Build, restore snapshot, run Matrix mode (fresh sandbox every time)
	@$(MAKE) --no-print-directory setup-incus
	@$(MAKE) --no-print-directory reset-incus
	@X0NE_FORWARD_ENV="$(X0NE_FORWARD_ENV)" bash scripts/sandbox-env.sh $(INCUS_TARGET)
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="incus exec $(INCUS_RUN_ENV) $(INCUS_TARGET) -- $(INCUS_RUNTIME_WRAP) $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne --matrix

run-incus-dirty: build setup-incus ## Like run-incus, but KEEP the previous state (no snapshot restore)
	@X0NE_FORWARD_ENV="$(X0NE_FORWARD_ENV)" bash scripts/sandbox-env.sh $(INCUS_TARGET)
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="incus exec $(INCUS_RUN_ENV) $(INCUS_TARGET) -- $(INCUS_RUNTIME_WRAP) $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne

diagnose-incus: ## Check if x0ne-runtime runs inside the container
	@echo "  → container state"
	@incus list $(INCUS_TARGET) --format compact 2>/dev/null || echo "  [x] container not found — run: make setup-incus"
	@echo "  → binary presence"
	@incus exec $(INCUS_CONTAINER) -- ls -lh $(INCUS_CORE_BIN) 2>/dev/null || echo "  [x] binary missing — run: make sync-incus"
	@echo "  → glibc version in container"
	@incus exec $(INCUS_CONTAINER) -- ldd --version 2>&1 | head -2 || true
	@echo "  → binary exec test"
	@incus exec $(INCUS_CONTAINER) -- $(INCUS_CORE_BIN) --version 2>&1 || echo "  [x] binary crashed — run: make sync-incus-musl"
	@echo "  → baseline snapshot"
	@if incus snapshot list $(INCUS_TARGET) -f csv 2>/dev/null | awk -F, '{print $$1}' | grep -qxF $(INCUS_SNAPSHOT_NAME); then \
		incus snapshot list $(INCUS_TARGET) 2>/dev/null | grep -E "NAME|$(INCUS_SNAPSHOT_NAME)"; \
	else \
		echo "  [x] no '$(INCUS_SNAPSHOT_NAME)' snapshot — run: make rebuild-baseline"; \
	fi

test-mock-incus: sync-incus ## Smoke test: pipe JSON actions into containerised x0ne-runtime
	@echo ""
	@echo "━━━ smoke test 1/3 : SystemInfo ━━━"
	@printf '{"type":"SystemInfo"}\n' | incus exec $(INCUS_TARGET) -- \
	    $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "━━━ smoke test 2/3 : ListDir / ━━━"
	@printf '{"type":"ListDir","path":"/","recursive":false}\n' | incus exec $(INCUS_TARGET) -- \
	    $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "━━━ smoke test 3/3 : RunCommand echo ━━━"
	@printf '{"type":"RunCommand","command":"echo","args":["hello from x0ne-sandbox"]}\n' | incus exec $(INCUS_TARGET) -- \
	    $(INCUS_CORE_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "  ✓ 3/3 smoke tests returned success:true"

# ── Podman sandbox workflow ───────────────────────────────────────────────────
#
# Full mirror of the Incus workflow above. The orchestrator doesn't care
# which backend is used — `ContainerRuntime::detect_from_sandbox_cmd` picks
# the right one from the first token of `X0NE_SANDBOX_CMD`. That means
# canvases, `RunCommand`, file ops, etc. all work unchanged against
# `run-podman*` targets.

setup-podman: build ## From-zero Podman onboarding: install daemon if missing, create container, deploy x0ne-runtime, commit baseline image
	@if ! command -v podman >/dev/null 2>&1; then \
		echo "  ✗ 'podman' CLI not found. Nothing was installed. To install it (sudo):"; \
		echo "      bash scripts/install-sandbox-backend.sh --backend podman"; \
		exit 1; \
	fi
	@echo "  → podman target: '$(PODMAN_CONTAINER)'"
	@if podman container exists $(PODMAN_CONTAINER) 2>/dev/null; then \
		state=$$(podman inspect -f '{{.State.Status}}' $(PODMAN_CONTAINER) 2>/dev/null); \
		if [ "$$state" != "running" ]; then \
			echo "  → starting container (was: $$state)…"; \
			podman start $(PODMAN_CONTAINER) >/dev/null; \
			sleep 0.5; \
		else \
			echo "  → container '$(PODMAN_CONTAINER)' already running"; \
		fi; \
	else \
		echo "  → creating container '$(PODMAN_CONTAINER)' ($(PODMAN_IMAGE_BASE))…"; \
		podman run -d --name $(PODMAN_CONTAINER) $(PODMAN_IMAGE_BASE) sleep infinity >/dev/null; \
		echo "  → waiting for network…"; \
		sleep 2; \
		for i in 1 2 3 4 5; do \
			podman exec $(PODMAN_CONTAINER) sh -c "getent hosts deb.debian.org" >/dev/null 2>&1 && break; \
			echo "    retry $$i/5…"; sleep 2; \
		done; \
		echo "  → installing packages…"; \
		podman exec $(PODMAN_CONTAINER) bash -c "apt-get update -qq && apt-get install -y -qq python3 python3-pip curl git jq procps net-tools >/dev/null 2>&1"; \
		echo "  ✓ packages installed"; \
	fi
	@$(MAKE) --no-print-directory sync-podman-musl
	@# Capture a baseline image once the environment is wired — symmetric to
	@# the Incus snapshot flow. `podman commit` of a light container is ~1 s.
	@if ! podman image exists $(PODMAN_BASELINE_IMG) 2>/dev/null; then \
		echo "  → committing baseline image '$(PODMAN_BASELINE_IMG)'…"; \
		podman commit -q $(PODMAN_CONTAINER) $(PODMAN_BASELINE_IMG) >/dev/null; \
		echo "  ✓ baseline ready — every 'make run-podman*' starts from a clean state"; \
	fi
	@echo ""
	@echo "  ✓ sandbox '$(PODMAN_CONTAINER)' ready — python3, git, curl, jq installed"
	@echo "    run: make run-podman-tui"

sync-podman-musl: ## Build glibc-2.35 compat binary + push to Podman container (skips if up-to-date)
	@# Nothing is installed on the host: a missing tool stops with the command to run.
	@which cargo-zigbuild > /dev/null 2>&1 || { echo "✗ cargo-zigbuild missing. Install it yourself: cargo install cargo-zigbuild"; exit 1; }
	@which zig > /dev/null 2>&1 || { echo "✗ zig missing. Install it yourself (distribution package, or: pip install --user ziglang)"; exit 1; }
	@rustup target list --installed 2>/dev/null | grep -qx x86_64-unknown-linux-gnu || { echo "✗ target missing. Add it yourself: rustup target add x86_64-unknown-linux-gnu"; exit 1; }
	$(CARGO) zigbuild -p x0ne-runtime --release --target x86_64-unknown-linux-gnu.2.35
	@# Same mtime-based skip as sync-incus-musl — avoids re-pushing an
	@# identical 40 MB binary on every `make run-podman`.
	@local_bin="$(TARGET)/x86_64-unknown-linux-gnu/release/x0ne-runtime"; \
	 local_mtime=$$(stat -c %Y "$$local_bin"); \
	 remote_mtime=$$(podman exec $(PODMAN_CONTAINER) stat -c %Y $(PODMAN_RUNTIME_BIN) 2>/dev/null || echo 0); \
	 if [ "$$remote_mtime" -ge "$$local_mtime" ]; then \
	     echo "  ✓ x0ne-runtime in '$(PODMAN_CONTAINER)' is up-to-date — skipping push"; \
	 else \
	     echo "  → stopping running x0ne-runtime (if any)"; \
	     podman exec $(PODMAN_CONTAINER) killall x0ne-runtime >/dev/null 2>&1 || true; \
	     sleep 0.3; \
	     sz=$$(stat -c %s "$$local_bin" | numfmt --to=iec 2>/dev/null || stat -c %s "$$local_bin"); \
	     echo "  → pushing binary ($$sz) to '$(PODMAN_CONTAINER)'…"; \
	     podman cp "$$local_bin" $(PODMAN_CONTAINER):$(PODMAN_RUNTIME_BIN); \
	     podman exec $(PODMAN_CONTAINER) chmod +x $(PODMAN_RUNTIME_BIN); \
	     podman cp $(CURDIR)/x0ne-runtime/x0ne-runtime-sandbox.toml $(PODMAN_CONTAINER):/root/x0ne-runtime.toml; \
	     podman exec $(PODMAN_CONTAINER) mkdir -p /root/.x0ne/snapshots; \
	     echo "  ✓ glibc-2.35 binary deployed to '$(PODMAN_CONTAINER)'"; \
	     if podman image exists $(PODMAN_BASELINE_IMG) 2>/dev/null; then \
	         echo "  → binary changed — invalidating stale baseline image '$(PODMAN_BASELINE_IMG)'"; \
	         podman rmi -f $(PODMAN_BASELINE_IMG) >/dev/null 2>&1 || true; \
	     fi; \
	 fi

force-sync-podman-musl: ## Force re-push x0ne-runtime to the Podman container (bypass mtime check)
	@podman exec $(PODMAN_CONTAINER) rm -f $(PODMAN_RUNTIME_BIN) 2>/dev/null || true
	@$(MAKE) sync-podman-musl

snapshot-podman: ## Re-create the baseline image from the CURRENT container state
	@if podman image exists $(PODMAN_BASELINE_IMG) 2>/dev/null; then \
		echo "  → removing old baseline image '$(PODMAN_BASELINE_IMG)'…"; \
		podman rmi -f $(PODMAN_BASELINE_IMG) >/dev/null 2>&1 || true; \
	fi
	@echo "  → committing baseline image '$(PODMAN_BASELINE_IMG)' from '$(PODMAN_CONTAINER)'…"
	@podman commit -q $(PODMAN_CONTAINER) $(PODMAN_BASELINE_IMG) >/dev/null
	@echo "  ✓ new baseline captured"

reset-podman: ## Remove current container, re-create a fresh one from the baseline image
	@if ! podman image exists $(PODMAN_BASELINE_IMG) 2>/dev/null; then \
		echo "  ⚠ no baseline image found — capturing current state first"; \
		$(SUBMAKE) --no-print-directory snapshot-podman; \
	fi
	@echo "  → restoring '$(PODMAN_CONTAINER)' from '$(PODMAN_BASELINE_IMG)' (fresh state)…"
	@podman rm -f $(PODMAN_CONTAINER) >/dev/null 2>&1 || true
	@podman run -d --name $(PODMAN_CONTAINER) $(PODMAN_BASELINE_IMG) sleep infinity >/dev/null
	@for i in 1 2 3 4 5 6 7 8 9 10; do \
		podman exec $(PODMAN_CONTAINER) true >/dev/null 2>&1 && break; \
		sleep 0.3; \
	done
	@echo "  ✓ sandbox restored to pristine state"

rebuild-baseline-podman: setup-podman ## Delete & re-create the Podman baseline image from a freshly-set-up container
	@$(MAKE) --no-print-directory snapshot-podman

run-podman-runtime: build ## Build, restore baseline, run CLI (fresh Podman sandbox every time)
	@$(MAKE) --no-print-directory setup-podman
	@$(MAKE) --no-print-directory reset-podman
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="podman exec $(PODMAN_EXEC_FLAGS) $(PODMAN_RUN_ENV) $(PODMAN_CONTAINER) $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne

run-podman-tui: build ## Build, restore baseline, run TUI (fresh Podman sandbox every time)
	@$(MAKE) --no-print-directory setup-podman
	@$(MAKE) --no-print-directory reset-podman
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="podman exec $(PODMAN_EXEC_FLAGS) $(PODMAN_RUN_ENV) $(PODMAN_CONTAINER) $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne --tui

run-podman-matrix: build ## Build, restore baseline, run Matrix mode (fresh Podman sandbox every time)
	@$(MAKE) --no-print-directory setup-podman
	@$(MAKE) --no-print-directory reset-podman
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="podman exec $(PODMAN_EXEC_FLAGS) $(PODMAN_RUN_ENV) $(PODMAN_CONTAINER) $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne --matrix

run-podman-dirty: build setup-podman ## Like run-podman, but KEEP the previous state (no reset)
	@X0NE_BRIDGE_MODE=sandbox \
	 X0NE_SANDBOX_CMD="podman exec $(PODMAN_EXEC_FLAGS) $(PODMAN_RUN_ENV) $(PODMAN_CONTAINER) $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container" \
	 $(X0NE) $(BIN)/x0ne

diagnose-podman: ## Check if x0ne-runtime runs inside the podman container
	@echo "  → container state"
	@podman ps -a --filter name=$(PODMAN_CONTAINER) --format "table {{.Names}}\t{{.Status}}\t{{.Image}}" 2>/dev/null || echo "  [x] container not found — run: make setup-podman"
	@echo "  → binary presence"
	@podman exec $(PODMAN_CONTAINER) ls -lh $(PODMAN_RUNTIME_BIN) 2>/dev/null || echo "  [x] binary missing — run: make sync-podman-musl"
	@echo "  → glibc version in container"
	@podman exec $(PODMAN_CONTAINER) ldd --version 2>&1 | head -2 || true
	@echo "  → binary exec test"
	@podman exec $(PODMAN_CONTAINER) $(PODMAN_RUNTIME_BIN) --version 2>&1 || echo "  [x] binary crashed — run: make sync-podman-musl"
	@echo "  → baseline image"
	@if podman image exists $(PODMAN_BASELINE_IMG) 2>/dev/null; then \
		podman images --filter reference=$(PODMAN_BASELINE_IMG) --format "table {{.Repository}}:{{.Tag}}\t{{.Size}}\t{{.CreatedSince}}"; \
	else \
		echo "  [x] no baseline image — run: make setup-podman (or rebuild-baseline-podman)"; \
	fi

test-mock-podman: sync-podman-musl ## Smoke test: pipe JSON actions into podman-containerised x0ne-runtime
	@echo ""
	@echo "━━━ smoke test 1/3 : SystemInfo ━━━"
	@printf '{"type":"SystemInfo"}\n' | podman exec -i $(PODMAN_CONTAINER) \
	    $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "━━━ smoke test 2/3 : ListDir / ━━━"
	@printf '{"type":"ListDir","path":"/","recursive":false}\n' | podman exec -i $(PODMAN_CONTAINER) \
	    $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "━━━ smoke test 3/3 : RunCommand echo ━━━"
	@printf '{"type":"RunCommand","command":"echo","args":["hello from x0ne-sandbox-podman"]}\n' | podman exec -i $(PODMAN_CONTAINER) \
	    $(PODMAN_RUNTIME_BIN) --config /root/x0ne-runtime.toml --mode stdio --isolation container 2>&1 | tee /dev/stderr | grep -q '"success":true' || { echo '  ✗ smoke test failed'; exit 1; }
	@echo ""
	@echo "  ✓ 3/3 smoke tests returned success:true"

# ── Dev tools ──────────────────────────────────────────────────────────────────

dev: ## Watch sources, rebuild and restart on every save
	@which cargo-watch > /dev/null 2>&1 || { echo "✗ cargo-watch missing. Install it yourself: cargo install cargo-watch"; exit 1; }
	$(X0NE) $(CAP) cargo watch \
		--watch x0ne-runtime/src \
		--watch x0ne-orchestrator/src \
		--watch x0ne-code/src \
		--watch x0ne-runtime/Cargo.toml \
		--watch x0ne-orchestrator/Cargo.toml \
		--watch x0ne-code/Cargo.toml \
		-s "cargo build --workspace 2>&1 && PATH=$(BIN):$$PATH $(BIN)/x0ne"

demo: build ## Build + send 3 scripted commands (no manual typing)
	@printf 'list files in the current dir\nwhat is the system info ?\ngit status\n/quit\n' | $(X0NE) $(BIN)/x0ne

test: ## Run the full test suite
	$(CARGO_TEST) test --workspace

# Scenario-runner defaults — override on CLI: make test-all INCUS_REMOTE=staging
# Note: INCUS_REMOTE is already declared at the top (sandbox target).
INCUS_IMAGE    ?= images:alpine/edge
INCUS_SNAPSHOT ?=

# Single command to validate the full stack: unit tests + scenarios + Incus.
# Everything. No skipping.
# The four legs of the validation loop. Nothing is committed without them. The unit
# leg aggregates every `test result:` line because cargo truncates the
# summary across targets (the cargo test summary trap); the awk exits
# non-zero on any failure, and pipefail carries cargo's own status.
loop: ## The 4 validation legs: tests (aggregated) + scenarios + clippy + fmt + hook cases
	@echo "── 1/4  cargo test --workspace ──"
	$(CARGO_TEST) test --workspace --no-fail-fast --locked 2>&1 | grep -E "^test result:|panicked|FAILED" | awk '/^test result:/{p+=$$4; f+=$$6} !/^test result:/{print} END {print "PASSED:", p, "FAILED:", f; exit (f>0)}'
	@echo "── 2/4  scenarios ──"
	$(CARGO_TEST) run --bin x0ne-scenario-runner
	@echo "── 3/4  clippy ──"
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	@echo "── 4/4  fmt ──"
	cargo fmt --all -- --check
	@echo "── hook cases ──"
	bash scripts/hooks/test-require-capped-cargo.sh
	sh scripts/hooks/test-x0ne-codex-pretooluse.sh
	@echo "✓ four legs green"

test-all: build loop ## Run EVERYTHING: the four legs + Incus (prod-infra by default)
	@printf '%b\n' "\n\x1b[90m── Incus → $(INCUS_REMOTE) ─────────────────\x1b[0m"
	INCUS_REMOTE="$(INCUS_REMOTE)" \
	INCUS_IMAGE="$(INCUS_IMAGE)" \
	INCUS_SNAPSHOT="$(INCUS_SNAPSHOT)" \
	$(CARGO) run --bin x0ne-scenario-runner -- \
		--scenario x0ne-runtime/tests/scenarios/incus.json --incus
	@printf '%b\n' "\n\x1b[32;1m✓ All done.\x1b[0m\n"

test-all-incus: test-all ## Alias for test-all (Incus always included)

test-matrix: ## Run matrix mode unit tests only
	$(CARGO_TEST) test -p x0ne-orchestrator matrix_mode::tests -- --nocapture

test-desktop: ## Run desktop/Wayland scenarios (requires grim, wlrctl, wtype)
	$(CARGO) run --bin x0ne-scenario-runner -- --desktop

check: ## Fast type-check without linking
	$(CARGO) check --workspace

fix: ## Apply automatic lint fixes
	$(CARGO) fix --workspace --allow-dirty --allow-staged

# ── Primitive testing (no LLM) ─────────────────────────────────────────────────

# One-shot probe: test a single JSON action directly against the executor.
# Usage: make probe ACTION='{"type":"SystemInfo"}'
#        make probe ACTION='{"type":"DiskUsage","path":"."}' SHOW_OUTPUT=1
SHOW_OUTPUT ?=
PROBE_FLAGS  = $(if $(SHOW_OUTPUT),--show-output,)

probe: build ## Probe a single action — make probe ACTION='{"type":"SystemInfo"}'
	@test -n "$(ACTION)" || (echo "Usage: make probe ACTION='{\"type\":\"SystemInfo\"}'"; exit 1)
	$(CARGO) run --bin x0ne-scenario-runner -- --probe '$(ACTION)' $(PROBE_FLAGS)

scenarios: build ## Run all scenario files (no Incus, no LLM required)
	@printf '%b\n' "\n  X0NE Scenario Suite\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"
	$(CARGO_TEST) run --bin x0ne-scenario-runner

scenarios-verbose: build ## Run scenario suite with full output on each action
	$(CARGO_TEST) run --bin x0ne-scenario-runner -- --show-output

# Corpus B: calls Claude Haiku 4.5 (ANTHROPIC_API_KEY, or ~/.config/x0ne-sandbox.env),
# stops at ASM_BENCH_CAP_USD (default 1) measured on the usage the API returns.
asm-bench: build ## Measure the asm-ai valid-program rate (corpus B; calls a model, spends money)
	ASM_BENCH_RUNTIME=$(BIN)/x0ne-runtime ASM_BENCH_OUT=$(TARGET)/asm-bench.jsonl \
	  $(CARGO) test -p x0ne-orchestrator --test corpus_b -- --ignored --nocapture --test-threads=1

# Incus scenario suite — INCUS_REMOTE defaults to the sandbox remote (prod-infra).
# Usage: make scenarios-incus INCUS_REMOTE=prod-infra
scenarios-incus: build ## Run Incus scenarios — make scenarios-incus INCUS_REMOTE=prod-infra
	@test -n "$(INCUS_REMOTE)" || (echo "Usage: make scenarios-incus INCUS_REMOTE=prod-infra"; exit 1)
	@printf '%b\n' "\n  X0NE Incus Scenario Suite  →  remote: $(INCUS_REMOTE)\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"
	@INCUS_REMOTE="$(INCUS_REMOTE)" \
	 INCUS_IMAGE="$(or $(INCUS_IMAGE),images:alpine/edge)" \
	 INCUS_SNAPSHOT="$(INCUS_SNAPSHOT)" \
	$(CARGO) run --bin x0ne-scenario-runner -- \
		--scenario x0ne-runtime/tests/scenarios/incus.json \
		--incus

# ── Visual demo — see the output of every primitive ───────────────────────────

demo-undo: build ## Undo demo: a scripted agent damages a project, the snapshot restores it
	@bash scripts/demo-undo.sh

demo-undo-fast: build ## Same as demo-undo, no pauses — for screencasts
	@X0NE_DEMO_NO_PAUSE=1 bash scripts/demo-undo.sh

test-demo-undo: build ## The undo demo fails loudly on a restore that restored nothing
	X0NE_RUNTIME=$(BIN)/x0ne-runtime bash scripts/test-demo-undo.sh

self-test: build ## Visual self-test: every action rendered through the real X0NE CLI
	@# Force the freshly-built sibling binary — $(PATH)'s x0ne-runtime may be a
	@# stale `cargo install` copy that predates the latest Action variants.
	@# `X0NE_RUNTIME_BIN` is the preferred name; `X0NE_CORE_BIN` is set too for
	@# backward compatibility with older orchestrator builds.
	X0NE_RUNTIME_BIN="$(TARGET)/debug/x0ne-runtime" \
	X0NE_CORE_BIN="$(TARGET)/debug/x0ne-runtime" \
	$(CARGO) run --bin x0ne -- --self-test

demo-incus: build ## Visual demo of Incus primitives (requires INCUS_REMOTE)
	@test -n "$(INCUS_REMOTE)" || (echo "Usage: make demo-incus INCUS_REMOTE=prod-infra"; exit 1)
	@printf '%b\n' "\n  X0NE Incus Visual Demo\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"
	INCUS_REMOTE="$(INCUS_REMOTE)" \
	INCUS_IMAGE="$(or $(INCUS_IMAGE),images:alpine/edge)" \
	INCUS_SNAPSHOT="$(INCUS_SNAPSHOT)" \
	$(CARGO) run --bin x0ne-scenario-runner -- \
		--scenario x0ne-runtime/tests/scenarios/incus.json --incus --show-output

# ── Stress-test prompts (requires LLM) ────────────────────────────────────────

STRESS_PROMPT ?=

stress-test: build ## Run ALL stress-test prompts (requires LLM API key)
	@printf '%b\n' "\n  Running X0NE stress-test battery…\n"
	@bash scripts/stress-prompts.sh

stress-pick: build ## Run a single stress prompt — make stress-pick N=3
	@test -n "$(N)" || (echo "Usage: make stress-pick N=3"; exit 1)
	@bash scripts/stress-prompts.sh --pick $(N)

stress-dry: ## Print all stress prompts without executing
	@bash scripts/stress-prompts.sh --dry-run
