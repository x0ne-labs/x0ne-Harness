# Changelog

All notable changes to X0NE will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `install.sh` refused to install when a GitHub CLI older than 2.56 was
  logged in, and said "provenance verification FAILED": gh has
  `attestation` from 2.49 and `--signer-workflow` from 2.51, and up to 2.55
  it cannot read the current Sigstore trusted root (Debian 13 ships 2.46).
  Such a gh now counts as no gh: the install goes on with the SHA-256 check
  and says the provenance is not verified, and why. A gh that verifies and
  fails still stops the install. The verification also checks the signing
  workflow (`--signer-workflow .../release.yml`).
  `scripts/test-install-provenance.sh` checks these cases with a fake gh.

## [0.2.0] - 2026-11-05

**Preview.** Usable today; defaults and interfaces may change before 1.0.

### Added

- Plan mode. `/plan` in the REPL and the TUI, or `Shift-Tab` in the
  TUI, puts the session in plan mode; `/build` takes it out. The runtime
  refuses every action that touches the host, the orchestrator refuses the
  same actions before any prompt and refuses every sysaicall that is not a
  read, standing approvals and yolo are suspended, and the model is told in
  its system prompt to answer with a plan. Only the human switches: no
  action, tool or configuration key does. The header shows `PLAN` and the
  input `plan ❯`. `Shift-Tab` no longer cycles the side panels backwards.
  `/plan` is refused while a world is running: a world has its own runtime,
  which plan mode does not reach.
- Hook events `PostToolUse` (`[[hooks.post_tool]]`, after an action ran and
  succeeded), `SessionStart` (`[[hooks.session_start]]`, matched on
  `startup`, `resume` or `clear`) and `Stop` (`[[hooks.stop]]`, at the end of
  a turn that was neither cancelled nor a model failure). They only
  observe: their output is never read, nothing reaches the model, and a
  failure is logged without stopping the session. Every payload, PreToolUse
  included, now carries `session_id` and `cwd`, under Claude Code's names.
  `docs/hooks.md`.

- `make hooks` points the clone at `scripts/hooks`: a commit-msg check
  (`scripts/check-commits.sh`) and a pre-push guard that refuses a push
  carrying a maintainer working file, a foreign author or committer, or an
  entry of a private deny list. Contributions carry a Developer Certificate
  of Origin sign-off (`CONTRIBUTING.md`).

### Changed

- The README is a front page. Its sections moved to `docs/`: `install.md`,
  `tui.md`, `modes.md`, `policy.md`, `primitives.md`, `fleet.md`,
  `development.md`, and into `architecture.md`, `mcp.md` and `training.md`;
  `docs/README.md` indexes them.
- A command-line flag now beats the environment, on every surface: the
  config is the file (`-c`, else the search), then the environment, then
  the flags. `--provider` beats `X0NE_LLM_PROVIDER` and `--mode` beats
  `X0NE_BRIDGE_MODE` in the REPL, `-p`, `--acp`, `--serve`, `--tui` and
  `--matrix`, and in `x0ne-orchestrator --serve`. Before, the variable won
  in the REPL, `-p`, `--acp` and `--serve`, and `--matrix` let `--provider`
  win. This changes the plain REPL and `-p` too: a `--provider` or
  `--mode` that an exported variable used to override now applies.
  `X0NE_MODEL` still names the model of the provider that runs, now the
  one `--provider` chose. `--endpoint` beats `OLLAMA_BASE_URL` and
  `X0NE_OPENAI_COMPAT_BASE_URL`, and `--model` beats `X0NE_MODEL`, in the
  REPL, `-p`, `--acp`, `--serve`, `--tui` and `--matrix`, also when
  startup picks the provider itself; with no provider named, startup looks
  for Ollama at `--endpoint`. `OLLAMA_BASE_URL` is now read with the rest of
  the environment, so a config saved during a session (`/model`,
  `/provider`) records its endpoint, as it records
  `X0NE_OPENAI_COMPAT_BASE_URL`.
  `--serve` keeps `bridge.mode = "http"`, which `X0NE_BRIDGE_MODE` used to
  overwrite after the fact. The Anthropic key check runs on the resolved
  config: `--provider openai-compat --endpoint URL` with an `sk-ant-` key
  in `-c`, `X0NE_OPENAI_COMPAT_KEY` or `OPENAI_API_KEY` is refused before
  any request, and `ANTHROPIC_API_KEY` is never sent there.

- `x0ne-confidential` is pinned to a commit of the public x0ne-Engine
  repository (`Cargo.toml`, `Cargo.lock`); `scripts/repin-public.sh` moves
  the pin to another public commit and refuses one whose crate differs
  unless the change was reviewed.
- x0ne-browser: the default allowlist is loopback (`localhost`,
  `127.0.0.1`, `[::1]`) and `about:` only; it no longer carries a domain
  of the publisher. `data:` stays opt-in.

### Removed

- The learn mode (`x0ne learn`, `/learn`) and the context capture it used,
  `~/.x0ne/learn/last-context.json`.

### Security

- `SearchCode` and `SearchAndRead` apply the read policy to every file they
  walk, not only to their root: a file under `deny_path_patterns` is
  skipped, a denied directory is pruned, and the result counts both in
  `omitted_by_policy`. The walkers call the gate's own function. Before,
  a search could return lines of files that `ReadFile` refused.
  (Audit of 2026-11-02, B2.)
- The policy gate expands a leading `~` the way the executor does before
  it judges a path. It judged `<cwd>/~/…` while the executor opened
  `$HOME/…`, so a deny pattern written as an absolute path did not see a
  `~` path when the cwd was inside a read root. (Found while fixing B2 of
  the audit of 2026-11-02.)
- A `<dir>/**` deny pattern also denies the directory itself: `**/.ssh/**`
  matched every file under `~/.ssh` and not `~/.ssh`, which `FileInfo`,
  `StatFiles`, `MoveToTrash` and a search rooted there could name. The
  runtime gate, the search walkers, the read-only dispensation and the
  PreToolUse hook share one matcher, `x0ne_actions::deny_paths::DenyList`.
  (Audit of 2026-11-02, A2.)
- The default deny list covers the credential stores next to the agent:
  x0ne's journal key, HTTP tokens, model-key configs, trust store and
  snapshot copies; `.claude/.credentials.json` and `.codex/auth.json`;
  `gh`, `hub`, `.netrc` and `.git-credentials`; `.kube`, `.aws`, `.azure`,
  gcloud and Docker credentials; and the other credential files bwrap
  hides from commands. A test keeps the two lists together. The runtime
  read these under a home read root. (Audit of 2026-11-02, A1.)
- `x0ne` no longer reads `x0ne-runtime/x0ne-runtime-sandbox.toml` from the
  current directory when it provisions a sandbox container. That file sets
  the policy of the runtime inside the container, and a cloned repository
  could ship its own. The config now comes from `$XDG_CONFIG_HOME/x0ne/`
  (absolute only) or the system-wide copy.
- `install.sh` downloads over https only (`curl --proto '=https'
  --proto-redir '=https' --tlsv1.2`): it followed a redirect to http and
  accepted an http `X0NE_RELEASES_URL`, where the checksum travels with the
  archive. It now requires curl: GNU wget follows a redirect to http even
  with `--https-only`. `scripts/test-install-https-only.sh` checks both cases.
- Dependabot no longer proposes `x0ne-confidential`: it would have moved
  the Engine to the head of its default branch each week, merged on green
  CI. The Engine moves only through `scripts/repin-public.sh` (GOVERNANCE).
- The MCP documentation registered `x0ne-runtime --mode mcp` without
  `--config`, recommended `policy.mode = "auto"` for MCP clients and listed
  tools that do not exist; `docs/mcp.md` is rewritten from the measured
  `tools/list`. The runtime's MCP refusal offered `--upstream-human-gate` as a
  way out; it now says the flag is reserved for `x0ne-orchestrator`.
- RUSTSEC-2026-0424 (wasmtime 36.0.19, borrowed resources moved between
  components) is named in `deny.toml` and `SECURITY.md`, not fixed: the
  component model is a wasmtime feature this workspace does not enable, and
  wasmtime is behind the optional `asm-exec` feature. The fix is wasmtime
  36.0.20 on the same LTS line.
- x0ne-browser: every request the browser makes passes the allowlist, not
  only the tab's URL. `Fetch` on the browser session pauses each request,
  a popup's first navigation included; the allowed continue, the others
  fail with `BlockedByClient` and are journaled
  (`browser_request_blocked`). Every target Chromium opens beside the page
  (`window.open`, a `target=_blank` link) is closed as it appears and
  journaled (`browser_popup_closed`); `--disable-popup-blocking` is gone
  from the launch flags. Measured before the fix with two local servers:
  an allowed page reached the one outside the allowlist seven times, by an
  image, a frame, a script, `fetch`, a beacon and a `window.open` popup,
  and the popup took the foreground, so the page answered no click; with
  `Fetch` on the page session alone, the `target=_blank` link of a click
  still sent its navigation before the new target was attached.
  (Audit of 2026-10-30, H4-1.)
- The router's `Cross-Origin-Resource-Policy` is `same-origin`, no longer
  `same-site`. Measured on Chromium 156 (2026-10-31): `same-site` already
  kept a `localhost` page from embedding a `127.0.0.1` artifact and the
  reverse (two sites to the browser), but let a `localhost` page of any
  other port embed one; an artifact's files are its own, and the runtime's
  server on 7900 says the same.
- An artifact id is `cv_` and 128 random bits from the OS (a v4 UUID),
  no longer a hash of the clock's nanoseconds and the pid: the id is the
  artifact's URL, and anyone on the machine enumerated it from a process
  listing and a clock. (Audit of 2026-10-30, H3-1.)
- The runtime's artifact server on 7900 answers this machine only, like
  the router on 7800: `GET` and `HEAD`; a `Host` that names this machine
  with its port (a loopback spelling, or an IP literal, which is how the
  router reaches a sandboxed runtime); a loopback `Origin` when the browser
  sends one; no cross-site embedding. Every answer carries `nosniff`,
  `no-referrer`, `Cross-Origin-Resource-Policy: same-origin`,
  `frame-ancestors 'none'` and `no-store`; a service artifact gets `GET`
  and `HEAD` through header allowlists and no body; `/_api/artifacts`
  reports a count, not the ids. Its port is in every URL it printed, and
  it served `/a/<id>/` under `Host: evil.example` with 200 and relayed any
  method and body to a service. (Audit of 2026-10-30, H3-1.)
- After a suspected prompt injection, every program the model would start
  asks, in sandbox yolo too, unless it is a read: a `RunCommand` off the
  read-only table (the ordinary gate's own verdict), a terminal keystroke, a
  `Listen`, a check that runs a command, a composite holding one, a
  container run. The post-injection gate knew the words of network egress
  and of secrets and nothing else: a script written without them, then
  `python3 exfil.py`, ran with nobody asked. Reads (`ls`, `git status`,
  `ReadFile`) stay free. (Audit of 2026-10-30, H2-4.)
- `x0ne audit verify`: a `chain_header` inside a signed chain must name the
  key that signed it, else the verdict is a break (`inserted`) at that line:
  a sealed journal cut after its seal and continued by someone without the
  key read "OK, signed by <key>" with nothing said about the resume. A
  resume by the key holder is accepted and said in the notes, and the count
  of lines after the last signed head now survives it instead of restarting
  at the new header. (Audit of 2026-10-30, H4-6.)
- What a listened program prints (`Listen`: a log, a dev server) is scanned
  by the injection guard before it enters the history, like a file the agent
  reads: a batch that addresses the model is wrapped as untrusted, shown to
  the operator, journaled, and taints the session, so that network egress
  and secret reads ask for the rest of it. The same line read through
  `ReadFile` already did; a log line entered as it was. (Audit of
  2026-10-30, H2-6.)
- The artifact router on `127.0.0.1:7800` answers a browser on this
  machine only: `GET` and `HEAD`, a loopback `Host` with its port, a
  loopback `Origin` when the browser sends one, and no cross-site embedding
  (`Sec-Fetch-Site: cross-site` passes for a top-level navigation only).
  Anything else gets 403, another method 405, and nothing of it reaches the
  runtime. Request and response headers cross through allowlists, the
  request body is no longer read or relayed (it was, up to 16 MB, with any
  method), and every answer carries `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `Cross-Origin-Resource-Policy: same-site`,
  `Content-Security-Policy: frame-ancestors 'none'` and `Cache-Control:
  no-store`. `/healthz` reports a count, no longer the artifact ids (their
  URLs) and the upstream. A page whose name resolved to 127.0.0.1 read an
  artifact under its own `Host`. (Audit of 2026-10-30, H3-1.)
- A registered artifact served from the host filesystem followed symlinks
  out of its root: the path is now canonicalised and must stay under the
  canonical root, like the runtime's own server; a link out answers 403.
  (Audit of 2026-10-30, H3-5.)
- Under bubblewrap the `cwd` of a `RunCommand`, `Listen`, `SafeTest` or
  terminal is no longer a Landlock read root of its own: reads come from
  `[policy] allowed_read_paths` and `allowed_write_paths` only. A `cwd` the
  action names outside every root is refused before anything is spawned,
  and the refusal names it; with no `cwd` the command runs in the runtime's
  working directory, which grants no read either. `cat token` with `cwd`
  set to a directory under `$HOME` read what no root covered.
  `~/.x0ne-secrets` joins the hidden paths. (Audit of 2026-10-30, H1-1.)
- The MCP gateway puts its native tools with effects behind the human gate:
  `x0ne_host_add`, `x0ne_host_remove`, `x0ne_host_use` (the inventory and
  active pointer under `~/.x0ne`), `x0ne_artifact_expose` and
  `x0ne_artifact_unexpose` (a loopback URL on this machine) now send
  `elicitation/create` before their handler runs, and are refused through a
  client that cannot ask. They ran at once, while the agent loop asks for
  the same actions. A host `target` that `ssh` would read as an option
  (leading `-`, whitespace, control characters) and any `ssh_options` key
  outside the `SshExec` allowlist are refused, on `x0ne_host_add` and when
  the inventory is loaded. (Audit of 2026-10-30, H3-2.)
- A `sh -c` line whose command carries an inline assignment on a variable
  that chooses the program (`PATH=/tmp/x cat secret`, `LD_PRELOAD=…`,
  `BASH_ENV=…`, `PYTHONPATH=…`: the `env_keys` list), and the same prefix
  given to `env`, are refused as not analyzable. The gate skipped the
  assignment and approved `cat` while the shell ran `/tmp/x/cat` under that
  name. Ordinary variables (`FOO=1 cat f`) still split as before. (Audit of
  2026-10-30, H1-2.)
- `/until <cmd>` runs its command as a `RunCommand` of the session: through
  the human gate, the runtime's policy and the runtime's isolation, before
  the first turn and after each one. It ran with `sh -c` in the `x0ne`
  process, on the host, with the operator's environment, outside every
  gate, and the code it ran is the workspace's, which the model writes. A
  refusal, no runtime or no exit code drops the goal; nothing is taken as
  passed on a guess. (Audit of 2026-10-30, H4-2.)
- A child agent (`SpawnAgent`) spends what its parent has left of the
  session cap and is billed to the parent when it returns; its own
  `Budget set` cannot pass that remainder, and a parent with nothing left
  spawns nothing. Each child used to start at $0 of a fresh
  `cost.budget_usd_limit`, unrecorded: a $5 session could spend $5 more per
  child. (H2-1.)
- Sandbox yolo no longer waives `Budget` and `ProjectNote`: neither acts
  inside the container. They ask, and are refused with nobody to ask. The
  model may lower the live cap under `cost.budget_usd_limit`, never raise or
  remove it; `/budget` stays the operator's. (H2-2.)
- `x0ne-browser-mcp` refuses to start on a `~/.x0ne/browser-tools.toml` it
  cannot read or parse. It fell back on the defaults, which have no
  allowlist: a typo in the operator's restriction lifted it without a word.
  (H4-5.)
- The browser journal no longer records what `browser_type` typed into the
  page: the key is `text`, which the key-name redactor let through, so a
  password typed into a login form went to the journal in clear. The journal
  is written 0600 in a 0700 directory, not with the umask. (H4-3.)
- Every `x0ne-vm-mcp` tool checks the agent's VM and snapshot names before
  virsh sees them, and a name may not start with `-`. The names went to virsh
  as bare words: `snapshot=--memspec=file=<path>` wrote the guest's memory to
  a host path of the agent's choosing. (H1-5.)
- `/history` and `/expand` fold terminal control characters out of the
  transcript before printing it. They printed it raw: an escape sequence in
  the model's text or in a tool's stdout ran in the terminal (clipboard
  write, scrollback wipe) as the operator reread the session before an
  approval. (H3-3.)
- A directory listed under `[project_trust] privileged` can no longer set
  `[hooks]` from its `x0ne-orchestrator.toml`: hooks are commands x0ne runs,
  and they come from the user configuration only.
- The runtime holds plan mode. A `{"control":"SessionMode","mode":"plan"}`
  line, accepted over stdio only, makes `x0ne-runtime` refuse every action
  whose class touches the host (`DenyReason::PlanMode`, not liftable), above
  the per-action switches and the session grants, until
  `{"control":"SessionMode","mode":"build"}`. The stdio bridge replays plan
  mode on a respawned runtime before it sends anything, and sends nothing if
  the fresh runtime refuses it.
- A model could widen the policy without a human. The orchestrator forwards
  a model's action as written, and the runtime read any stdio line with a
  `control` key as a control message, so `{"type":"ReadFile", …,
  "control":"PolicyGrant","kind":"command","value":"touch"}` added `touch` to
  `allowed_commands` for the session (measured 2026-10-27, from model text
  through the parser, dispatch and the real runtime). A control line is now
  a `control` key with no `type`; a line carrying both is refused whole and
  nothing runs.
- A project file that is a symbolic link out of its folder is no longer
  read. Measured 2026-10-27: with `AGENTS.md ->
  ../outside/secret.md`, the trust question listed `AGENTS.md` only, and
  once the folder was trusted `-p` sent the target's text to the model in
  the system prompt; a `SKILL.md` that linked out, or sat in a skill
  directory that linked out, put its description in the skill catalogue.
  The instructions, the rules, the skills and `x0ne-orchestrator.toml` now
  follow the rule `bridge.runtime_config` already had: symlinks are
  followed, but the real path must lie inside the real folder. A file that
  leaves it is shown on the trust question with its target, as refused,
  and is read neither to fingerprint the folder nor to build the prompt;
  the file opened is the file checked. A link that stays inside the folder
  is read as before. An instruction file that links to a parent folder's
  file is now refused too.
- A project file is now read only if it is a regular file inside its
  folder, with one hard link and at most 1 MiB, and is not a `.env*` file
  nor under a `.git` directory. Measured 2026-10-27: with `X0NE.md -> .env`
  and `.x0ne/rules/git.md -> ../../.git/config`, links that stay inside the
  folder, the trust question showed `X0NE.md` and `.x0ne/rules/git.md`
  alone, and once the folder was trusted `-p` sent the content of `.env`
  and of `.git/config` to the model; a hard link to a file outside the folder was read, and a
  file over 1 MiB was read whole, before the question. Such a file is now
  shown refused on the question, with its target and the reason, and is
  never read; the question also shows where every link leads. The file is
  opened one directory at a time from the folder, none of them a link, on
  every Unix: a directory swapped for a link while the file is opened now
  fails the read. Measured 2026-10-27 with `/proc` hidden: the old check,
  which read `/proc/self/fd`, let reads through such a swap to the file
  outside, 3 and 9 in two runs of 3 seconds.
- The prompt and the configuration of a trusted folder now get what was
  trusted. Measured 2026-10-27: a rule rewritten while the trust question
  waited for its answer reached the model in its new text. Instruction
  files, rules, skills and `x0ne-orchestrator.toml` are read again when
  used and left out, with a warning, unless their bytes and their target
  match the fingerprint the decision was taken on. The runtime policy a
  privileged folder names is read by the runtime itself and is not checked
  again.
- The project name and the project tree no longer follow a link out of the
  folder, trusted or not. Measured 2026-10-27: `Cargo.toml ->
  ../outside/gitconfig` put the `name` of a git config's `[user]` section
  in the system prompt, and a directory linked out of the folder put the
  names of its files in the project tree, whether the folder was trusted
  or not. `Cargo.toml`,
  `package.json` and `pyproject.toml` are now read as project files are:
  one that is refused or not a regular file counts as absent, and a FIFO no
  longer hangs detection. The tree lists a link to a directory without
  walking it. For a privileged folder, `x0ne-orchestrator.toml` is no
  longer read through a link out of the folder to find its runtime policy,
  and a FIFO in its place no longer hangs the trust check.
- The trust question shows every refused file and every link with its
  target, refusals first, and folds only the regular files past 8 lines
  into one "and N other files of this folder, none a link" line. Each line
  is cut to 120 characters, the name and the target apart, so a link keeps
  its arrow. Before, every file was listed on one line, without bound; then
  the fold to 8 lines hid links. Measured 2026-10-28: with eight plain
  rules and a ninth linked to the folder's `.npmrc`, the link was folded
  away and a yes sent the token to the model.
- The trust check reads at most 256 rules and skills and 16 MiB of project
  files in all. Past either bound it fails before the question, and the
  folder is not trusted, as with an unreadable store. Measured 2026-10-28:
  4096 rules linked to one 1 MiB file inside the folder made the check
  hash 4 GiB before the question.
- A manifest name reaches the prompt only if it is a package name: 1 to 214
  characters of `[A-Za-z0-9@/._-]`; otherwise the folder's name is used,
  its controls folded and cut at 214 characters. Measured 2026-10-28: a
  `package.json` name with newlines and 200 kB wrote its own lines into the
  system prompt of a folder that asked nothing.
- A file name stays on its line in the project tree and in the boot
  listing, trusted or not: controls, Unicode line separators and bidi
  characters are folded to spaces, and the listing runs `ls -1Apq`.
  Measured 2026-10-28: a file name holding newlines wrote lines of its own
  into the system prompt, and `ls -1Ap` listed it as several names.
- `setup.py` and `.git` are detected by their own entry, not by their
  target. Measured 2026-10-28: with `setup.py -> ../outside/probe`, the
  prompt said `python project` exactly when the path outside existed.
- `@file` and `/read` read a path under the working directory as a project
  file is read: a link out of the folder, a `.env*` file, a path under
  `.git`, a file with more than one hard link or over 1 MiB is refused, the
  model is told so in place of the reference, and the terminal shows why
  with the target; a link inside is named with its target. A path written
  outside the working directory is read as before. Measured 2026-10-28:
  `@README.md`, a link to a file outside, sent that file to the model in a
  folder that asked nothing.
- A key read from the environment without the config naming it now goes
  only to the vendor that issued it. Measured 2026-10-25 on the pre-release build: with
  `OPENAI_API_KEY` set and no key in `[llm.openai_compat]`, `-p`, the REPL,
  `--tui` and `--matrix` sent it as a Bearer to the host of `base_url`, and
  `OLLAMA_API_KEY` went to any Ollama `endpoint`. One table
  (`x0ne-actions/src/model_keys.rs`) now says where such a key may go:
  `ANTHROPIC_API_KEY` to api.anthropic.com, `OPENAI_API_KEY` to
  api.openai.com, `OLLAMA_API_KEY` to ollama.com, over https. Anywhere else
  the session is refused before any request, the startup probe included,
  and the refusal names the line to add. To keep sending the key to your
  server, name it in the provider's section:

  ```toml
  [llm.openai_compat]               # or [llm.ollama]
  api_key_env = "OPENAI_API_KEY"    # or "OLLAMA_API_KEY"
  ```

  For a server that takes no key: `api_key_env = ""`. `--acp` and
  `--serve` now resolve keys the same way, so they too take
  `OPENAI_API_KEY` for an openai-compat endpoint at api.openai.com, and
  refuse it elsewhere. A key the config names (`api_key`, `api_key_env`,
  `X0NE_OPENAI_COMPAT_KEY`) goes to the config's host, and the REPL, `-p`,
  `--tui`, `--matrix`, `--acp` and `--serve` say so once at startup:
  `key from OPENAI_API_KEY → 127.0.0.1:4000`, never the key.
  `[llm.openai_compat] api_key_env` is now read; it was ignored. A key
  that holds `sk-ant-` anywhere still goes through provider anthropic only,
  named or not: on the pre-release build, `Bearer sk-ant-…` or a leading U+FEFF or U+200B
  reached an openai-compat host. api.anthropic.com and api.openai.com take
  no key of another vendor's format, whatever the provider. The `auth` of a
  runtime action reads the same table (below). `X0NE_OPENAI_COMPAT_KEY`
  is no longer copied into the config, so a config saved during a session
  no longer writes it to disk. A config saved by an earlier version names
  the default variables (`api_key_env = "OLLAMA_API_KEY"`, ...): its keys
  count as named.

- The `auth` of an `LlmCall`, `OllamaChat` or `OllamaApi` action now reads
  the same table, which moved to `x0ne-actions/src/model_keys.rs` so that
  the runtime and the orchestrator share it. Measured 2026-10-25 on the pre-release build:
  with `LlmCall = true`, an action the model wrote,
  `{"auth": "$ENV:ANTHROPIC_API_KEY", "endpoint": "http://127.0.0.1:…"}`,
  sent the key as a Bearer to that endpoint, and no one was asked: a model
  call was waived from the confirmation gate. `$ENV:ANTHROPIC_API_KEY`,
  `$ENV:OPENAI_API_KEY` and `$ENV:OLLAMA_API_KEY` now go to their vendor's
  hosts over https only, a key that holds `sk-ant-` to api.anthropic.com
  only, and anywhere else the action fails with `auth refused` before any
  request. A model call that carries an `auth` meets the confirmation gate.
  The runtime the session spawns in stdio mode no longer inherits
  `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OLLAMA_API_KEY` or
  `X0NE_OPENAI_COMPAT_KEY`; a key meant for an action goes in a variable of
  your own (`$ENV:MY_LLM_KEY`). A sandbox runtime still receives
  `X0NE_FORWARD_ENV`.

- A loopback model endpoint went through `HTTP_PROXY` when one was set: on
  the pre-release build the proxy received the request meant for a model on 127.0.0.1,
  key included. The Ollama and openai-compat clients, the startup probes and
  the runtime's model actions now skip the proxy for `localhost`,
  127.0.0.0/8 and `::1`. The Anthropic client follows no redirect: reqwest
  keeps `x-api-key` on a redirect to another host. A compat profile added
  without a name is named after the host of `base_url`, never its userinfo
  (`https://user:pass@host:1` was named `user`). In `-p`, a refused key is the
  error on stderr; it was printed on stdout, followed by
  `no usable model. Set ANTHROPIC_API_KEY or OPENAI_API_KEY`.

- A password in a provider URL (`base_url = "http://user:pass@host"`, an
  Ollama `endpoint`, `--endpoint`) was printed as written: in the `--tui`
  and `--matrix` header, the model timeout line, the matrix error screen,
  and errors such as `Streaming request to … failed` (measured 2026-10-25).
  Every line that names a model endpoint, debug logs included, now shows
  its userinfo as `***@`, whatever spelling the
  URL parser of reqwest accepts (a leading space, `http:/`, `http:\\`); a
  text it cannot parse, a password holding `/`, `#` or `?`, shows nothing
  before its last `@`.

- The setup wizard wrote `allowed_read_paths = ["."]` and
  `allowed_write_paths = [".", "/tmp"]`, which the runtime resolves in the
  directory it is started from: started from `/`, the whole tree (measured
  2026-10-16). The roots are now absolute, resolved when setup runs. `/`,
  `~` or a parent of `~` as a root needs a typed yes; without it that root
  is dropped, and with none left setup writes nothing and says why. The
  Developer profile no longer lists `env`, `curl`, `wget`, `docker`,
  `podman`, `rm` or `chmod`; each can still be added by hand or granted at
  the prompt that offers it.

- On the host, an approved `rm -rf` ran with no undo point and `/undo` then
  answered "no undo point" (measured 2026-10-16, 3 runs). A command that may
  delete files now snapshots the workspace before the confirmation prompt, as
  in the sandbox. When no snapshot can hold what it deletes (a target outside
  the workspace, a variable, a `cd`, a secret under `deny_path_patterns` at
  the top, more than `[snapshots] max_snapshot_size_mb` to copy), the prompt
  starts with "NO UNDO POSSIBLE", offers no standing approval, and no trust
  root, earlier "always" or missing answer lets it run. The Developer and Full
  profiles now enable the four snapshot actions and state `[snapshots]`.
  Widening `allowed_commands` to a shell or a launcher now says that a granted
  shell is a granted everything.

### Fixed

- `/undo` typed in the TUI went to the model as a prompt and restored
  nothing (measured 2026-11-02 against a scripted server: a 4th request
  whose last user message was `/undo`); only the REPL answered it. The
  TUI now restores the agent's last write through the session's undo
  journal, the same path as the REPL, sends nothing to the model, and
  `Tab` completes `/undo`. While a turn runs it is refused, not deferred.
- The stdio bridge respawns a runtime that exited even when the write to
  its stdin succeeds because another process still holds the pipe (a
  program the runtime started). Before, the answer was "x0ne-runtime
  returned an empty response line" and no respawn (measured in CI,
  2026-10-31).
- Terminals on the native PTY backend (no tmux): `TermWait` with `exit`,
  `TermRead` and `TermList` could report a program as exited with a screen
  that missed the last output it wrote (measured 2026-10-27: 2 failures in
  27 runs of the `[y/N]` test under load, `answer=y` absent). The exit
  status is now published once the terminal is read to end of file and
  that output is on the screen. A program that leaves a descendant holding
  the terminal open (`setsid`, `SIGHUP` ignored) is reported exited 2 s
  after its exit.

- `--acp` and `--serve` dropped `--endpoint` and `--model`: the prompt
  went to the endpoint of `-c`, with its model and key (measured
  2026-10-25). `OLLAMA_BASE_URL` beat `--endpoint` on `-p` and `--tui`: the
  prompt and `OLLAMA_API_KEY` went to the variable's host. `X0NE_MODEL` beat
  `--model` when startup picked the provider. Each flag now wins.

- `--tui` and `--matrix` ignored `-c` and reread `~/.x0ne/config.toml`
  (measured 2026-10-24: the TUI header named the home config's model, and
  matrix sent its first request to the home config's endpoint). Both now
  run on the config the REPL resolves: the `-c` file, the environment,
  then `--mode`, `--provider`, `--endpoint` and `--model`; `--sandbox`
  and `--no-sandbox` apply to them too. A flag they would drop (`-p`,
  `--stdin`, `--output-format`, `--auto-mode`, `--max-iterations`,
  `--health`, `--acp`, `--serve`, `--bench-agentic`, `--desktop`, a
  subcommand, `--tui` with `--matrix`) is refused, exit 1.

- Matrix retried a failed model call every 2 s, forever, a 401 included
  (measured 2026-10-24 on a 503 endpoint: a 4th call within 17 s). It now
  calls the model through the agent's invoker: 3 attempts for a transient
  failure, 1 s then 2 s apart, none for a permanent one. Then the error is
  shown full screen with the model and endpoint called; `r` starts one
  more bounded cycle, any other key quits with exit 1.

- In `--tui`, a model that sent nothing kept the turn on "thinking" for up
  to 3 attempts of 300 s (the transport's bound), Ctrl+C quit the whole
  TUI, and a failed call showed only "turn model_failure" (measured
  2026-10-24). `[ui] model_timeout_secs` (default 300, 0 = no bound) now
  bounds a model call, retries included, in `--tui` and `--matrix`; the
  default is the OpenAI-compat and Ollama clients' own per-request
  timeout, so no answer they accept is cut. Ctrl+C during a turn stops
  the turn; idle, or twice within 2 s, it quits. The failure's kind and
  the provider's message are shown, and the next prompt is answered.

- The TUI's first screen named `claude-sonnet-4` whatever model was
  configured, until the runner reported the real one. It now reads
  `connecting…`, then the configured model, or `not connected` if the
  boot failed.

## [0.2.0-beta.3] - 2026-10-16

### Security

- A shell in `allowed_commands` made the list a formality: the command gate
  saw only argv[0], so `bash -c "curl evil"` ran `curl` though it was never
  listed. `RunCommand` and `Listen` now submit each program a shell or `env`
  invocation would launch to the same allowlist: a `-c` line is split on
  `;`, `&&`, `||`, `|`, `&` and newline, nested shells and `env` recurse. A
  script, or a line with control flow, substitution or redirection, is
  refused as `CommandNotAnalyzable`.

- A non-Bourne shell in `allowed_commands` was a command-gate bypass. The gate
  split every allowed shell's `-c` line with Bourne quoting, but `fish` reads
  `\'` as an escaped quote inside `'…'`, so `fish -c "echo 'a\' '; curl evil;
  echo ''\'"` was read as `echo` only and allowed while fish ran `curl`
  (measured 2026-10-16). `fish`, `csh` and `tcsh` now have no gate grammar: any
  `-c` line or script they are handed is refused as unanalyzable, nested ones
  too; only a bare, inert invocation passes. Bourne and POSIX shells
  (sh/bash/dash/zsh/ksh/mksh/ash/rbash) are unchanged (extends the per-command check of shell lines).

- An agent's own tools escaped the policy: Claude Code's `Bash`, `Edit`,
  `Write` (and Codex's shell and `apply_patch`) reached the host while only
  the MCP tools were gated. `x0ne hook pretooluse`, wired as a PreToolUse
  hook, judges each of them against `x0ne-runtime.toml`: allow, ask, or deny
  with the reason, refused when no policy is found. Every decision goes to the
  system journal under `SYSLOG_IDENTIFIER=x0ne-harness`. Wiring, guarantees
  and limits: `docs/agent-hooks.md`.

- The Codex hook was fail-open on a missing binary. Codex has no
  `onFailure: block`, so wiring it straight at `x0ne hook pretooluse` let the
  tool call through whenever the binary was absent, crashed or hung (measured
  2026-10-16: absent binary exits 127, emits no decision). Codex is now wired
  through `scripts/hooks/x0ne-codex-pretooluse.sh`, which denies in Codex's
  format in all three cases and forwards x0ne's own verdict otherwise. Cases:
  `scripts/hooks/test-x0ne-codex-pretooluse.sh` (run by `make loop`).

- A scripted function's `print` and `debug` wrote to the runtime's stdout,
  which in stdio mode is the bridge: the orchestrator took the script's line
  for the answer to the current action, and a printed JSON line passed for an
  answer of the script's choosing. Both go to stderr now, prefixed
  `x0ne-runtime: rhai print:` or `rhai debug:`, one escaped line per message,
  16 KiB per call.
- Scripted functions had no size cap, and `import` loaded any `.rhai` file on
  disk, code DefineFunction never saw. One value now holds at most 1 MiB of
  strings, 16,384 array items (or BLOB bytes) and 4,096 map entries, a call at
  most 256 variables and 2 s; past a cap the call fails alone. DefineFunction
  refuses `import`, and no module resolves. Map keys are not counted: 2,048
  keys of 512 KiB passed the caps (1 GiB, debug build).

- Under `[desktop] allowed_apps`, agent commands could drive the session
  around the list (`ydotool`, `wlrctl`, `xdotool`, `swaymsg`, through any
  interpreter). They now run only under bubblewrap, which hides the Wayland,
  X11, sway/i3 IPC and ydotool sockets named by the runtime's environment on
  top of `/run/user/<uid>`; `--isolation none` or `container` refuse them while
  the list is set, as do a session socket inside a write root and a shared
  network without Landlock ABI 6.
- Under `allowed_apps`, `Screenshot` returned every window. The image is now
  black except the listed windows, identical in the tree read before and after
  the capture; unlisted windows are blacked out over them. Refused, with no
  file left, when the tree is unreadable. `GetWindowTree` and
  `GetFocusedWindow` blank the titles of unlisted windows.
- `x0ne setup` wrote `deny_path_patterns = []` in the developer, read-only
  and full profiles, so `ReadFile` served a project's `.env`, `*.pem` and
  `secrets/`, and `~/.ssh` under a Home root. The default deny patterns
  now hold at load whatever the list says; one is lifted only by naming it in
  the new `[policy] deny_path_exceptions` (empty by default, announced at
  startup; a name that is not a default refuses the file). The wizard writes
  the defaults out. A policy that relied on an empty list now gets the
  defaults, sandbox files written before this change included; a new sandbox
  policy and the shipped `x0ne-runtime-sandbox.toml` lift each one by name.

- `ListProcesses` returned each process's command line as read from
  `/proc/<pid>/cmdline`, in every profile that enables it, the MCP readonly
  one included: a password, token or URL credential passed as an argument to
  another program reached the model. Each argument now goes through the
  journal's masker (`x0ne-audit`): the value of `--name=value` or
  `--name value` when the name says secret, URL userinfo, known token shapes,
  the password of `--user name:pass`, `curl -u`, `mysql -pX`, `sshpass -p` and
  `redis-cli -a`, also inside a `sh -c` script. The other arguments are kept.
  The profile's "no PII" comment is corrected: command lines can still name
  paths, hosts and users.
- Dependencies with published advisories: rustls 0.23.45 -> 0.23.46
  (RUSTSEC-2026-0346, -0347, -0348, -0349) and wasmtime 36.0.13 ->
  36.0.18 on the same LTS line (RUSTSEC-2026-0269, -0316, -0363).
  `x0ne-confidential` builds against the Engine 0.2.0-beta.3.

### Fixed

- `ArtifactStop`, `ArtifactDelete` and a re-serve ran the `kill` program
  found in `PATH` and ignored its result: where no `kill` is installed
  (procps absent, as in the rust:1.98.1 image) the service kept running
  while the action reported it stopped. The service group is now signalled
  with kill(2); pid 0, pid 1 and the runtime's own pid are refused, since
  `kill -9 -1` would signal every process of the user.
- `agent_commands_are_confined`: under a `HOME` in `/tmp`, which the sandbox
  replaces with a tmpfs, the test now says why the unlisted-token attack
  cannot run instead of failing on ENOENT.
- CI: the rustdoc link of `Launched::Opaque` and four clippy pedantic
  findings.
- `x0ne -p` without a policy file printed "the runtime crashed at startup"
  around the runtime's refusal; it now prints the refusal alone. The startup
  diagnosis reads only what the current runtime wrote to `core.log`, not a
  refusal left by an earlier run.
- `X0NE_REQUIRE_SANDBOX`: plain `x0ne`, `-p`, `--mode stdio`,
  `--no-sandbox`, `--acp`, `--serve` and the `x0ne-orchestrator --mcp`
  downstream still started or reached a runtime outside any verified container
  under it. It was read only when autowrap (`--sandbox`, mode `auto`) found no
  ready container, and only as `1`. Those entries and `--isolation`, the TUI,
  matrix, `/target` and worlds now refuse before their runtime starts, with
  the cause and the ways out on stderr. Two do not read it: `x0ne cc`
  (feature `cloud`, off by default), whose runtime runs on the host by design
  to reach gcloud and says so on screen, and the `x0ne-vm-mcp` downstream
  that `X0NE_VM_DOWNSTREAM=1` adds to `x0ne-orchestrator --mcp`. A `sandbox_cmd` or `--downstream-prefix`
  counts only when it runs `incus|podman|docker exec`; x0ne in a declared,
  confirmed sandbox (`scripts/run-sandboxed.sh`) counts. Any value but empty or
  `0` turns it on.

## [0.2.0-beta.2] - 2026-10-14

### Changed

- **Breaking.** `x0ne-runtime` refuses to start without a policy file, instead
  of running on the compiled-in defaults. The message names the file to write
  (`$XDG_CONFIG_HOME/x0ne/x0ne-runtime.toml`) and how (`x0ne setup`, the
  reference profile, or `--config`). Refused the same way: an empty or
  comment-only file, an unreadable or invalid one, the cwd opt-in with no file,
  and a dangling link (it no longer falls through to the legacy location).

### Security

- `[desktop] allowed_apps` was never enforced: not by `FocusWindow`, which
  the docs said it limited, nor by any other gesture. Every input gesture now
  checks it against the live window tree: `Click`, `DoubleClick`, `MoveMouse`
  (windows under the point, before and after the warp), `Scroll`,
  `TypeText` (re-checked per 32-character chunk), `PressKey`,
  `PressKeyCombination`, `SetClipboard` (focused window), `FocusWindow`
  (resolved to a listed window id, focus confirmed). Identity is the
  `app_id` or X11 class, never the title; a window without one is refused.
  While the list is set, `GetClipboard`, clicks without coordinates and
  compositor keys (`super`, `XF86*`…) are refused. An empty list changes
  nothing.
- Snapshot restore refuses a manifest `snapshot_file` that leaves the
  snapshot directory (absolute path, `..`, separator): it could copy a file
  from outside into the write roots. Backups are opened `O_NOFOLLOW` and
  copied from the checked descriptor.
- In a sandbox the operator's policy still decides: yolo no longer grants a
  refused command, write or switched-off action without a human; `TermOpen`
  faces `allowed_commands` in a container; an existing policy file is never
  replaced by the sandbox profile.

## [0.2.0-beta.1] - 2026-10-13

### Added

- First numbered version, a beta. Built as the `x0ne-harness` package of
  x0ne OS 0.2 (`x0ne-orchestrator`, `x0ne-runtime`, `x0ne`), with the
  `x0ne-engine` provider and the system configuration layer below.

### Changed

- `x0ne -p` exits `2` when the turn did not complete (denied, budget, loop,
  model failure), with the reason on stderr; `0` only for a completed turn,
  `1` stays for startup errors. JSON envelope unchanged.
- A partial `[policy]` table in `x0ne-runtime.toml` now loads; omitted keys
  take the fail-closed defaults (`deny`, empty allowlists, shared deny
  patterns) instead of a "missing field" error. A table that names `allowed_*`
  entries without `default_effect` is still refused, by name.

### Fixed

- Headless `-p` with `[ui] auto_confirm = true`: the flag is not an approval
  and never was; `x0ne` now says so at startup and, on a denial, names what to
  set (`[trust] roots` in the user config). README documents trust roots.
- `./x0ne-runtime.toml` present but not read: `x0ne` says it on stderr at
  startup with the two ways to have it read; `docs/mcp.md` had the search
  order wrong since the opt-in was introduced.
- `/undo`: a snapshot refused by the runtime's policy (`CreateSnapshot`
  disabled) is no longer retried before every WriteFile; failures are counted.
  Logged at DEBUG (a WARN printed raw over the TUI).

### Added — learn mode (cargo feature `learn`, off by default)

Views that show what an LLM agent does and where its risks are:
`x0ne learn context`, `trace`, `tokens`, `next`, `attention`, `model`, and
`/learn`, `/learn trace` in the TUI and the REPL. See README « Learn mode ».

- The driver writes `~/.x0ne/learn/last-context.json` (0600) before each model
  call: sizes, sources, trust and masked previews, never the full text.
- Engine views relay x0ne-Engine's `learn` routes (contract v1) from the
  configured `openai-compat` server or `--engine URL` (no API key sent there),
  no redirects, response capped at 2 MiB; with a hosted provider nothing is
  sent. Text that looks like a credential is refused before sending.
- Everything printed or stored is masked (keys, tokens, `NAME=value` secrets,
  URL credentials, private keys) then control-folded.
- Without the feature: no subcommand, no slash command, no file written;
  release binary compared in `docs/learn-manual-test.md` step 1.

### Fixed — against a real x0ne-Engine lab (OLMo-2 1B, CPU, 2026-09-27)

- `openai-compat` streaming: a server that answers `stream: true` with one
  JSON completion (x0ne-infer-lab does) was read as SSE, so every turn got an
  empty reply. A JSON answer is now read as a completion.
- `learn tokens`: a lab refusal (`413`, over 64 tokens) was shown as the
  bytes view with "no tokenizer reachable"; the refusal is now the error.
- `learn model`: the lab's `model.digest` (weights, config, tokenizer) was
  labelled "Weights digest"; it is now "Model digest", and the weight file's
  sha256 (`openness.weights_sha256`) is shown as `weights`.
- `-p` journaled the answer and the turn but not the prompt: `x0ne sessions`
  counted 0 turns and `learn trace` found none. The prompt is now journaled
  as `user_input` before the turn, as in the REPL.

### SECURITY — the MCP service door is closed

An MCP client wired to `x0ne-runtime` could run arbitrary host commands with no
human in the loop. Every link came from a git-tracked file, so a fresh clone was
enough: `.mcp.json` and `make claude-add` registered
`x0ne-runtime --mode mcp --log-level error` **without `--config`**, config
resolution fell through to the write-enabled CLI reference profile that
`make install` deposits (`policy.mode = "auto"`, `default_effect = "allow"`,
`allowed_commands = []`, `RunCommand = true`), and `mode = "auto"` never
produces a confirmation request — so the fail-closed refusal in
`mcp/server.rs` was unreachable. Reproduced on the real binary:
`run_command {"command": "id"}` returned the host uid and groups with
`isError: false`. The hardened `x0ne-runtime-mcp-readonly.toml` had shipped all
along and nothing pointed at it.

Fixed in three independent layers:

1. **Entry points name their policy.** `.mcp.json` and `make claude-add` pass
   `--config ~/.config/x0ne/x0ne-runtime-mcp.toml`, which `make install` now
   deposits from `x0ne-runtime-mcp-readonly.toml` (never overwriting yours).
2. **`--mode mcp` refuses to start under `policy.mode = "auto"`.** A runtime
   with no reachable human must be `confirm` or `readonly`. The launcher may
   pass `--upstream-human-gate` to assert it front-ends the runtime and owns
   the gate — which is what `x0ne-orchestrator --mcp` does, so no double-gating
   is introduced.
3. **The working directory no longer supplies policy.** `./x0ne-runtime.toml`
   was the second rung of the search, ahead of the XDG install; an agent able
   to write one file where its runtime starts could author that runtime's
   policy. It is now read only under `X0NE_RUNTIME_ALLOW_CWD_CONFIG=1`.

The runtime also prints the absolute path of the policy it loaded to stderr at
startup, outside `tracing` — `.mcp.json` ships `--log-level error`, which had
been filtering the only line that could have revealed this.

`[policy] deny_path_patterns` is no longer empty in the shipped reference
config: it protected neither `~/.ssh` nor `.env` on a fresh install while
`SECURITY.md` claimed otherwise.

### BREAKING — `allowed_commands`: an empty list now means DENY ALL

`[policy] allowed_commands = []` meant "no restriction configured" and allowed
every command, while its two neighbours in the same table had already been
flipped to "empty = deny all". The shipped reference config carried exactly that
empty list next to `RunCommand = true`.

An empty list now denies every command. **There is no wildcard** — `["*"]` is a
command named `*`, not a blanket allow. To permit a command, name it:

```toml
[policy]
allowed_commands = ["git", "cargo", "ls"]
```

Denials surface as the existing `CommandNotAllowed` / `policy_denied` error.

### BREAKING — filesystem policy: an empty allowlist now means DENY ALL

`[policy] allowed_read_paths` and `allowed_write_paths` previously treated an
**empty list as "allow any path"**. That inverted the fail-closed posture the
rest of the runtime is built on: the shipped default (both lists empty) could
read any file on the host, including `~/.ssh/id_rsa`, `~/.aws/credentials` and
`/proc/self/environ`. An empty list now denies every path for that access kind.

**Who is affected.** Anyone relying on the old implicit "allow everything",
including the default config with no `x0ne-runtime.toml`.

**Migration.** State the directories the agent may touch:

```toml
[policy]
allowed_read_paths  = ["~/code"]
allowed_write_paths = ["~/code", "/tmp"]
```

To deliberately keep unrestricted access — only sane when something else is the
isolation boundary, e.g. inside a container — say so explicitly:

```toml
allowed_read_paths  = ["/"]
allowed_write_paths = ["/"]
```

The shipped profiles were updated in this change: `x0ne-runtime-sandbox.toml`
now declares `["/"]` (the container is the boundary), `x0ne-runtime.toml` ships
concrete roots instead of commented-out examples, and
`x0ne-runtime-mcp-readonly.toml` keeps `allowed_write_paths = []`, which now
literally means "no writes" — its intent all along.

Denials surface as the existing structured `policy_denied` error, so agent
feedback loops are unchanged.

### Security — filesystem confinement is now intrinsic and TOCTOU-free

Confinement used to be advisory: the policy engine canonicalized a path and
checked *that*, then the executor re-opened the caller's **raw string** with an
independent syscall. Swapping the final component for a symlink in between
redirected the I/O anywhere on the host.

Paths are now resolved and validated **once**, and the very same object is
opened. `ConfinedPath` is the only key to the I/O layer, so the boundary is
intrinsic to the executor rather than something each call site must remember to
request. Containment is component-wise (`/srv/rootEVIL` is not inside
`/srv/root`), writes refuse symlink targets including dangling ones, opens use
`O_NOFOLLOW`, and the resulting descriptor is re-verified through
`/proc/self/fd`. A configured root that does not exist is dropped so a later
`mkdir` cannot silently widen the sandbox.


### Features

#### Phase 2 — subprocess stability (Claude Code / Codex no-longer hangs forever)

The flaky empty jsonl sessions from earlier were the LLM subprocess (`claude -p` or `codex exec`) hanging silently on auth / network / zero-output failure paths — X0NE would block forever on `wait_with_output()`. Phase 2 fixes the entire failure mode:

- **Wall-clock timeout** on every `run_cli` call via `tokio::time::timeout`. Default 180s (3 min — Opus on big prompts), overridable via `X0NE_SUBPROCESS_TIMEOUT_SECS`. On timeout the child is killed (`kill_on_drop(true)`) and a clear error surfaces: *"X did not return within Ns — the subprocess is stuck (auth issue, network stall, or zero-output bug). Try: `echo hi | X -p` in a separate shell."*
- **Zero-output failures fail loudly**. Previous behaviour: silent `Ok(("", "", 1))` → the caller saw empty content and the TUI looked frozen. Now: `Err("X exited with code N and no stdout. stderr: …")` — the user sees the auth/network problem immediately.
- **`kill_on_drop(true)` on the spawn** so a Ctrl-C anywhere in the agent loop kills the child instead of letting it outlive X0NE.

**5 new tests**: timeout default (180s), env override (30s / 600s), fallback on garbage env value, `run_cli_times_out_on_stuck_subprocess` (uses `/usr/bin/env sleep 999` with a 1s timeout — must abort in <5s), `run_cli_surfaces_zero_output_with_nonzero_exit` (uses `/usr/bin/false`).

#### Telegram + Matrix slash commands — push messages from inside the TUI

`/telegram`, `/telegram msg <text>`, `/tg`, `/tg msg <text>`, `/mx`, `/mx msg <text>` all work as local slashes inside tui2 (no LLM round-trip). Status slashes report paired chats + bot username; `msg` slashes spawn a fire-and-forget tokio task that loads the saved config and sends via the Bot API / Client-Server API. Setup slashes (`/telegram setup` / `/mx setup`) print a clear pointer to run `x0ne --telegram-setup` / `x0ne --mx-setup` in the host terminal, since the QR scan + rpassword prompt need terminal output that the alt-screen TUI would clobber.

#### Matrix bridge — open-protocol counterpart to the Telegram bridge

New module `x0ne-orchestrator/src/matrix.rs` + CLI flags `x0ne --mx-setup` and `x0ne --mx` (the chat-protocol Matrix; spelled `mx` to avoid colliding with the pre-existing `--matrix` LLM matrix-mode flag).

- **Why both** — Telegram wins on setup smoothness (30s with @BotFather); Matrix wins on data sovereignty (self-hostable Conduit/Dendrite), end-to-end encryption (default-on), and standard openness (Client-Server API is an IETF-style spec). They share the same agent loop, you can run them in parallel.

- **Pairing** — `x0ne --mx-setup` prompts for homeserver URL, bot user, bot password (rpassword — never echoed), logs in via `m.login.password`, persists the access token + bot user id to `~/.x0ne/matrix.toml` (mode 0600). Then renders the SAME QR helper Telegram uses, pointing at `https://matrix.to/#/<bot-user-id>` — scan with your phone, Element / FluffyChat / etc. opens the bot's profile, tap "Start chat", X0NE sees the DM and whitelists your Matrix ID.

- **Headless bridge** — `x0ne --mx` long-polls `/sync`, forwards whitelisted senders' messages as `UserInput`, sends assistant turns back as `m.room.message` events with markdown→HTML formatting in `formatted_body`. Tool calls one-line. `AskUser` modals render as numbered options + reaction emojis (1️⃣ 2️⃣ 3️⃣ …) — the bot reacts to its own message with each emoji so they show as tappable buttons under the message in any client. Tap = `m.reaction` event = `TuiCommand::AskAnswer` to the agent. Most native UX achievable without custom client extensions.

- **Lean deps** — Direct `reqwest` calls to the Client-Server API, no `matrix-rust-sdk` (which brings ~80 sub-deps for E2EE state machine, SAS verification, sliding sync — features we don't need yet). 5 endpoints total: `/login`, `/sync`, `PUT /rooms/{room}/send/m.room.message/{txn}`, `PUT /rooms/{room}/send/m.reaction/{txn}`, `POST /rooms/{room}/join`. Reuses the `qrcode` crate from the Telegram bridge for QR rendering.

- **10 new tests** cover homeserver trailing-slash strip, whitelist idempotency, matrix.to URL builder, percent-encoding (preserves `!` and `:` for Matrix room IDs while escaping spaces / slashes), transaction-id uniqueness, mini markdown→HTML converter (bold, code, HTML-escape), number emoji mapping (1️⃣ → 🔟, fallback `[N]` past 10), AskUser reaction lookup (correct event id + emoji → label), sync invite shape extraction.

Workspace: **1277 tests passing**, 0 warnings (release).

#### Shell mode — `Ctrl+S` turns tui2 into a real shell, with the agent watching

Press `Ctrl+S` from anywhere in tui2 → the body becomes a `🐚 shell · <cwd>` page, the input bar runs `bash -c` on the host, Enter executes, output scrolls into a trail. Esc / `Ctrl+S` / `exit` returns to agent mode. CWD is tracked client-side so `cd src/` persists; output is capped at 64 KB per command; Up arrow recalls the last command.

The killer feature: **the agent sees what you did**. When you exit shell mode and send the next message, the runner automatically prepends a `[shell trail since last turn]` system note with the last 8 commands (cmd · exit · duration · stdout snippet · stderr snippet) so the LLM reacts to actual host state instead of asking "what did you run?". The shared trail is then cleared so each turn sees only fresh activity.

Implementation:
- `AppState.shell_mode: bool` (toggle), `shell_cwd: PathBuf` (canonicalised on `cd`), `shell_history: Vec<ShellEntry>` (cmd · cwd · stdout · stderr · exit · duration), `shell_running: Option<String>` (single in-flight cmd, blocks Enter from queueing).
- `events::submit_shell_command` handles `cd` / `exit` builtins inline; everything else spawns `std::process::Command::new("bash").arg("-c").arg(cmd).current_dir(state.shell_cwd)` synchronously and captures stdout+stderr with the 64 KB cap (`MAX_SHELL_BYTES`).
- `events::submit_input` prepends `state.shell_context_note()` to the next user message and clears the trail — bounded to 8 entries × ~600 chars so the token cost stays in single-thousand digits.
- `render::draw_shell_page` paints the trail (cmd in teal-bold, stdout in soft grey, stderr in warn, footer `└ ✓ exit=0 · 12ms`) and bottom-aligns when overflowing.
- Slash bar contextually shows `^S exit shell · ↑ recall · Esc back · ^C quit` while in shell mode, and `^S shell` joins the default footer otherwise.
- Cheat sheet (F1) gets a new `^S → shell mode` row.

**10 new tests** cover `^S` toggle (in + out), Esc exit, Ctrl-C escape hatch, `cd <dir>` persistence with canonicalisation, `cd /nonexistent` error envelope, real stdout capture (`echo hello-x0ne`), `exit` builtin, `shell_context_note` None-when-quiet, and summary format.

Workspace: **1323 tests passing**, 0 warnings (release).

#### Multimodal — `@file.png` for image vision

Drop `@screenshot.png`, `@~/Desktop/foo.jpg`, or `@/abs/path/img.webp` anywhere in your TUI message and X0NE loads + base64-encodes the file, attaches as `ImageAttachment { media_type, data }` to the next `Message.images`. The existing vision plumbing in `x0ne-orchestrator/src/api.rs` then routes through Anthropic's multi-part content blocks or Ollama's `images` array — both backends already wired. Subprocess backends (Claude Code, Codex) see just the text + a marker for now; native attachment is a follow-up.

- **Detection** (`tui2::app::extract_image_refs`): scans whitespace-separated tokens, recognises `.png / .jpg / .jpeg / .webp / .gif` (case-insensitive), strips trailing punctuation (`,.;:!?)]}>\"`), expands `~/`, dedupes, returns paths in order.
- **Loader** (`tui2::runner::load_image_attachment`): reads the file, caps at 8 MB, sniffs media type from extension, base64-encodes. Surfaces clear errors (`read failed`, `image too large`, `unsupported image extension`) which appear as red event lines so the user knows what went wrong.
- **Wire** new `TuiCommand::UserInputWithImages { text, image_paths }` and matching runner handler — keeps the simple `UserInput(String)` path untouched.
- **Visual feedback**: tui2 echoes `🖼 attached N images for vision: …` as a system line when an image ref is detected, so the user knows X0NE actually parsed it.

#### Web search + sources panel — `WebSearch` action and `📚 sources` side panel

- **`WebSearch { query, limit }`** new typed action in `x0ne-runtime`. Returns a flat `[{title, url, snippet}]` list so the LLM no longer has to scrape search-engine HTML by hand (saves 2–3 round-trips per query). Two backends:
  - **Brave Search API** when `X0NE_BRAVE_API_KEY` is set (2000 reqs/mo free).
  - **DuckDuckGo HTML scrape** as the no-key default (POST `/html/`, anchor on `result__title` / `result__snippet`, unwraps the `//duckduckgo.com/l/?uddg=…` tracker URLs).
  - Brave auto-falls back to DDG on rate-limit / expired key.
  - Both paths emit a uniform `{"kind":"web_search_result","query","source","count","results"}` envelope.
- **`📚 sources` side panel** (new — cycled between `▶ pipeline` and `❤ pulse`). Auto-populated from any `FetchUrl`, `BrowseWeb`, or `WebSearch` ToolEnd via the `extract_sources` helper — the LLM doesn't cooperate, the panel just observes. Each row: origin glyph (🔍 WebSearch · 🌐 BrowseWeb · ● FetchUrl · ✗ failed) + title + age + URL on a continuation row (terminal-clickable, underlined). Per-query grouping when sources came from a `WebSearch`. Bounded at 200 entries; full audit lives in `~/.x0ne/sessions/<ts>.jsonl`.
- **15 new tests**: `urlencoding_minimal`, `parse_ddg_html` (canonical layout, limit respect, unparseable input), `clean_ddg_url` (tracker unwrap + passthrough), `strip_html_tags` (HTML entity decode), `web_search` (empty-query reject + huge-limit clamp), `extract_sources` (FetchUrl one-entry, BrowseWeb title pickup, WebSearch fan-out, unrelated-action passthrough, failure marker), `extract_image_refs` (8 tests covering extensions, dedup, tilde expansion, absolute paths, punctuation, non-image rejection), `load_image_attachment` (PNG+JPEG mime sniff, unsupported-ext reject, missing-file error, 8MB cap).

Workspace: **1313 tests passing**, 0 warnings (release).

#### Telegram echo into TUI — see what came from your phone

When you chat from Telegram with the bridge ON (`/telegram on`), the TUI now renders your incoming messages with a clear source tag: `› [📱 telegram] your message here`. Same for Matrix: `› [💬 matrix] …`. Cross-bridge fan-out: if both bridges are on, a Telegram message also lands in Matrix as `📱 via telegram · …` so you see cross-channel activity wherever you're looking.

Implementation: new `TuiCommand::ExternalUserInput { source, text }` and matching `AgentEvent::ExternalUserInput` — the runner emits the event before processing the user turn so the TUI's `apply_event` renders the line in real time. Each bridge filters its own echo to avoid duping.

#### Telegram bridge — control X0NE from your phone with one QR scan

New module `x0ne-orchestrator/src/telegram.rs` + two CLI flags (`x0ne --telegram-setup` and `x0ne --telegram`) that let you drive X0NE remotely via a Telegram bot:

- **One-scan pairing** — `x0ne --telegram-setup` prompts for the bot token once (saved mode-0600 to `~/.x0ne/telegram.toml`), generates a fresh 8-byte hex pairing token, and renders a half-block ASCII QR code in the terminal pointing at `https://t.me/<bot>?start=<token>`. Scan with the camera → Telegram opens the bot and auto-sends `/start <token>` → X0NE captures the `chat_id` and whitelists it. No copy-paste, no typing on mobile, 5-min TTL on the pairing token.

- **Headless bridge** — `x0ne --telegram` runs the same agent loop as `tui2` but without a screen. Inbound messages become `UserInput`s; assistant responses (markdown-formatted, capped at 3800 chars) are sent back as Telegram messages. Tool calls surface as compact one-liners (`▶ ReadFile · /etc/hostname` then `✓ ReadFile · 3ms · 3B`). The bridge uses `tokio::select!` to merge Telegram updates and runner events on a single loop, no extra threads.

- **AskUser ↔ inline keyboard** — when the agent emits an `AskUser` action, the bridge renders the options as Telegram inline-keyboard buttons. One tap on the phone → the choice lands in the agent's tool-result slot via `TuiCommand::AskAnswer`, the turn continues. This makes the human-in-the-loop primitive feel native on mobile.

- **Security** — bot token stays on disk mode 0600, never reaches the LLM. Only whitelisted `chat_id`s pass the door (unknown chats get a polite refusal and bounce). Every inbound message is audited in the per-session jsonl.

- **Lean deps** — used direct `reqwest` calls to the Telegram Bot API instead of pulling `teloxide` (which brings ~50 sub-deps for features we don't need). Only one new crate: `qrcode = "0.14"` (pure-Rust, no transitive C deps) for the terminal QR. Adds ~0.2 MB to the release binary and ~5s to the cold build.

- **12 new tests** cover pairing token shape + uniqueness, deep-link URL format, QR rendering produces non-empty ASCII with the right glyphs, `/start <token>` matching (simple form, `@BotName` suffix variant, mismatch rejection, non-message updates ignored), tool start/end formatters, message cap helper, whitelist idempotency.

Workspace: **1267 tests passing**, 0 warnings (release).

#### tui2 — frame overhaul: AskUser modal, Tasks/Cron/Pipeline/Pulse panels, live status, help overlay

The next-generation TUI (`make run-tui2` or `x0ne --tui2`) graduated from "skeleton" to "a frame for LLMs" in one push:

- **`AskUser` meta-action + auto-trigger on `policy_denied`** — agent can pause mid-turn and pop a multi-choice modal (single- or multi-select, with `recommended` highlight). When the runtime rejects an action because it needs human approval, tui2 auto-synthesises a calm 2-option modal ("Find a safer way" / "Stop and explain") instead of dumping a red error wall. Honest copy: each option's description reflects what actually happens next; the result lands in the LLM's tool-result slot so the next turn reacts naturally.

- **Three new agent meta-actions** that drive dedicated side panels:
  - `TaskAdd { title }` / `TaskDone { id }` / `TaskClear` → **Tasks panel** (`✓`) with open-at-top, completed-at-bottom strikethrough, counter header.
  - `CronSchedule { label, in_secs|at_unix, prompt? }` / `CronCancel { id }` → **Cron panel** (`⏰`) with live countdown (`in 2m 13s` / `in 4h`). With `prompt` set, the runner self-triggers via a tokio 1Hz ticker; without, it's a passive reminder.
  - `PaintBadge { … }` already existed — now also logged to the session jsonl for replay.

- **`Pipeline` panel** (`▶`) — sticky per-step breakdown of the most-recent Pipeline action (status · duration · 1-line summary derived from `size_bytes` / `sha256` / `stdout` / `matches` / `content`). Mirrors tui1's pipeline-preview but as a panel so you can scroll the conversation and come back.

- **`Pulse` panel** (`❤`) — live session heart-rate monitor: every turn / tool / snapshot / ask / goal / task / cron event becomes a coloured dot + label + age (`5s` / `2m` / `1h`). Auto-derived from `AgentEvent`s, zero new APIs.

- **Live status row** — 1-row dim italic strip just above the input that shows what X0NE is doing *right now*: `⋯ x0ne is thinking · 2.3s` while waiting on the first token, `✎ x0ne is writing · ~142 tok` during streaming, `▶ x0ne is running Pipeline · 12s` during tool dispatch. Spinner cycles at 320ms. Zero space consumed when idle.

- **Help overlay (F1 / `/help` / `Ctrl-?`)** — centred floating cheat sheet with 3 columns: keyboard shortcuts · slash & panels · agent actions. Any key dismisses; Ctrl-C still quits.

- **Local slash commands** (no LLM round-trip): `/task add <title>`, `/task done <id>`, `/task clear`, `/cron in <secs> <label> [-> <prompt>]`, `/cron cancel <id>`, `/help`.

- **Exhaustive session audit jsonl** — every action (bridge actions, meta-actions, AskUser questions + user answers, PaintBadge frames, policy-denied envelopes) is persisted to `~/.x0ne/sessions/<ts>.jsonl` as a `type:"action"` + `type:"action_result"` pair, giving you the "requested" and the "effective" for full replay / dataset extraction.

- **23 new tests** cover live status (4 states), AskUser modal mechanics (8 cases), Pipeline result parsing (ok + failed step), `pretty_policy_reason`, `result_is_policy_confirm_required` (4 cases), help overlay toggle (F1 + Ctrl-? open · any key closes · Ctrl-C still quits), local slash commands, and panel renderers (smoke tests so empty/populated never panic).

Workspace: **1255 tests passing**, 0 warnings (release).

#### `RenderTemplate` — one-shot templated tool calls

New typed Action that lets the LLM emit a single fluent string with
`{{tool(args)}}` placeholders; the runtime parses, dispatches each
placeholder through the regular action executor, and splices the live
result back in. Solves the "compose-step re-prompt" tax — for prompts
like *"how many r in strawberry?"*, the model now needs **one** LLM
round-trip instead of two (emit StrCount → wait → re-prompt to phrase
the answer).

- Two placeholder forms: call (`{{ str_count("strawberry","r") }}` —
  snake_case, positional binding against a built-in table covering math,
  string, file_info, checksum, system_info, …) and JSON
  (`{{ Pascal { …params… } }}` — works for every Action, no positional
  table required).
- Recursion: up to `max_depth` substitution passes (default 2, capped at
  8). Lets a tool's rendered result itself contain `{{…}}` markers.
- `strict` flag toggles abort-on-first-failure vs leave-raw-token-and-log.
- Output is `Generic { rendered, calls[], passes }` — every substitution
  is logged with `{ tool, args, ok, value, duration_ms }`, identical
  audit trail to a Pipeline.
- Policy: each placeholder is policy-checked individually at dispatch
  time. No privilege elevation; `{{run_command(…)}}` hits the same
  allowlist as a top-level `RunCommand`.
- Exposed via `mcp__x0ne-gateway__x0ne_render_template` for direct use
  from Claude Code / Codex.
- Dataset export's SFT system prompt updated so the LLM learns this
  pattern at fine-tune time.
- 14 unit + E2E tests; see `x0ne-runtime/src/executor/template.rs`.

#### LLM credential auth-forward into the sandbox

`LlmCall`, `OllamaApi`, `OllamaChat` gain an optional `auth` field
(`"$ENV:ANTHROPIC_API_KEY"` syntax — resolved at request time so the
secret never lands in the trajectory JSONL). Headers auto-routed:
Anthropic endpoints get `x-api-key` + `anthropic-version`, everything
else gets `Authorization: Bearer …`. Anthropic body shape is now
correct too (`POST /v1/messages` with `{model, max_tokens, messages,
system?}`, response decoded from `content[0].text`).

Sandbox forwarding via new Makefile `X0NE_FORWARD_ENV` allowlist
(default: `OLLAMA_HOST ANTHROPIC_API_KEY OPENAI_API_KEY
X0NE_OPENAI_COMPAT_KEY X0NE_LLM_PROVIDER X0NE_LLM_MODEL`) — only listed
vars cross into `incus exec` / `podman exec`; unset host vars are
silently skipped.

New `/auth` slash command + tab-completion entry + help section: shows
which keys are set on the current process, with the secret redacted to
its last 4 chars. Detects sandbox vs host context so the operator can
tell whether listed vars came in via the forward or originate on this
host. See `SECURITY.md › LLM credential forwarding` for the threat
model.

### Architecture

#### Incus container management migrated to the orchestrator — privilege boundary made structural

All Incus primitives (`RunIncus`, `RunIncusGroup`, plus the four
`IncusCreateSnapshot`, `IncusRestoreSnapshot`, `IncusListSnapshots`,
`IncusDeleteSnapshot`) moved out of `x0ne-runtime` and into a new
`x0ne-orchestrator/src/incus_actions.rs` module.

- **Why it matters**: container management is structurally outside the
  runtime's privilege boundary. The runtime may itself run *inside* the
  Incus container these primitives manipulate — a process inside a cage
  has neither the privilege nor the topological awareness to operate on
  containers safely. Until now the friction was patched at runtime by
  spawning a *second* host-side `x0ne-runtime` ("incus_bridge") with a
  hardened TOML; that workaround is gone.
- **What this buys**: the runtime no longer compiles any Incus code.
  Send `{"type":"RunIncus",…}` through the bridge and the runtime
  returns `unknown variant`. The orchestrator intercepts these action
  types before forwarding, dispatches to its own `incus` CLI calls on
  the host, and returns the result. The privilege boundary is now a
  property of the type system, not a policy check that can be bypassed.
- **Wire format unchanged**: the LLM emits the same JSON for these six
  action types. `DescribeAction` and `ListActions` now mark them as
  `source: "orchestrator"` (T6 contract).
- **What was removed from `x0ne-runtime`**: the `Action::RunIncus` /
  `RunIncusGroup` / `IncusCreate*Snapshot` enum variants, the
  `IncusTask` sub-struct, the `IncusSandboxOutput`/`IncusGroupOutput`/
  `IncusTaskResult` result variants, the `ErrorKind::IncusImageDenied`
  variant, the `prim-incus` Cargo feature, the entire
  `executor/incus.rs` module, the `tests/incus_integration.rs` test,
  and the `x0ne-runtime-incus-only.toml` sandboxed config. The
  `policy.rs` `is_run_command()` no longer has Incus arms. ~700 LOC out.
- **What was added to `x0ne-orchestrator`**: `incus_actions.rs` (~700
  LOC: `ContainerGuard` RAII, image allowlist, file-injection guard,
  env-key validator, low-level `incus` CLI wrappers for launch / copy /
  push / exec / pull / snapshot, and six top-level dispatchers). New
  `IncusConfig` in `Config` with sensible defaults (`default_remote =
  "local"`, fail-closed `allowed_images`, etc.). Six new
  `OrchestratorMeta` entries in `prompt.rs::orchestrator_registry()`.
  All six action types added to `ORCHESTRATOR_ACTION_TYPES`. The
  `is_incus_action()` helper and `incus_bridge` plumbing across
  `agent_loop.rs` / `subagent.rs` / `sysaicalls.rs` / `x0ne-code/src/main.rs`
  are gone. **18 new tests** (15 in `incus_actions::tests`, 3 in the
  `prompt::tests::describe_orchestrator_action_*` cases extended to
  cover Incus types).
- **Pitch**: X0NE is the agent runtime where the LLM **structurally
  cannot** call container-management primitives outside its sandbox.
  Not by policy check, by code structure.

### Added

#### File-level snapshots: directory recursion + atomic rollback + reflink-aware copy

`CreateSnapshot` and `RestoreSnapshot` (host filesystem snapshots, distinct
from the Incus subvolume snapshots above) gained three substantial upgrades:

- **Directory recursion**: `paths` now accepts directories. Each entry
  is walked via `walkdir` (no symlink-following) and every regular file
  inside is captured. Previously each file had to be listed
  individually — unusable beyond toy examples.
- **Atomic restore**: `RestoreSnapshot` now records the original input
  paths as `roots` in the manifest. At restore time, for each directory
  root, the runtime walks the live tree and **deletes any file that
  isn't in the manifest** — i.e. files the agent created after the
  snapshot. Result: state-after-restore == state-at-snapshot, byte for
  byte. Previously restore only re-wrote the captured files; rogue
  additions persisted, which made "rollback" a half-truth. The
  `SnapshotRestored` output gains `files_removed: usize` so callers
  can see the cleanup count.
- **Reflink-aware copy** (`try_reflink_or_copy`): on Linux this calls
  the `FICLONE` ioctl (works on btrfs, XFS with `reflink=1`, OCFS2);
  on macOS APFS this calls `clonefile()`. Falls back transparently to
  `std::fs::copy` when the filesystem doesn't support CoW. Tracked in
  the manifest and surfaced in the output as
  `method: "reflink" | "copy" | "mixed"`. On btrfs/APFS the per-file
  copy is now O(1) bytes via kernel CoW; on ext4/tmpfs the behaviour
  is identical to before.
- **Backwards-compat**: legacy manifests (without the `roots` field)
  restore as before — no cleanup pass, no surprises. Covered by a
  dedicated test (`restore_legacy_manifest_without_roots_skips_cleanup`).
- **`scope: "filesystem"`** is now emitted on every successful
  `SnapshotCreated` / `SnapshotRestored` so callers can tell host-FS
  snapshots apart from Incus subvolume snapshots at a glance.
- **Both new fields are `Option<String>`** with `skip_serializing_if`
  so the wire format stays compatible with older runtimes/orchestrators.

#### Viral demo `make demo-undo` — agent breaks the project, X0NE undoes it in 200 ms

A self-contained 30-second narrated demo (`scripts/demo-undo.sh`) that
plays out a fake Stripe payment service, snapshots it, lets a scripted
"agent" delete + clobber + write rogue files, then atomically restores
to the pre-agent state. Visible counter at the end: `6 restored, 2
cleaned-up`. Designed for screencast capture (`make demo-undo-fast`,
no pauses). Companion docs `scripts/DEMO-UNDO.md` (recording instructions
+ variants) and `scripts/DEMO-PITCH.md` (tweet thread, HN post body,
launch checklist).

### Fixed

#### Pre-existing test failures on `main` brought back to green

Discovered while validating the snapshot/Incus work. Same branch was
shipping 7 broken tests:

- `x0ne-orchestrator::prompt::lazy_prompt_is_minimal` and
  `lazy_prompt_stays_below_token_budget`: the lazy system prompt grew
  past its 2300-char / 560-token ceiling after the T0.5–T11 hardening
  pass added FORMAT/MEMORY/COMPOSITES contracts. Ceilings raised to
  3000 chars / 750 tokens with comments explaining the bump policy
  (only on deliberate primitive-family additions, never to silence drift).
- `x0ne-runtime::executor::pipeline::nested_pipeline_executes_inner_steps`,
  `nested_pipeline_failure_propagates`, `nested_pipeline_in_parallel_mode`,
  `triple_nested_pipeline_succeeds`: nested-Pipeline tests built their
  policy from `AppConfig::defaults()` (Pipeline disabled by default) so
  the inner step was rejected before execution. Each test now mirrors
  the existing `pipeline_rejects_at_max_depth` pattern and explicitly
  enables `Pipeline` in its policy config.
- `x0ne-runtime::executor::math::test_pow_bigint_9_pow_999`: asserted a
  wrong tail digit string. Verified against Python (`9**999` ends with
  `…868783790048889`, not `…656482166408`) and the assertion corrected.
  The error message also now prints the last 20 digits on failure to
  make future drift instantly diagnosable.

### Fixed

#### Autowrap sandbox_cmd now matches Makefile conventions — `x0ne` starts cleanly on Makefile-provisioned containers

- **Problem**: fresh `make install` + plain `x0ne` bailed with `x0ne-runtime health check failed — no response to ListActions` / `x0ne-runtime returned an empty response line`. Root cause: autowrap's `Runtime::Incus::default_runtime_bin()` returned `/root/x0ne-runtime`, but `make sync-incus` and `make setup-incus` deploy the binary to `/opt/x0ne-runtime`. The `sandbox_cmd` autowrap generated (`incus exec x0ne-sandbox -- /root/x0ne-runtime --mode stdio`) pointed at a missing binary — `incus exec` exits 1 with empty stdout, and the bridge reports "empty response line". Every machine provisioned via the Makefile was bricked for `x0ne`.
- **Additionally**: autowrap's `sandbox_cmd` did not pass `--config /root/x0ne-runtime.toml`, meaning the runtime inside the container would boot with defaults rather than the hardened policy that `make run-incus` applies. The orchestrator's banner (`⚡ Incus bridge ready (host-side, hardened config)`) was technically correct for the orchestrator side, but misleading — the runtime side was running *un*hardened.
- **Fix**: three alignments in `x0ne-cli/src/autowrap.rs` + `x0ne-cli/src/container_setup.rs`:
  1. **`Runtime::Incus::default_runtime_bin()` → `/opt/x0ne-runtime`** (previously `/root/x0ne-runtime`). Both runtimes now use the same path, matching `INCUS_CORE_BIN` / `PODMAN_RUNTIME_BIN` in the Makefile.
  2. **`sandbox_cmd()` now includes `--config /root/x0ne-runtime.toml`** for both runtimes, so the in-container runtime boots with the hardened sandbox policy. Matches `make run-incus` / `make run-podman` one-for-one.
  3. **`container_setup.rs::setup_incus` deploys the binary to `/opt/x0ne-runtime`** (previously `/root/x0ne-runtime`) and **also pushes the hardened config to `/root/x0ne-runtime.toml`** (previously skipped — now mirrors what `make sync-incus` does on line 188 of the Makefile). Same for Podman's new config-push step.
- **Config location discovery**: a new `locate_sandbox_runtime_config()` helper looks for `x0ne-runtime-sandbox.toml` in XDG (`~/.config/x0ne/`), then the source tree, then `/etc/x0ne/`. If none exist, we skip the config push and log "host copy missing — runtime will use defaults" so the operator knows they're running less-hardened.
- **Tests**: two new regression pins in `autowrap.rs::tests`.
  - `incus_sandbox_cmd_matches_makefile_flow` now asserts the exact shape `incus exec x0ne-sandbox -- /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio` (previously asserted the wrong binary path, which let the regression sneak in).
  - `incus_default_runtime_bin_matches_makefile_opt_path` — standalone pin so a refactor of `default_runtime_bin()` can't silently break interop without a red test.
  - `podman_sandbox_cmd_has_dash_i_and_config_flag` — existing `-i` pin extended to cover `--config` too.

#### Context-loss bug: LLM no longer loses mid-session memory on long runs

- **Problem**: on long sessions, `Conversation::trim_if_needed` would `drain(0..2)` oldest messages until under the `max_context_tokens` budget OR `messages.len() <= 2`. The floor of 2 was too low: if the budget was still busted at 2 messages, the loop exited leaving the LLM with system prompt + 1 short user result. A real rpc-rust session log showed `tokens_in` collapsing from **~16659 to ~1070 mid-session**, at which point gemma replied `"I am ready. Please provide the task"` **twice** as if the session had just started. This was silent — no warning, no visible truncation, operators only noticed from the log afterwards.
- **Fix**: three changes together.
  1. **Floor raised to a configurable minimum**. New `ConversationConfig::keep_recent_min_messages` (default **8**, clamped to ≥4 to preserve pairing) replaces the hardcoded `> 2`. Under severe budget pressure the floor wins — an extra `[Context truncated]` marker is injected at the head so the model knows history was cropped rather than absent, killing the "ready for task" regression even in the pathological case.
  2. **Pairing preservation**. The trim loop now drops 1 message when the head is not a `User` turn (realigns the history), then 2 at a time (user/assistant pair) — so the model never receives a transcript that starts with an orphan assistant reply referring to a no-longer-present user question.
  3. **Trim telemetry**. `Conversation::trim_dropped_count()` exposes the lifetime count of dropped messages. The marker is updated **in place** (not stacked) on subsequent trims, so the LLM sees one marker with the cumulative count instead of a stack of markers accumulating at the head of the conversation.
- **Propagation**: the new floor is wired through every `Conversation::new` call site: `x0ne-cli` main loop, TUI mode, matrix mode, HTTP `/serve`, SpawnAgent inline sub-agent, SpawnWorld remote sub-agent. A new builder `Conversation::with_keep_recent_min(n)` is the one-line change.
- **Config**: `x0ne-orchestrator.toml` documents the new key under `[conversation]`; set it to a higher value if you see `[Context truncated]` markers firing too often, lower it only if you're running on a budget so tight 8 messages bust it.
- **Tests**: four new tests in `x0ne-orchestrator/src/conversation.rs::tests` pin the contract — `token_limit_triggers_truncation` (floor honored, dropped-counter increments, marker at head), `context_marker_is_updated_in_place_not_stacked` (marker singularity), `with_keep_recent_min_clamps_to_safe_floor` (0 → 4 clamp), `trim_realigns_when_head_is_orphan_assistant` (pairing preserved).

#### BrowseUrl now flags low-signal extractions instead of silently lying to the LLM

- **Problem**: `BrowseUrl` drove its output through a naive `strip_html` pipeline with no content-quality check. On Google/DuckDuckGo the LLM got the `"Click here if you are not redirected"` JS shim and proceeded to cite it as a real search result. On GitHub repo URLs the extractor returned only the header/nav chrome (`"Skip to content Sign in Sign up…"`) and the LLM confidently claimed the README said things it hadn't read. On a real session this ended with gemma fabricating a `github.com/openclaw/openclaw` URL, `git clone`-ing it (timeout 30s), and still claiming the README contents were what it said. `BrowseUrl` was lying to the model by not distinguishing "200 OK with 3000 bytes of output" from "200 OK with 3000 bytes of nothing useful".
- **Fix**: a new `detect_low_signal(text, title, url)` heuristic runs after HTML stripping, before the result is handed back. When the extraction looks like a known failure mode, `ActionOutput::BrowseResult` is returned with `low_signal: true` + a machine-readable `low_signal_reason` tag. The action still succeeds — we don't want to flip the LLM into error-recovery for a page that might legitimately be thin — but the prompt can now branch on the tag instead of treating every 200 as ground truth.
- **Failure modes detected** (five distinct tags so prompts can act differently):
  - `js_redirect_shim` — `"click here if you are not redirected"` and variants. Signature of Google/DuckDuckGo when JS hasn't run. Recovery: retry with headless JS or different URL.
  - `noscript_wall` — `"please enable javascript to continue"` in the first 400 chars. Recovery: same.
  - `nav_chrome_only` — GitHub-style `"skip to content" + "sign in"` without README markers (` ``` `, `installation`, `usage`, `readme`), OR very low unique-word-ratio (< 0.12) at < 500 words. Recovery: try the raw `github.com/.../raw/main/README.md` URL or an API endpoint.
  - `too_short` — under 120 words of prose. Catches empty pages, 404-disguised-as-200, paywalls truncated to teasers.
  - `search_engine_placeholder` — URL on a known search engine (google/duckduckgo/bing/kagi) + < 300 words. Distinct tag because recovery is *different*: the LLM should switch to a WebSearch-style primitive or pick a specific URL, not retry BrowseUrl on the same search engine.
- **Result schema**: `ActionOutput::BrowseResult` now carries `low_signal: bool` (skipped when false — wire format stays compact for the 99% case) and `low_signal_reason: Option<String>`. The brief result line shown in the CLI/TUI adds a `⚠ <reason>` suffix when flagged, so operators see it without having to pretty-print the JSON.
- **Tests**: six new tests in `x0ne-runtime/src/executor/browse.rs::tests` pin the detector — one per tag plus `does_not_flag_real_article` (a 150-word legitimate Wikipedia-style prose sample) to catch regressions where we over-flag real content.

#### OllamaChat error when endpoint is sandbox-loopback now explains the fix

- **Problem**: running `x0ne-runtime` inside an Incus/Podman/Docker sandbox and pointing `OllamaChat` at the default `http://localhost:11434` produced a cryptic `"Request failed: error sending request for url (http://localhost:11434/api/chat): connection refused"`. Technically correct — `localhost` inside the sandbox is the sandbox's own loopback, not the host's — but the LLM and the operator both had to *know* that fact to diagnose it.
- **Fix**: when an `OllamaChat` or `LlmCall` connect error hits, `sandbox_localhost_hint(endpoint, err)` inspects three things: (a) endpoint is loopback (`localhost`, `127.0.0.1`, `::1`, `0.0.0.0`); (b) error is a connect failure (`reqwest::Error::is_connect()` — not timeout, not decode — so we don't misfire on "service answered but weirdly"); (c) process is inside a container (marker-file heuristic: `/.dockerenv`, `/run/.containerenv`, `/run/systemd/container`). All three → an actionable hint is appended to the error message: `host.containers.internal` for Podman/Docker, `incus network show incusbr0` for Incus, or switch the orchestrator to `bridge.mode="stdio"` on the host.
- **Why a hint rather than auto-redirecting**: there's no single correct host from inside a sandbox (Podman exposes `host.containers.internal`, Incus exposes `incusbr0.gateway`, rootless vs rootful differ). A hint surfaces the right commands for the user to pick, without guessing and silently changing endpoints.
- **Tests**: two tests in `x0ne-runtime/src/executor/ml.rs::sandbox_hint_tests` — hint does NOT fire for remote endpoints, hint does NOT fire for non-connect errors (prevents misfire on decode/timeout failures).

### Changed

#### **BREAKING**: `x0ne` now runs safe-by-default — plain invocation auto-wraps in Incus/Podman

- **Problem**: the historical `x0ne` binary bridged to `x0ne-runtime` via **stdio** on the host by default, meaning every `ReadFile`, `WriteFile`, `RunCommand`, `Pipeline` the LLM emitted hit the user's real machine. Safety lived in Incus/Podman *only when the user remembered to invoke the Makefile target* (`make run-incus` / `make run-podman`). Type `x0ne` instead of `make run-incus` and you shipped an autonomous agent with host-filesystem-write on your laptop.
- **New contract**: plain `x0ne` (no flags, no env vars) **autowraps itself in a sandbox container before the LLM is allowed to emit any action**. Preference order is deterministic (Incus → Podman → fail-closed), and the decision is surfaced to the user with a green `◆ sandbox: incus · x0ne-sandbox` banner right under the intro animation.
- **Decision tree** (exhaustive — see `x0ne-cli/src/autowrap.rs::decide_inner`):
  1. Explicit `--mode X`, `X0NE_BRIDGE_MODE=X`, or a non-default `[bridge] mode` in config → `RespectExplicit`: autowrap steps aside, the user knows what they're doing. Shown as a grey info line `◆ bridge: sandbox (X0NE_BRIDGE_MODE=sandbox)` so operators running `make run-incus*` / `make run-podman*` see their intent was picked up.
  2. `--no-sandbox` / `X0NE_NO_SANDBOX=1` → `UnsandboxedExplicit`: host execution, but with a loud red `⚠ UNSAFE — running unsandboxed on host` banner so no one can claim they didn't notice.
  3. Incus binary in PATH + `x0ne-sandbox` container exists (running or stopped — we auto-start) → `Autowrapped { Runtime::Incus }`. The Incus-first preference is intentional: `incus snapshot` is near-instant (btrfs CoW), `podman commit` + `rm -f` + `run` is 1-2 s per reset.
  4. Otherwise, Podman + `x0ne-sandbox-podman` exists → `Autowrapped { Runtime::Podman }`. Stdio is wired via the `-i` flag + `X0NE_ALLOW_LOOPBACK=1` env (the exact incantation that the recent Makefile `PODMAN_EXEC_FLAGS` fix produces — a unit test pins this so a refactor can't regress).
  5. Runtime installed but baseline container missing → `NeedsBootstrap`: fatal error pointing at `x0ne setup incus` / `x0ne setup podman`.
  6. Nothing installed → `MissingRuntime`: fatal error listing install URLs for Incus (preferred) and Podman (fallback), plus the `X0NE_NO_SANDBOX=1` escape hatch.
- **Configuration surface**: the rewrite targets `config.bridge.{mode, sandbox_cmd}` in-memory — TOML files are never written by autowrap, so a user's explicit `x0ne-orchestrator.toml` is always the source of truth for anything beyond mode/sandbox_cmd.
- **Escape hatches**:
  - `--no-sandbox` / `X0NE_NO_SANDBOX=1` — explicit host mode for dev laptops, CI, in-VM runs.
  - `--mode stdio|http|sandbox` — full bridge-mode control (autowrap sees this as "explicit" and bows out).
  - `X0NE_BRIDGE_MODE=X` env — same as `--mode X`, handy for shells/scripts.
- **Why fail-closed** when no runtime is available (rather than warning + falling back to host): a user typing `x0ne` on a fresh machine expects either "it works and it's safe" or a clear error. "It works but is silently unsafe" is the worst of all worlds — the exact class of bug this change exists to prevent.
- **Tests**: 12 unit tests in `x0ne-cli/src/autowrap.rs::tests` covering every branch of the truth table — CLI override respected, env override respected, non-default config respected, `X0NE_NO_SANDBOX` opt-out, Incus-wins-when-both-ready, Podman-fallback-when-Incus-missing, `NeedsBootstrap` for preferred runtime, `MissingRuntime` when neither is installed, and the two sandbox_cmd format pins (Incus shape matches `make run-incus`, Podman shape includes `-i` + `X0NE_ALLOW_LOOPBACK=1`).

### Added

#### `x0ne setup <incus|podman>` — one-command sandbox bootstrap, no Makefile required

- **Problem**: after `make install`, a user outside the source tree had no way to provision the baseline sandbox container. `make setup-incus` / `make setup-podman` only work inside the repo (and require `make` itself installed), so an `x0ne` binary dropped into `/usr/local/bin/` by a packager was effectively inert on first run. The autowrap flow would correctly detect `NeedsBootstrap` and tell the user to run a command that doesn't exist on their PATH.
- **New subcommand**: `x0ne setup [runtime]` — idempotent, no `make`, no bash scripts. Mirrors `make setup-{incus,podman}` one-for-one:
  1. Verify the requested container runtime is in PATH (`incus --version` / `podman --version`).
  2. Locate an `x0ne-runtime` binary to deploy. Priority order: `X0NE_RUNTIME_BIN` env → `~/.cargo/bin/x0ne-runtime` (what `make install` deploys) → `./target/release/x0ne-runtime` (source-tree fallback) → `./x0ne-runtime` (cwd). If none match, bail with an actionable error listing the three install paths.
  3. Launch the container (`incus launch images:debian/12 x0ne-sandbox` / `podman run -d --name x0ne-sandbox-podman debian:12 sleep infinity`) — skipped if it already exists.
  4. Install `python3 python3-pip curl git jq procps net-tools` via `apt-get`, gated behind a `which python3` canary so subsequent runs are no-ops (seconds, not minutes).
  5. Push the binary: `incus file push <bin> x0ne-sandbox/root/x0ne-runtime` or `podman cp <bin> x0ne-sandbox-podman:/opt/x0ne-runtime` + `chmod +x`.
  6. Capture a reset point: `incus snapshot create x0ne-sandbox baseline` (idempotent) or `podman commit x0ne-sandbox-podman localhost/x0ne-sandbox-baseline:latest` (overwrites previous, which is correct after a binary rebuild).
- **No-arg form**: `x0ne setup` (no runtime) auto-picks Incus if installed, else Podman — matching the autowrap preference so a user running `x0ne setup` and then `x0ne` gets the same runtime both times.
- **What this subcommand does NOT do** (deliberately, to keep the Rust port in the 300-LoC budget):
  - No cross-compile of x0ne-runtime for the container's glibc — if the binary at `~/.cargo/bin/x0ne-runtime` is too new for Debian 12, `make sync-incus-musl` / `make sync-podman-musl` remain the dev-side answer (they shell out to `cargo zigbuild`, which is a whole separate tool tree).
  - No remote Incus clusters — `make setup-incus INCUS_REMOTE=my-lab:` still owns that flow. Local `incus` only in the Rust path.
  - No baseline versioning (`localhost/x0ne-sandbox-baseline:v0.3-experimental`) — hardcoded `:latest` tag, same as the autowrap lookup expects.
- **Tests**: `container_setup::tests::locate_runtime_binary_rejects_missing_env_override` pins the "explicit configuration wins, even when it's wrong" contract (a bogus `X0NE_RUNTIME_BIN` must fail, not silently fall through to the PATH candidates). `base_packages_include_canary` guards the `python3` sentinel that both Incus and Podman setup paths use to determine whether to re-run `apt-get install`.

#### Boot intro animation plays at every `x0ne` startup (not just `--matrix`)

- **Problem**: the six-phase "noise → logo → pulse → gradient → subtitle → fade" entrance animation lived inside `matrix_mode::play_entrance_animation`, private to that module. Only `x0ne --matrix` got the delight; plain `x0ne` and `x0ne --tui` dropped the user straight onto a setup-wizard prompt.
- **Extraction**: new module `x0ne-orchestrator/src/intro.rs` — 260 LoC, zero new dependencies. Exposes two entry points:
  - `intro::play_boot_intro() -> Result<()>` — the "I just want it at startup" wrapper: enters the alternate screen buffer, hides cursor, plays the animation, restores the primary screen on Drop (RAII guard so panics don't leave the terminal in raw mode). This is what `x0ne-cli/src/main.rs` calls right after CLI parsing.
  - `intro::play_entrance_animation(stdout)` — the raw animation, takes an already-set-up stdout. `matrix_mode` uses this directly because it already owns the alt-screen lifetime for the whole session.
- **Skip conditions** (`intro::should_play`): non-TTY stdout (piped / redirected / CI), `X0NE_NO_INTRO=1`, or `TERM=dumb`. Plus `--no-intro` CLI flag (which sets the env var under the hood, so descendants inherit).
- **Exclusions**: serve mode (`--serve`) skips the animation because it has no terminal to animate on. `--health` also skips — that subcommand exits after a 1-second probe, a 2-second animation would be > 50% of its runtime.
- **Integration in matrix mode**: `matrix_mode::play_entrance_animation` is now a thin trampoline to `intro::play_entrance_animation`, so tweaks to the animation (extra pulse, different gradient) only need to touch one file. The 150-line duplicated body was deleted; the call-site at `run_matrix_loop` was left untouched.
- **Tests**: 3 unit tests — non-TTY no-op (returns Ok without touching the terminal, which would hang cargo test if it did), `X0NE_NO_INTRO=1` opt-out, `TERM=dumb` opt-out.

#### `make install` now deploys XDG config — no more missing host-side TOML

- **Problem**: `x0ne-cli/src/main.rs` looks up `x0ne-runtime-incus-only.toml` (the hardened host-side Incus bridge config) via two hardcoded relative paths: `x0ne-runtime/x0ne-runtime-incus-only.toml` and `x0ne-runtime-incus-only.toml` — both relative to `cwd`. A user running `x0ne` outside the source tree (which is the whole point of `make install`) gets a warning `x0ne-runtime-incus-only.toml not found — Incus bridge disabled` and every `RunIncus` action fails. Nothing in `make install` ever shipped those files.
- **Fix**:
  - `make install` now additionally copies `x0ne-runtime/x0ne-runtime-incus-only.toml`, `x0ne-runtime/x0ne-runtime.toml`, and `x0ne-runtime/x0ne-runtime-sandbox.toml` into `${XDG_CONFIG_HOME:-$HOME/.config}/x0ne/` with `install -m 644`. `mkdir -p` guards the first-time run.
  - The lookup in `main.rs` extends to `[cwd/x0ne-runtime/X, cwd/X, $XDG_CONFIG_HOME/x0ne/X, /etc/x0ne/X]` — source-tree first (dev iteration wins), then XDG (installed user), then system-wide (distro packagers can drop into `/etc/x0ne/` once and every user sees it).
- **Why not `/etc/x0ne/` by default**: `make install` writes to `~/.cargo/bin/` which is single-user, so we kept the TOML install in the same privilege boundary. A future `make install-system` could add `/etc/x0ne/` for multi-user setups, but that's a packager-owned target.

### Changed

#### **BREAKING**: `SpawnAgent` renamed to `SpawnWorld` — naming now matches behaviour

- **Problem**: the action named `SpawnAgent` in fact spun up a **fresh x0ne-runtime world** — a new container (in sandbox mode) or a new subprocess (in stdio mode) with its own bridge, its own filesystem, and its own conversation context. "Agent" suggests something lightweight running next to you; the actual semantics were closer to `git worktree + docker run`. The name leaked into system prompts, `DescribeAction` output, and the `/worlds` TUI, so the LLM had no way to tell that calling `SpawnAgent` meant paying a full cold-start cost every time — it picked it for trivial sub-tasks where inline delegation would have been 10× cheaper.
- **Rename surface** (18 files, every occurrence touched):
  - `x0ne-orchestrator/src/{sysaicalls,agent_loop,parser,prompt,display,serve,subagent,conversation,worlds_registry,worlds_view,worlds_cli,tui_mode,session_registry,ui}.rs` — action-type strings, match arms, orchestrator action table, and test fixtures.
  - `x0ne-orchestrator/tests/integration_demo.rs` (5 occurrences).
  - `x0ne-cli/src/main.rs` — benchmark prompts and live-mode action list.
  - `x0ne/README.md` (14) + `x0ne-orchestrator/README.md` (8) + `x0ne/scripts/stress-prompts.sh` (3).
  - `x0ne-orchestrator.toml` + docs references.
  - Docstrings and diagnostic messages (`SpawnWorld requires a non-empty 'id'`, `SpawnWorld unavailable: bridge context not configured`, etc.).
  - `WorldEvent::ActionStarted { action_type: "SpawnWorld", … }` and the lifecycle-close `ActionFinished` + `WorldCompleted` emitters.
- **No compat alias** — this is a workspace-internal action surface; every caller was updated in the same commit. A legacy `SpawnAgent` alias in the parser would have competed with the *new* `SpawnAgent` semantics introduced below and confused the lazy-discovery prompt ("which one do I get?"), so we chose a clean break over a long-lived synonym.
- **Tests**: `integration_demo::*` (22 cases) pass unchanged after the rename; `agent_loop::is_incus_action_rejects_non_incus_types` now asserts `!is_incus_action("SpawnWorld")` and `!is_incus_action("SpawnAgent")` (the new action shares the exclusion); `is_orchestrator_action` tests include both names.

### Added

#### `SpawnAgent` (v2) — same-space sub-agent sharing the parent's bridge

- **Why**: with `SpawnAgent → SpawnWorld` decoupled above, we now have a slot for the *cheap* spawn the LLM actually wants most of the time — "delegate this focused sub-task to a fresh context, but keep working in the same container/process". Previously, any delegation paid the `SpawnWorld` cold-start tax (new runtime process, new policy boot, new X0NE.md injection), even for 15-line tasks like "rename this symbol" or "summarise this file".
- **Semantics**:
  - Shares the **parent's** `Box<dyn bridge::Bridge>` — same container in sandbox mode, same `x0ne-runtime` subprocess in stdio mode, same filesystem, same policy state.
  - Runs **synchronously** inside `execute_spawn_agent`: spawns a fresh `Conversation` + `UsageTracker`, calls `run_agent_loop` inline (wrapped in `Box::pin` to break the recursive async-type cycle), and returns `{success, output.response, output.tokens_used, output.actions_executed, output.actions_breakdown, output.duration_ms, output.mode:"inline", output.bridge:"shared"}` directly — no `WaitAgent` roundtrip.
  - The sub-agent is passed `registry: None` + `spawn_ctx: None`, preventing recursive spawning (a SpawnAgent can't itself call SpawnAgent/SpawnWorld). This keeps the inline stack bounded and avoids surprise bridge contention from a nested spawn.
  - Registers in the same `AgentRegistry` as `SpawnWorld` (so `WaitAgent` / `ListAgents` / `BroadcastAgents` see both kinds), but with `result_rx: None` — the cached `result` is set before return, so any later `WaitAgent` on the id hits the fast path instantly.
  - Emits the full `WorldEvent` lifecycle (`ActionStarted → ActionFinished → WorldCompleted | WorldFailed`) so the `/worlds` snapshot and the TUI renderer don't have to special-case inline spawns.
- **Prompt guidance** (`prompt.rs::ORCHESTRATOR_ACTIONS` + `orchestrator_registry`): both actions now ship with explicit "choose SpawnWorld when you need parallelism / destructive sandboxing / strict isolation; choose SpawnAgent for focused delegation with shared state" descriptions and contrasting examples (SpawnWorld → researcher/security-audit; SpawnAgent → refactor-helper/doc-summarizer).
- **HTTP dispatcher** (`serve.rs`): `SpawnWorld` and `SpawnAgent` are grouped in the handler — in remote-runtime/HTTP mode the cold-start distinction is moot (the bridge is always remote), so both route to the same `spawn_agent_http` endpoint. The `output.mode` / `output.bridge` fields still let the model tell them apart.
- **Tests** (3 new in `sysaicalls::tests`):
  - `spawn_agent_rejects_empty_id` — empty `id` → `success:false`, registry untouched.
  - `spawn_agent_rejects_empty_task` — empty `task` → `success:false`, registry untouched.
  - `spawn_agent_rejects_duplicate_id_from_spawn_world` — collision across the shared namespace (SpawnAgent on an id already used by a prior SpawnWorld) → `success:false`, pre-existing entry untouched.
  - Backed by a trivial `MockLlm` (`LlmBackend` impl whose `send` is `unreachable!`) — these validation paths short-circuit before any LLM/bridge work, keeping the tests hermetic. Happy-path coverage is delegated to the live e2e harness in `x0ne-runtime/tests/`.

#### CLI prose renderer — width-capped, word-boundary wrapping with richer markdown

- **Problem**: LLM responses streamed to the CLI ran edge-to-edge of the terminal. At 240 cols the result was a "ligne-de-200-chars-illisible" experience, tables dissolved the moment the viewport widened, and long URLs or Rust type signatures were cut mid-grapheme by the terminal's own hard-wrap, destroying ANSI styling.
- **New module** `x0ne-orchestrator/src/prose.rs` (~600 LoC, 15 unit tests):
  - `render_prose(&str, &ProseLayout, &ProseTheme) -> Vec<String>` — pure function, no I/O, returns styled body lines ready to print. Kept separate from `terminal.rs` so the same renderer can be reused later for log export or a web preview.
  - **Word-boundary wrapping** via `textwrap = "0.16"` with `WordSplitter::NoHyphenation`, hanging indents for list continuations, and `break_words: true` as an overflow fallback for pathological tokens (long URLs, 80-char Rust path strings). Handles ANSI styling by wrapping before colouring — no escape-code-splitting bugs.
  - **Unicode-correct measurement** via `unicode-width` — emojis and CJK now count as 2 columns, so wraps align on Japanese names, command-palette icons, and rocket emoji the same way they do on ASCII.
  - **Tables**: proportional column shrinking; if a cell still overflows after compression it is **truncated with "…"** (wrap-in-cell is deferred to v2 — the layout engine needed is disproportionate to the gain at v1).
  - **Rendering**: headings (4 glyph tiers), ordered / unordered / nested lists with depth-aware bullets and checkbox support, GitHub-style callouts (`> [!NOTE]`, `> [!WARNING]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!CAUTION]`) with coloured continuous rails, fenced code blocks with subtle dark-background tint + language label + per-line truncation, horizontal rules stretched to the content width, and styled inline code with the same tint as fenced blocks.
- **Terminal integration** (`terminal.rs`):
  - `print_llm_message` reduced from ~600 lines of manual parsing to a thin dispatcher over `prose::render_prose`. Computes the effective width as `min(terminal_width - 4, config.display.response_width)`, frames the output with bars stretched to that width, and strips leading blank prose lines to avoid doubling with the initial bar rail.
  - `print_reasoning` now uses `prose::wrap_plain` for the same word-boundary + width-capping behaviour the body gets — reasoning blocks no longer escape the 120-col envelope when the model goes verbose.
  - Inline code picks up a matching subtle `on_truecolor(20,22,28)` tint so fenced and inline code read as the same visual language. `visible_cell_width` upgraded to `UnicodeWidthStr::width` — table alignment and ANSI-stripped measurement now match the rest of the renderer.
  - Several helpers (`parse_list_item`, `looks_like_block_latex`, `render_inline_styles`, `visible_cell_width`, `strip_html_tags`, `latex_to_unicode`) are now `pub(crate)` so `prose.rs` can reuse them without duplicating logic.
- **Configuration**:
  - `DisplayConfig::response_width: usize` (default **120**, capped by the terminal width). `0` disables the cap entirely — useful when piping into `less -R` or writing to a log file where external wrapping is preferred.
  - `x0ne-orchestrator.toml` ships a commented-out `[display]` block documenting `response_width`, `show_reasoning`, `show_actions`, and the `min(terminal_width - 4, response_width)` clamp rule — discoverable via `less x0ne-orchestrator.toml` without being active by default.
- **What's deliberately *not* in v1**: `syntect`-based syntax highlighting for fenced code blocks (adds ~3 MB to the distributed binary; subtle background tint is enough for v1 — wire up `syntect` when X0NE ships as an IDE replacement), and wrap-in-cell for overflow table content (truncate-with-ellipsis gives 90% of the readability at 10% of the layout complexity).
- **Preview harness**: `examples/prose_preview.rs` renders a reference markdown sample through the new renderer — makes it easy to eyeball the output against a wide range of widths (`RESPONSE_WIDTH=80 cargo run --example prose_preview` vs `RESPONSE_WIDTH=160`) without spinning up a full session.

#### `Action::If` — conditional branching without an LLM round-trip

- **Problem**: pipelines were strictly linear. Any "if X then A else B" decision cost a full LLM turn: emit `Assert`, wait for `AssertResult`, re-prompt, emit the chosen action. For autonomous loops (scaffold a project, check pre-conditions, retry on drift) the round-trip tax compounded — the agent paid one turn per decision even when both outcomes were known in advance.
- **New primitive** `{"type":"If","check":"<name>","then":[…],"else":[…]}` in x0ne-runtime:
  - Reuses the **exact** `check` grammar as `Assert` (`file_exists`, `file_contains`, `port_open`, `command_succeeds`, `env_set`, `eq`, `gt`, `lt`, `not_empty`) so the LLM has zero new vocabulary. The condition fields (`path`, `value`, `host`, `port`, `command`, `args`, `name`, `actual`, `expected`) are flat on the `If` object, matching `Assert` one-for-one.
  - `then` and `else` are both `Vec<Action>`; either can be empty (`branch: "none"` is reported in that case, useful for one-sided guards). The selected branch runs sequentially with stop-on-error, like a non-parallel Pipeline.
  - **Policy preflight covers BOTH branches** before any step executes — same contract as Pipeline/Try. A denied action in the `else` branch blocks the whole `If` even when the condition resolves to `then`. Guarantees no-surprise execution: if `If` returns `Allow`, every reachable step is already policy-approved.
  - Nesting counts against `MAX_PIPELINE_DEPTH` (3). `If` inside `If` inside `Pipeline` maxes out at 3 levels; deeper nests return `NestingNotAllowed` at depth-guard time, not after running half the branch.
  - New `ActionOutput::IfResult { check_ok, branch, check_detail, steps_total, steps_completed, stopped_early, results }` — carries both the condition verdict and the full per-step trace of the taken branch, so the model can reason about "what actually ran" in a single follow-up.
- **Surface**: registered in `meta::ACTION_REGISTRY` with three examples (build-lockfile fallback, port-check gate, math-eq trivia), surfaced in the compact prompt COMPOSITES table, and added to the orchestrator parser's static known-actions list for typo-correction.
- **Tests**: 7 unit tests in `executor::if_else` cover happy path (then-branch, else-branch), empty-branch no-op, branch-step failure propagation, policy preflight of the un-taken branch, real `file_exists` check bridged through `Assert`, and the `MAX_PIPELINE_DEPTH` guard. `list_actions_returns_all_known_types` extended to assert `"If"` is in the native registry.
- **What this doesn't add** (by design): no `ForEach`, no `While`. `ForEach` is blocked on cross-step output templating (pipelines can't yet reference `steps[0].output.lines` from step 1), and `While` is a footgun — `WatchAndReact` + the CLI `/until` mode already cover the legit loop-until-condition use cases without the infinite-loop risk. `If` is the 80% win at 20% of the risk; the other two primitives stay on the shelf until the cross-step dataflow story is designed.

### Fixed

#### Agent loop no longer panics when the model's response contains emoji

- **Symptom**: a successful session ending with the model writing something like `The artifact has been upgraded to a **Live Intelligence Dashboard**! 🚀` crashed the CLI with:
  ```
  thread 'main' panicked at x0ne-orchestrator/src/agent_loop.rs:643:46:
  byte index 70 is not a char boundary; it is inside '🚀' (bytes 67..71)
  ```
  The work was done — both the Pipelines executed, the `WriteFile` succeeded, the user had the URL — but the panic tore down the loop and `make run-podman` exited 101. No state was lost (the artifact was already served by `artifact_local_proxy`), but the operator was ejected mid-session.
- **Root cause**: `response_summary = format!("{}…", &raw_first[..cut])` at `agent_loop.rs:643`, where `cut = sentence_end.min(70)` is a *byte* count. UTF-8 encodes 🚀 as four bytes (F0 9F 9A 80); if `cut` lands on bytes 67, 68, 69 or 70 of the string, Rust's `&str[..N]` panics to enforce the char-boundary invariant. The same footgun existed in two neighbouring spots: `x0ne-cli/src/main.rs:1014` (benchmark prompt display) and `x0ne-orchestrator/src/pulse.rs:194` (recent-error log rendering) — both slice user / model strings at a hardcoded byte offset.
- **Fix**:
  - `agent_loop.rs:643` — after computing `cut`, walk backwards until `raw_first.is_char_boundary(cut)` returns true. Emoji in the first sentence is now rendered as `"That worked! "` or `"The artifact has been upgraded to a **Live Intelligence Dashboard**…"` depending on where the clamp lands — never a panic.
  - `x0ne-cli/src/main.rs` — introduced a `truncate_display(s, byte_budget)` helper that does the clamp + ellipsis dance once, and rewired the benchmark banner to use it. 5 unit tests pin the contract: under-budget identity, ASCII clip, emoji-boundary clamp (the actual 4-byte-rocket-at-offset-70 case the user hit), zero budget (`"…"`), and leading-multibyte (`"🦀🦀🦀"` with budget 1 → `"…"`).
  - `pulse.rs:194` — inlined the same clamp loop for the recent-errors renderer (log lines routinely include systemd status emojis + x0ne's own ✓/✗/→ glyphs).
- **Audit**: the only other direct `&str[..N]` slices in the tree (`result.rs:821` for MathBigResult digits, `sandbox.rs:68` / `incus.rs:72` for hex IDs, `math.rs:362` in a test asserting ASCII digits) all operate on strings that are structurally ASCII — verified and left untouched. Every other truncation site already uses `chars().take(n).collect::<String>()`, which is UTF-8-safe by construction.
- **Regression guard**: `truncate_display_emoji_boundary_does_not_panic` asserts that `&s[..70]` on a string where byte 70 is inside 🚀 no longer panics — this is the exact layout from the user-reported crash, encoded as a test.

#### `make run-podman*` — stdin now actually reaches the containerised x0ne-runtime

- **Symptom**: every `make run-podman` attempt died at startup with
  ```
  ✗ x0ne-runtime health check failed: x0ne-runtime health check failed — no response to ListActions
  Caused by: x0ne-runtime returned an empty response line
  ```
  Reset, rebuild, force-resync — all went clean. The container was up, the binary was fresh, `test-mock-podman` passed. But the interactive launch hung the bridge's initial `ListActions` probe.
- **Root cause**: `podman exec` does **not** attach the caller's stdin to the containerised process unless you pass `-i`. `incus exec` does — so the Incus flow worked from day one while the symmetric Podman flow silently starved the stdio bridge. x0ne-runtime started correctly inside the container, printed `Send JSON actions on stdin`, and then immediately hit EOF because the pipe wasn't wired through. The bridge waited ~2 s for the probe response, got nothing, and tore the session down.
- **Fix**: introduced `PODMAN_EXEC_FLAGS ?= -i` (overridable in `.env.dev`) and threaded it through all four `run-podman` / `run-podman-tui` / `run-podman-matrix` / `run-podman-dirty` targets. The expanded `X0NE_SANDBOX_CMD` is now `podman exec -i --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio`. Kept the flag separate from `PODMAN_RUN_ENV` (env vars) for clarity and so users can layer additional exec flags (`-u`, `-w`, …) without touching env plumbing.
- **Reproduction proof** (side-by-side on the same container):
  - Without `-i`: x0ne-runtime logs `Send JSON actions on stdin`, then… nothing. No `Received action` ever appears, the pipe is closed on the runtime side before the bridge even writes the probe.
  - With `-i`: `Received action ListActions` → `Action complete … success=true duration_ms=1`. Bridge moves past the health check and the session opens normally.
- **Regression test** `container_runtime::tests::detect_podman_with_standalone_i_flag` asserts that `podman exec -i --env K=V target /bin …` still resolves to `Backend::Podman` with `target`, so a future refactor of the flag parser can't silently regress the detection that the `bridge_tag` navbar label and `ArtifactExpose` both depend on. `container_runtime` tests: 18 pass (up from 17).

### Changed

#### CLI status bar — runtime-aware label replaces the generic 🛡 glyph

- **Before**: the inline navbar (`┊ gemma4:31b · 33.4ktk · 12msg · 🛡  ^E unfold · /help`) showed a static 🛡 shield whenever `[bridge] mode` was sandbox-ish, and nothing at all when running on the host. Operators running four terminals side by side (host / Incus / Podman / Docker) had no way to tell which sandbox they were typing into — only the big banner-once-at-startup line carried that info.
- **Now**: the shield is replaced by the actual runtime name:
  - `podman` / `incus` / `docker` — resolved by running `config.bridge.sandbox_cmd` through `container_runtime::detect_from_sandbox_cmd`, so the label mirrors exactly what the orchestrator will shell out to for `RunCommand` / `ArtifactExpose` / `pull_file`. Rendered in calm green `rgb(0,200,120)`.
  - `sandbox` — fallback when sandbox mode is active but the wrapper isn't a known container CLI (nsjail, bubblewrap, a custom script). Rendered in amber `rgb(200,160,0)` to flag "isolated, runtime unknown".
  - `host` — shown explicitly (previously blank) when `mode = local | stdio | ""`. Rendered in neutral grey `rgb(110,120,135)` so it doesn't compete visually with sandboxed sessions.
- **Why**: the 🛡 glyph was ambiguous (it fired for any mode containing `"sandbox"|"container"|"nsjail"`) and invisible on terminals that strip emoji. A text label is accessible, copy-paste-friendly, and — crucially — *informative*: the operator knows instantly whether the next command will run under Podman or Incus without `/status` or `/showconfig` detours.
- **Implementation**: `x0ne-cli/src/main.rs` (~30 LoC around the `bridge_tag` closure). Reuses `x0ne_orchestrator::container_runtime::detect_from_sandbox_cmd` (already exercised by 17 unit tests), so every `sandbox_cmd` shape the backend understands is reflected one-for-one in the status bar.

### Added

#### Makefile — full Podman sandbox workflow (symmetric to Incus)

- **Problem**: now that `artifact_expose.rs` / `artifact_local_proxy.rs` speak every `ContainerRuntime` backend, there was a one-command UX gap: `make run-incus[-tui|-matrix|-dirty]` existed, but the same flow for Podman required hand-rolling a container, an `apt-get` invocation, a `podman cp` for the binary, and a matching `X0NE_SANDBOX_CMD`. Users wanting to validate Podman compatibility had to reverse-engineer it from the source.
- **New targets** in the root `Makefile`:
  - `setup-podman` — creates `x0ne-sandbox-podman` from `debian:12` if missing, installs `python3 python3-pip curl git jq procps net-tools`, deploys the glibc-2.35 x0ne-runtime binary, and commits a reusable baseline image `localhost/x0ne-sandbox-baseline:latest`.
  - `sync-podman-musl` / `force-sync-podman-musl` — same mtime-based skip logic as `sync-incus-musl`; pushes the release binary via `podman cp` and invalidates the stale baseline when the binary changes.
  - `snapshot-podman` / `reset-podman` / `rebuild-baseline-podman` — mirror of the Incus snapshot/restore flow, but implemented via `podman commit` + `podman rm -f && podman run` (≈1–2 s to get back to a pristine state, vs ≪1 s for btrfs snapshots; acceptable trade-off for zero-config portability).
  - `run-podman` / `run-podman-tui` / `run-podman-matrix` — one-shot `build → setup → reset → launch x0ne` with `X0NE_SANDBOX_CMD="podman exec --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman /opt/x0ne-runtime --config /root/x0ne-runtime.toml --mode stdio"`.
  - `run-podman-dirty` — like `run-podman`, but keeps the previous state (no `reset-podman`), useful when debugging something that takes multiple turns to reproduce.
  - `diagnose-podman` — container state, binary presence, glibc version, `x0ne-runtime --version`, and baseline image summary. Symmetric to `diagnose-incus`.
  - `test-mock-podman` — pipes three canned JSON actions (`SystemInfo`, `ListDir /`, `RunCommand echo`) into the containerised x0ne-runtime over `podman exec -i`. Mirrors `test-mock-incus`.
- **New variables**: `PODMAN_CONTAINER` (default `x0ne-sandbox-podman`), `PODMAN_IMAGE_BASE` (default `debian:12`), `PODMAN_BASELINE_IMG` (default `localhost/x0ne-sandbox-baseline:latest`), `PODMAN_RUN_ENV` (default `--env X0NE_ALLOW_LOOPBACK=1`), all overridable in `.env.dev`. Binary path is fixed at `/opt/x0ne-runtime` for parity with the Incus target.
- **Zero changes to the orchestrator**: `ContainerRuntime::detect_from_sandbox_cmd` already returned `Backend::Podman` when the first token of `X0NE_SANDBOX_CMD` is `podman`, so `run-podman*` reuses the exact same artifact exposure, file pull, and action-dispatch code paths as `run-incus*`. The only practical difference user-side is the CLI the orchestrator shells out to for `ArtifactExpose`.
- **End-to-end verification** on a clean machine:
  - `make setup-podman` → container created, `apt-get` pass, 14 MB binary pushed, `localhost/x0ne-sandbox-baseline:latest` committed (725 MB). ~65 s cold.
  - `make test-mock-podman` → `ListDir /` returns 19 entries with `"success": true`.
  - `make diagnose-podman` → binary `14M`, glibc `2.36`, `x0ne-runtime 0.2.0`, baseline image listed.
- **Docs**: this entry is the discoverability anchor. `make help` lists all nine new targets inline with their `##` descriptions.

#### Artifact — container runtime abstraction (Incus / Podman / Docker)

- **Problem**: the first cut of the artifact loopback proxy called `incus exec` and `incus file pull` directly from two hot paths (`artifact_expose.rs`, `artifact_local_proxy.rs`). Users running their x0ne-runtime sandbox on Podman or Docker therefore got a clean `"ArtifactExpose requires sandbox mode"` error even though their bridge was perfectly configured — the word `"incus"` was just hard-coded as the first token of `parse_target_from_sandbox_cmd`. Making artifacts a container-runtime-agnostic primitive was a prerequisite for tenant flexibility, CI pipelines running under Docker, and local dev loops on Podman-Machine (macOS).
- **New module** `x0ne-orchestrator/src/container_runtime.rs` (~400 LoC):
  - `enum Backend { Incus, Podman, Docker }` — lower-cased in JSON so action results ship `"backend": "podman"`.
  - `trait ContainerRuntime { backend, exec(target, argv), pull_file(target, abs_path) }` — the minimal surface the artifact subsystem needs. Shared spawn-and-collect helpers (`run_capture`, `run_pull`) avoid duplication between backends.
  - `IncusRuntime` uses `incus exec <target> -- <argv>` and the native `incus file pull <target><path> -` streamed command.
  - `PodmanRuntime` / `DockerRuntime` use `<bin> exec <target> <argv>` and `<bin> exec <target> sh -c 'cat -- <path>'` — on purpose, `cp container:path -` is avoided because it emits a tar archive on stdout instead of raw bytes.
  - `detect_from_sandbox_cmd` inspects the configured `[bridge] sandbox_cmd` and returns `(Arc<dyn ContainerRuntime>, target)` if the first bare token is `incus|podman|docker`. It strips leading `KEY=VAL` env assignments, honours the `--` end-of-options separator, and knows the short list of flags that take a value (`--env/-e`, `--user/-u`, `--workdir/-w`, `--volume/-v`) so boolean flags (`-it`, `--rm`, `--privileged`) are skipped without consuming the next token.
  - `shell_single_quote` — POSIX-safe single-quoting (handles embedded single quotes via the `'\''` trick) so artifact-supplied absolute paths cannot inject commands via the `sh -c` wrapper.
- **`ArtifactEntry` gains** `backend: Option<Backend>` alongside `tenant: Option<String>`. When `tenant` is `Some(_)` the proxy dispatches `pull_file` through `container_runtime::for_backend(backend).pull_file(...)`; when both are `None` we fall back to the host filesystem (unit tests and future host-mode artifacts). A specific registration error is now returned when `tenant` is set but `backend` is missing, instead of silently defaulting.
- **`ArtifactExpose` action result** gains a `backend` field (`"incus"` / `"podman"` / `"docker"`) and its `transport` string is rewritten to `"loopback (x0ne-orchestrator → <backend> exec ... cat)"`, so operators eyeballing session logs can confirm which runtime served a given artifact.
- **Prompt updates** (`x0ne-orchestrator/src/prompt.rs`): the `ArtifactExpose` description now says "auto-detected from bridge.sandbox_cmd" instead of pointing at Incus specifically, in both the compact and full prompt flavours. The LLM is unchanged in every other way; no prompt-budget regression.
- **Tests** (`cargo test -p x0ne-orchestrator --lib container_runtime` → 17 pass, 0 fail; combined artifact suite → 20 pass):
  - `backend_bin_matches_expected_strings`, `backend_parse_roundtrip`, `backend_json_roundtrip` — enum discipline.
  - `detect_plain_incus`, `detect_incus_with_env_flags`, `detect_plain_podman`, `detect_podman_with_boolean_and_value_flags`, `detect_podman_with_equals_env`, `detect_plain_docker`, `detect_docker_with_leading_env_assignment` — parser correctness across realistic sandbox_cmd shapes.
  - `detect_rejects_non_container_cli`, `detect_rejects_non_exec_subcommand`, `detect_gives_up_when_only_flags_follow_exec` — hard-rejects for `ssh`/`kubectl`, empty input, `incus shell`, and malformed inputs.
  - `single_quote_wraps_plain_path`, `single_quote_escapes_embedded_quote`, `single_quote_handles_empty_string` — shell-escape invariants.
  - `for_backend_returns_matching_backend` — dispatch table integrity.
  - Existing `artifact_expose::tests::*` were renamed from `tenant_parses_*` to `detect_parses_*` and gained new variants for Podman and Docker sandbox_cmds.
- **Live smoke harness** (`x0ne-orchestrator/examples/artifact_smoke.rs`) now takes an explicit `backend` argument, so you can validate Podman or Docker end-to-end with `cargo run --release --example artifact_smoke -- podman mycontainer cv_xxx /root/.x0ne/artifacts/…/cv_xxx`.

#### Artifact — top-level dispatch guard now routes `ArtifactExpose` / `ArtifactUnexpose` correctly

- **Bug**: the top-level dispatch guard in `run_agent_loop` (`agent_loop.rs`) listed only the seven multi-agent sysaicalls (`SpawnAgent`, `WaitAgent`, `ListAgents`, `BroadcastAgents`, `GetBroadcasts`, `AcknowledgeBroadcast`, `GetBroadcastReceipts`). `ArtifactExpose` / `ArtifactUnexpose` lived in a match arm *inside* that guard, so they were unreachable and the LLM's valid `ArtifactExpose` action fell through to the x0ne-runtime bridge — which rejected it with `"unknown variant \`ArtifactExpose\`"` in ~14 ms. The local proxy therefore never saw a registration, and the browser got `"artifact 'cv_...' not registered (or expired)"`.
- **Fix**: the guard now also matches `ArtifactExpose`/`ArtifactUnexpose`, and these two actions take a dedicated fast-path *before* the `if let Some(ref reg) = registry` branch (artifacts don't need the multi-agent registry and must work at top level too).
- **Regression tests** in `agent_loop::tests`:
  - `artifact_actions_are_orchestrator_actions` — sanity-checks `ORCHESTRATOR_ACTION_TYPES`.
  - `top_level_guard_covers_all_multi_agent_and_artifact_actions` — mirrors the guard and asserts it accepts every action type that the inner match / artifact fast-path knows how to execute; fails loudly if a future orchestrator action is added without extending the guard.

### Changed

#### Artifact exposure — loopback reverse-proxy embedded in `x0ne-orchestrator` replaces the per-container Incus proxy device

- **Problem**: the first cut of `ArtifactExpose` (previous section) published artifacts by adding an `incus config device add … proxy listen=tcp:0.0.0.0:7800 connect=tcp:127.0.0.1:7800 bind=host` to the tenant container, shipping an `x0ne-artifact-proxy` binary into `/opt/`, and starting it via `setsid`/`systemd`. Three things broke this in practice: (1) `make run-incus` restores the sandbox from a baseline snapshot on each launch, which wipes every side-effect we'd pushed (binary, systemd unit, proxy device); (2) the LLM would emit `ArtifactCreate`+`WriteFile` and then hand the user the naive `http://localhost:7800/a/<id>/` URL that `ArtifactCreate` returns — which is in-container-only and refuses-to-connect from the browser; (3) the model needed to know about the host IP of the Incus remote to build a useful URL, which required us to publicize tenant-host IPs and leak LAN topology into the agent prompt.
- **New architecture**: `x0ne-orchestrator` now embeds a loopback-only reverse-proxy (`artifact_local_proxy`) that listens on `127.0.0.1:7800` on the user's machine. `ArtifactExpose` is a 3-line handler that just registers the artifact in the proxy's in-memory registry. Requests come in from the browser, the proxy resolves `/a/<id>/<path>` against the registry, and streams bytes out of the tenant via `incus file pull` on demand — no files are copied to the host, no port is opened on the network, no Incus device is created, no binary is shipped into the container, no systemd unit is installed anywhere. Ctrl-C on `x0ne` = 100% of exposure gone.
- **Why this is safer**: loopback-only bind means adjacent machines on the LAN cannot scan the port, unlike the previous `0.0.0.0` on the Incus host. Multi-tenant becomes trivial — each user runs their own `x0ne`, each `x0ne` has its own `127.0.0.1:7800`, no port collision on the remote. The LLM's naive `http://localhost:7800/a/<id>/` URL is now *correct* — we changed the world to match what the model was saying rather than fighting the model.
- **Data path (static artifact)**:
  ```
  browser ─► 127.0.0.1:7800 ─► x0ne (local) ─► incus file pull ─► tenant container
            (loopback only)    (this process)  (authenticated)
  ```
- **New file** `x0ne-orchestrator/src/artifact_local_proxy.rs` (~460 LoC): axum router on `127.0.0.1:7800`, `ArtifactRegistry` (async `RwLock<HashMap>` with lazy TTL eviction on access), `ArtifactEntry { artifact_id, tenant, root_path, kind: Static|Service { port }, expires_at }`, `ensure_started()` idempotent singleton using `OnceLock`, `pull_via_incus` streaming transport, MIME-type table, path-traversal guard (`..` segments rejected), `cache-control: no-store`, `/healthz` JSON endpoint. Entry-point `url_for(artifact_id)` returns the externally-consumable URL.
- **Rewritten** `x0ne-orchestrator/src/artifact_expose.rs`: the ~500-line module that used to wrap `incus config device add`/`systemctl start`/`setsid -f`/IP resolution is now ~200 lines of "parse → lookup artifact in tenant registry.json → register in local proxy". `ArtifactUnexpose` now supports `{artifact_id?}` — omit to revoke every artifact in the session at once. `visibility` is reduced to `host` only (loopback); `lan`/`public` return a clean error message pending the edge-nginx iteration. `gateway_port` is removed (it was a leaky abstraction).
- **Gated on the `serve` feature** (default-on) to keep `--no-default-features` builds stripped of axum. When the feature is off, `ArtifactExpose` returns a helpful error explaining how to rebuild.
- **Prompt updates** (`x0ne-orchestrator/src/prompt.rs`): `PRIMITIVE` registry and `ORCHESTRATOR_ACTIONS` metadata + schemas rewritten to document the loopback URL; explicit rule "for a browser link, chain `ArtifactCreate → WriteFile → ArtifactExpose`" in both the compact and full prompts; `visibility` reduced to `enum: ["host"]`; `gateway_port` dropped from the schema.
- **Deleted (implicitly)**: `/opt/x0ne-artifact-proxy` binary push, the `x0ne-artifact-gateway` Incus proxy device, the systemd unit (`/tmp/x0ne-artifact-proxy.service`), `setsid -f` gymnastics, `incus config device add/remove` wrappers, `remote_host_addr()` IP resolver. The `x0ne-artifact-proxy` binary itself stays in the workspace for local (non-sandbox) use, but it's no longer required for sandbox mode.
- **Tests** (`cargo test -p x0ne-orchestrator --lib artifact_` → 10 pass, 0 fail):
  - `artifact_local_proxy::tests::guesses_content_types` — extension → MIME mapping covers html/css/js/json/svg/png/jpg/webp/gif/ico/woff/woff2/ttf/txt/md/wasm + octet-stream fallback.
  - `artifact_local_proxy::tests::registry_register_get_expire` — lazy TTL eviction works, an entry whose `expires_at` is in the past is transparently evicted on first `get()`.
  - `artifact_local_proxy::tests::serves_static_from_host_filesystem` — full axum router smoke test: 200 on nested file with correct MIME, 404 on missing artifact / missing file, 400 on path traversal (`..%2Fetc%2Fpasswd`).
  - `artifact_expose::tests::tenant_parses_*` (3 cases) — sandbox_cmd parser still extracts `prod-infra:x0ne-sandbox` with/without `--env` flags, returns `None` for non-Incus bridges.
  - `artifact_expose::tests::expose_rejects_non_host_visibility` — `lan`/`public` rejected with a clear message.
  - `artifact_expose::tests::expose_rejects_missing_artifact_id` — shape validation.
  - `artifact_expose::tests::expose_errors_when_not_sandbox` — stdio/http bridges get a specific error, not a generic crash.
  - `artifact_expose::tests::describe_policy_includes_ttl` — telemetry helper.
- **Live smoke test**: `cargo run --release --example artifact_smoke prod-infra:x0ne-sandbox cv_xxx /root/.x0ne/artifacts/…/cv_xxx` starts the proxy, and `curl -D- http://127.0.0.1:7800/a/cv_xxx/` returns `200 OK`, `content-type: text/html; charset=utf-8`, `content-length: 7316`, `cache-control: no-store`, the body is the HTML written by the agent; `/a/cv_xxx/nope.js` → 404; `/a/cv_fake/` → 404; `/a/cv_xxx/..%2Fetc%2Fpasswd` → 400. Latency ≈ 240ms including `incus file pull` spawn.
- **New example**: `x0ne-orchestrator/examples/artifact_smoke.rs` (~50 LoC) — live harness that registers a single artifact and blocks on Ctrl-C, for manual end-to-end testing without spawning a full LLM loop.

### Added

#### X0NE Artifact — agent-generated web artifacts served via a unified proxy

- **Problem**: agents have always been able to *write* HTML/CSS/JS files into a sandbox, but there was no way to *open them in a browser*. Either the user ran their own `python3 -m http.server` and remembered the path, or the file lived inside an Incus container with no port-forwarding. Every "show me a quick mockup" turned into a manual hosting chore, and ephemeral sandboxes silently destroyed any progress when they were torn down.
- **New surface**: a *artifact* is a self-contained directory served at `http://localhost:7800/a/<id>/` by a new long-running daemon, `x0ne-artifact-proxy`. Six new typed actions cover the full lifecycle:
  - `ArtifactCreate {name?, kind: "static"|"service", persistent?}` — allocate a directory under `~/.x0ne/artifacts/{ephemeral/<session>,saved}/<id>/`, register it, and auto-spawn the proxy on first use. Returns `{id, root_path, url, …}`.
  - `ArtifactList` — every artifact (kind, state, age, size, URL) plus `proxy_running`.
  - `ArtifactServe {id, command, port?, env?}` — for `kind=service` artifacts: spawn `sh -c <command>` under `setsid` in the artifact root, with `$PORT` substituted to an auto-allocated port in `[7810,7899]`. The proxy reverse-proxies `/a/<id>/*` to that port.
  - `ArtifactSave {id, name?}` — atomically move an ephemeral artifact to the persistent `saved/` store.
  - `ArtifactStop {id}` — SIGTERM the *process group* (so `sh -c "python3 …"` plus its python child both die), SIGKILL after a grace.
  - `ArtifactDestroy {id, force?}` — stop, delete files, drop from the registry. `force` required for persistent artifacts.
- **Proxy daemon** (`x0ne-artifact-proxy`, axum on `127.0.0.1:7800`):
  - `GET /` — HTML index of every artifact.
  - `GET /_health` — for the runtime to probe before re-spawning.
  - `GET /_api/artifacts` — JSON listing.
  - `GET /a/<id>/…` — static-file serving (with mime-sniffing and `..` rejection) for `kind=static`, full reverse proxy (preserves headers/methods/body, strips hop-by-hop) for `kind=service`. Returns `503` if the registered service PID is no longer alive.
  - Idempotent boot: refuses to start a second instance on the same port (`bind` fails → exits with `0`) so the runtime can fire-and-forget.
- **CLI integration** (`/artifact` slash command in `x0ne-cli`):
  - `/artifact` — colored table: `id  kind  state  where  age  size  url`. State is colored (`running` green, `failed` red, `stopped` yellow).
  - `/artifact open <id>` — `xdg-open` the URL.
  - `/artifact save <id> [name]`, `/artifact stop <id>`, `/artifact rm <id> [-f]` — the obvious things.
  - Help (`/help`) lists the command group under "⫸ Artifact (web artifacts)".
  - `ArtifactList` (and thus `/artifact`) now calls the same `ensure_proxy_running` path as `ArtifactCreate`, so the first listing attempt can bring `x0ne-artifact-proxy` up instead of always showing `○ offline` until an agent emits `ArtifactCreate`.
  - **Env tuning**: `X0NE_ARTIFACT_PUBLIC_HOST` (default `localhost`) controls the hostname embedded in returned `url` fields; `X0NE_ARTIFACT_PROXY_CHECK_HOST` (default `127.0.0.1`) controls where the runtime probes `/_health` (set to the host/bridge IP when the executor runs inside Incus but the proxy listens on the host); `X0NE_ARTIFACT_PROXY_SKIP_SPAWN=1` skips auto-spawn when the operator starts the proxy manually.
  - **Orchestrator prompts** (lazy + compact + static fallbacks): explicit rule — for a browser link, `ArtifactCreate` then `WriteFile` under the returned `root_path`, then echo the `url` field; do not claim hosting is impossible after a successful artifact create.
  - **`ACTION_REGISTRY` in `meta.rs`**: the six `Artifact*` primitives are now registered so `ListActions` / `DescribeAction` match the real executor (fixes `Unknown action type 'ArtifactCreate'` even on an up-to-date binary).
  - **Policy defaults**: `ArtifactList` enabled like other reads; `ArtifactCreate` / `ArtifactServe` / `ArtifactSave` / `ArtifactStop` / `ArtifactDestroy` gated like other writes. Sandbox TOML updated accordingly.
- **Storage layout** (under `$X0NE_HOME/artifacts/` or `~/.x0ne/artifacts/`):
  ```
  registry.json              # source of truth
  registry.lock              # advisory lock for concurrent runtime/proxy access
  ephemeral/<session>/<id>/  # session-scoped, auto-purged
  saved/<id>/                # persistent, kept across restarts
  ```
  The registry uses atomic `rename`-based writes and a tiny spinlock for serializing the runtime (writer) and the proxy (reader).
- **Files**:
  - `x0ne-runtime/src/artifact.rs` — types (`Artifact`, `ArtifactKind`, `ArtifactState`, `ServiceSpec`, `ArtifactRegistry`), storage layout, port allocation (`SERVICE_PORT_MIN..=SERVICE_PORT_MAX = 7810..=7899`), `RegistryLock`, `terminate_pid` (process-group aware).
  - `x0ne-runtime/src/executor/artifact.rs` — six action handlers, `ensure_proxy_running` (auto-spawns the daemon next to the runtime binary or via `$PATH`).
  - `x0ne-runtime/src/bin/artifact_proxy.rs` — the axum daemon binary (`x0ne-artifact-proxy`).
  - `x0ne-orchestrator/src/slash.rs` — `/artifact` dispatcher + `print_artifact_list`.
- **Smoke test passed end-to-end**: `ArtifactCreate` (static) → write `index.html` → `curl http://localhost:7800/a/<id>/` returns 200 with correct mime; `ArtifactCreate` (service) → `ArtifactServe "python3 -m http.server $PORT"` → reverse-proxied request returns 200; `ArtifactSave` migrates ephemeral → `saved/`; `ArtifactStop` reaps both `sh` and child `python` (process-group kill); `ArtifactDestroy` cleans up.
- **Architectural decisions encoded** (per the user's `go a b b` choice):
  - Q1=a → Rust+axum (no nginx, no Caddy, no extra moving parts).
  - Q2=b → daemon-managed: services survive the X0NE session that spawned them; `/artifact stop` is the only kill switch.
  - Q3=b → bind-mount the host `~/.x0ne/artifacts/` into the existing `user-1000/x0ne-sandbox` so agents writing into the artifact via `WriteFile` from inside the sandbox land on the same path the proxy serves from. (Service-artifact processes currently still run on the host; moving them inside the sandbox via `incus exec` + a per-artifact `proxy device` is the next step.)

### Fixed

#### LLM-spawned sub-agents now actually appear in `/worlds` and the TUI Worlds pane

- **Bug**: when the LLM emitted a `SpawnAgent` action (directly or as a Pipeline step), the sub-agents booted, ran, and produced output cards correctly — but `/worlds` afterwards reported `no parallel worlds have been spawned yet`, and the auto-toggled TUI Worlds pane stayed empty. The on-screen evidence (5 `hw_cpp` / `hw_js` / … completion frames) didn't match what the worlds UI showed.
- **Root cause**: there are two spawn code paths in the orchestrator. `subagent::spawn_subagent` (used by the `/spawn N` demo) correctly calls `worlds_registry::global().register()` + emits `WorldEvent`s. But `sysaicalls::execute_spawn_agent` (used when the LLM commits to a `SpawnAgent` action) was a separate `tokio::spawn` flow that only updated the `AgentRegistry` (used by `WaitAgent`) and never touched the worlds registry. The two registries are independent: `AgentRegistry` powers the action protocol (handles, `WaitAgent`, broadcasts), `worlds_registry` powers the visual parallel-worlds UI. Only the first was being populated.
- **Fix**: `execute_spawn_agent` now mirrors every lifecycle event into `worlds_registry::global()`:
  - On entry: `register(&id)` + `ActionStarted("SpawnAgent")` so the world appears in the pane immediately, even before the inner agent loop has booted up its bridge.
  - On bridge-creation failure: `WorldFailed` so the world doesn't stay stuck on "running" forever.
  - On normal completion: `ActionFinished("SpawnAgent", duration_ms)` + `WorldCompleted` with a summary (`{tk} tk · {chars} chars · {actions} actions`). The summary mirrors what `subagent::spawn_subagent` already produces, plus the action count from the inner sub_stats.
- This restores the contract that the auto-toggle TUI feature (added earlier this iteration) was implicitly relying on — the pane was opening but had nothing to show.

### Added

#### Per-agent context-switch dividers in CLI sub-agent output

- **Problem**: with N parallel `SpawnAgent`s running, every sub-agent calls `println!(agent_status_line)` whenever it gets a result. Lines from different agents arrive interleaved on the parent's stdout — the operator sees `⊕ agent_00 …` then `⊕ agent_06 …` then `⊕ agent_05 …` then `⊕ agent_00 …` with no visual cue that any of these belong to the *same* "world". The on-screen feel was a flat stream, not "8 isolated worlds each doing their thing".
- **Fix**: new `display::emit_agent_status_line(agent_id, action, result, duration_ms)` wraps the existing `agent_status_line` formatter with a process-global tracker (`LAST_AGENT_EMITTER: Mutex<Option<String>>`). When the next emission has a *different* `agent_id` than the previous one, a one-line dim divider header is injected first:
  ```
  ── agent_00 ──────────────────────
    ⊕ agent_00: ✓ read · ReadFile → 86B (10ms)
    ⊕ agent_00: ✓ write · WriteFile → 131B (12ms)
  ── agent_06 ──────────────────────
    ⊕ agent_06: ✓ read · ReadFile → 113B (11ms)
  ```
  Consecutive lines from the same agent stay grouped (no extra divider noise); a context switch is visually obvious.
- Each emission is prefixed with `\x1b[2K\r` (CSI erase-line + carriage-return) so the parent's `indicatif` spinner — which redraws on the bottom row using `\r` — can't leave half-erased content that would smash the new line into the spinner's column 80+. This also fixes the secondary "weird long run of spaces with the agent line at column 80" effect the operator was seeing in the previous iteration.
- New companion `display::reset_agent_emitter()` that clears the tracker. The parent agent loop calls it whenever IT prints something non-agent (its own action card, a doing-spinner result, …) so the next sub-agent emission gets a fresh divider rather than being wrongly grouped under the parent's most recent line.
- All 4 call sites in `agent_loop.rs` (`OrchestratorAction` quiet branch, `ListActions`/`DescribeAction` quiet branches, post-bridge result quiet branch) migrated from `println!(disp::agent_status_line(…))` to the new emit helper.
- **Tests**: 2 new unit tests in `display::tests` (`reset_makes_first_emit_print_divider`, `emitter_tracker_distinguishes_same_vs_different`) pin the state-machine contract — verified at the tracker level rather than via stdout capture so they're reliable across platforms.

#### Auto-open the parallel-worlds pane the moment the LLM commits to a `SpawnAgent`

- New `agent_loop::action_will_spawn_world(&Value) -> bool` predicate — returns `true` for any direct `SpawnAgent`, or any `Pipeline` whose `steps[]` contains one. The TUI dispatch path consults it right after `action_type` is extracted (just before `log_dispatch`), and if the worlds pane is currently hidden, calls `AppState::toggle_worlds_pane()` to bring it up.
- This fixes the "by the time I hit F10 the fast worlds are already done" problem: every sub-agent emits live `WorldEvent`s into the global `worlds_registry` from the moment it starts, so the pane is now populated from the very first event instead of catching only the tail end.
- Idempotent — already-open pane is left alone, so the operator can manually toggle it off mid-flight without it bouncing back. A one-line console entry (`⫸ worlds pane auto-opened — LLM is spawning sub-agents`) makes the auto-toggle visible in the action log.
- Discoverability: the existing F10 chip in the F-key bar already highlights cyan whenever `worlds_total > 0`, so even after manually closing the pane the operator can see worlds are live.
- **Tests**: 3 new unit tests in `agent_loop::tests` pin the positive cases (direct + pipeline-nested) and the negative cases (`ReadFile`, `RunCommand`, no-spawn pipeline, malformed pipeline, `WaitAgent`).

#### `Ctrl+E` is now a 3-state CYCLE through views of the session (CLI + TUI)

- Pressing `Ctrl+E` no longer toggles "unfold ↔ blank" — it now cycles through three views of the *same* conversation, so the operator never loses sight of the session content:
  - **Normal**   → bottom-of-pane is the prompt / a transient action result. Ctrl+E here goes to **Unfolded**.
  - **Unfolded** → full unfolded session (every action JSON, every result body verbatim — the output of `/expand`). Ctrl+E here goes to **Compact**.
  - **Compact**  → compact session view (one line per turn, summarized actions and results — the output of `/history`). Ctrl+E here goes back to **Unfolded**, so the operator can flip-flop between both representations of the exact same conversation as many times as they want.
- The previous "press again → blank pane / cleared screen" behaviour was confusing: it erased the only on-screen view of the session right when the operator wanted to compare it with something. The 3-state cycle keeps a session view on screen at all times once Ctrl+E has been pressed once, and ANY non-Ctrl+E input still resets to Normal so a fresh press always starts at Unfolded.
- **CLI** (`x0ne`): cycle state lives in `cli_helper::EXPAND_STATE: AtomicU8` with a typed `ExpandState` enum and helpers `expand_state()` (peek), `set_expand_state(state)`, `clear_expand_active()` (reset to Normal). The CLI loop matches on the current state inside the Ctrl+E branch and either prints the corresponding view in place (clearing the screen first so it starts at the top — clearing exactly N lines is unreliable across terminal widths) or routes through `/x` so the existing slash dispatch produces the unfold. `/x` / `/expand` / `/unfold` typed as slash commands set state to Unfolded; `/history` typed as a slash command sets state to Compact, so a typed view command and a Ctrl+E press are interchangeable.
- **TUI** (`x0ne --tui`): replaces `data_view_unfolded: bool` with `data_view_mode: DataViewMode { Normal, Unfolded, Compact }` on `AppState`. The Ctrl+E handler in `tui_mode.rs` matches on the current mode and either calls `handle_tui_enter("/expand")` (Normal → Unfolded, Compact → Unfolded) or directly writes the compact view via the new `terminal::format_history_compact_plain` (Unfolded → Compact). `set_data_force` and `set_result` reset the mode to Normal so the cycle never accidentally cycles a transient action result.
- Navbar hints rotate with the state so the operator always knows what the NEXT press will produce — never has to remember the cycle direction:
  - **CLI**: hint label flips between `^E unfold · /help` (Normal / Compact) and `^E compact · /help` (Unfolded).
  - **TUI**: F-key chip flips between ` Unfold ` and ` Compact ` (same chip slot, dim styling so it doesn't fight for attention).
- New `terminal::format_history_compact_plain(&Conversation) -> String` — plain-text mirror of `print_history` (no ANSI escapes), used by the TUI Compact view. Same one-line-per-turn summary, same per-step Pipeline / Try breakdown, but suitable for direct rendering in the ratatui data pane (which applies its own styling via `colorize_json_line`).
- Cycle state machine pinned by 2 new unit tests in `cli_helper::tests`: `expand_state_cycle` (Normal → Unfolded → Compact → Unfolded round-trip + `clear_expand_active` resets from any state) and `expand_state_peek_does_not_mutate` (the navbar polls every redraw and must not race the keystroke handler).

#### Single-keystroke `Ctrl+E` for unfold in the CLI (parity with TUI)

- Pressing `Ctrl+E` at the rustyline prompt now unfolds the entire session in one keystroke, no `/x` + Enter required. Mirrors the TUI's existing `Ctrl+E` binding so the keyboard story is consistent across both surfaces.
- Implementation note: rustyline 14 binds one `Cmd` per event and `EventHandler::Macro` is still a TODO upstream, so a single binding cannot atomically "replace buffer with /x AND submit". Worked around with a process-wide `AtomicBool`: a `ConditionalEventHandler` flips the flag and returns `Cmd::AcceptLine`; the readline-loop in `x0ne-cli` checks `cli_helper::take_expand_requested()` immediately after each `readline()` return and substitutes the result with `/x` when set. Whatever was in the buffer at the moment of Ctrl+E is discarded — by design, since Ctrl+E is meant as a between-prompts peek.
- The default emacs `Ctrl+E` binding (move-cursor-to-end-of-line) is overridden; `End` and `Ctrl+A`/`Ctrl+E`-twin shortcuts still cover that need.
- The substituted `/x` is NOT added to readline history (it's a transient peek, not a message worth recalling with ↑).
- A small `⫸ unfolding session…` line is printed before the `/x` runs so the operator gets immediate visual confirmation the keystroke registered.

#### `^E Unfold` chip in the TUI F-key bar + `^E unfold · /help` hint in the CLI navbar

- The `Ctrl+E` shortcut was silent — invisible by design but undiscoverable in practice. The TUI F-key bar now carries a dim `^E Unfold` chip between `Tab Console` and `^C stop` (same passive styling as the other modifier-key chips so it doesn't fight for attention), and the CLI navbar appends an `^E unfold · /help` hint to the right of the auto-confirm flag.
- The CLI hint is suppressed on the very first prompt of a session (`msg_count == 0`) — there's nothing to unfold yet, and a clean welcome screen reads better than one cluttered with hints for features the user hasn't earned the need for.

#### Live token gauge during LLM streaming (CLI spinner + TUI navbar)

- New `api::LiveTokenCounter` — Arc-shared, atomic, lock-free per-call counter built specifically for the streaming hot loop. Estimates input tokens up-front from `(sys_prompt + messages).chars() / 4` (same heuristic as `Conversation::estimated_tokens`, so the in-flight gauge agrees with the persistent context-tracker once the call resolves) and bumps output by at least 1 per chunk so byte-by-byte servers still move the gauge.
- **CLI** (`x0ne` / `x0ne --until …`): the existing "doing" word (thinking / cooking / brewing / …) now stays visible **alongside** the live gauge for the entire round-trip — both signals coexist as `⠋ thinking · in 1234 · out 89 · 23 tk/s · 0.5s` instead of the gauge silently replacing "doing" once chunks start arriving. The gauge is rendered from the very first frame (so `in {N}tk` is shown before any output token lands, then `out` and `tk/s` fill in as chunks flow). Sysmon `status_line()` is hidden as soon as `out > 0` to avoid line-width competition. The pre-existing dimmed inline-text echo has been dropped — indicatif's spinner redraw and `eprint!` to stderr cannot share a line cleanly, and the live gauge is now the "model is alive" indicator. Action-block detection is unchanged: as soon as `<action>` appears in the buffer the spinner is finished and the action card takes over.
- **TUI** (`x0ne --tui`): `AppState.live_tokens: Option<LiveTokenCounter>` — set when a `stream()` call starts, cleared when it ends. The navbar's `build_title` reads the gauge and renders the same `in N · out N · X tk/s · Ys` segment in cyan while the call is in flight, falling back to the input-token-size hint before the first output token arrives.
- Output is plain ASCII (`format_status` asserts no ANSI escape leakage) — colorization happens at each rendering site (CLI uses `colored::Colorize`, TUI uses ratatui `Style`).
- **Tests**: 4 new unit tests in `api::tests` cover the empty start state, the at-least-one-per-chunk floor, the `chars/4` heuristic agreement with `Conversation::estimated_tokens`, and the ANSI-clean `format_status` output.

#### `/expand` slash command (alias `/x`, TUI Ctrl+E) — unfold the entire session

- New operator escape-hatch that re-prints the **entire conversation** with **no truncation**: full action JSON, full result body (full file contents, full `stdout`/`stderr`, full pipeline step breakdowns, full system feedback messages). The data source is `Conversation::messages()` — i.e. the same memory the LLM sees — so what's printed is ground-truth context, not a re-rendered view.
- Replaces the `/history` blind-spot: `/history` keeps its compact, scannable summary; `/expand` is the "show me what really happened" hatch when the inline collapsed renderer hid something useful.
- **CLI** (`x0ne`): `/expand`, `/x`, `/unfold` — colored output with `colored::Colorize`, JSON pretty-printed, footer with per-source action stats and `Type×N` tally. Tab-completion via `cli_helper::COMMANDS`.
- **TUI** (`x0ne --tui`): `Ctrl+E` keybind dumps the unfolded session into the smart-data pane (PgUp/PgDn to scroll). Uses an ANSI-free formatter (`format_full_history_plain`) so escape codes don't leak into ratatui rendering.
- **Tests**: 2 unit tests in `terminal::expand_tests` cover the empty case and a >2 KiB ReadFile result to ensure no truncation is ever re-introduced and the TUI variant stays ANSI-clean.

### Changed

#### Workspace split — `x0ne-cli` extracted from `x0ne-orchestrator`

The repository is now a 3-crate Cargo workspace (was 2):

| Crate | Role | Binary |
|-------|------|--------|
| `x0ne-runtime` | Executor + policy + MCP/HTTP server (zero AI) | `x0ne-runtime`, `x0ne-scenario-runner` |
| `x0ne-orchestrator` | LLM loop, parser, conversation, sysaicalls, TUI/Matrix engines (library) | `x0ne-orchestrator` (headless HTTP/SSE) |
| `x0ne-cli` | Interactive entry point — wraps the orchestrator library | `x0ne` |

`x0ne-orchestrator` is now a pure library that exposes `agent_loop`, `tui_mode`, `matrix_mode`, etc. as public modules. `x0ne-cli` wires those into the actual `x0ne` binary you ship to users. This unblocks reusing the orchestrator library from external tooling (`x0ne-orchestrator` headless server, future Tauri/web frontends, embedded tests) without dragging in `rustyline` / `crossterm` from the CLI surface.

The headless HTTP/SSE binary previously named `x0ne-server` has been renamed to `x0ne-orchestrator` so the binary name matches the crate. The Makefile keeps `make run-server` as a backwards-compat alias of `make run-orchestrator`.

The previous `x0ne-core/` directory has been renamed to `x0ne-runtime/` everywhere — including binaries (`x0ne-core` → `x0ne-runtime`), config files (`x0ne-core.toml` → `x0ne-runtime.toml`, sandbox + incus-only variants), Makefile targets, install scripts, and CI release workflow. The `X0NE_CORE_BIN` env var is still honoured for backward compatibility with existing scripts.

### Added

#### Tier 1 — Agentic Primitives (Interactive, Discovery, Reactive)

Three new primitives that transform X0NE from a batch-action executor into an interactive, autonomous agent platform:

- **`SendInput` action** — write to stdin of a running streaming process (`RunCommand` with `stream: true`). Enables interactive CLI sessions, REPL-driven workflows, and game-playing via the agent loop. Always-allowed (requires an existing handle, no side-channel risk). Includes flush-after-write for immediate delivery.
- **`BrowseUrl` action** — fetch and extract readable text from any public URL. Cascading headless browser engine chain: `chromium` → `chrome` → `lynx` → `curl`. HTML-to-text stripping (scripts/styles removed), `<title>` extraction, 50KB output cap. SSRF-hardened: blocks loopback, private networks (RFC 1918), link-local, and cloud metadata endpoints (169.254.169.254, metadata.google.internal). Disabled by default (network action).
- **`WatchAndReact` action** — watch a file or directory for changes and auto-execute an action on each trigger. Returns a handle immediately (poll with `CheckHandle`). Features: configurable debounce (default 500ms), glob pattern filtering (`*.rs`, `*.py`), max-triggers safety cap (default 50, prevents infinite loops), runtime policy checking (trigger action is policy-checked at fire time, not registration time). Uses the `notify` crate (inotify/kqueue). Disabled by default (execution action).

**Architecture:**
- `executor/interactive.rs` — stdin pipe manager with `Arc<Mutex<ChildStdin>>` for safe concurrent writes.
- `executor/browse.rs` — portable browser engine chain with SSRF firewall.
- `executor/watch_react.rs` — async file watcher with debounced, policy-gated trigger loop.
- `PolicyEngine` — now `#[derive(Clone)]` for use in spawned watcher tasks. New `evaluate_with_config()` two-arg method. `SendInput` added to `ALWAYS_ALLOW`. `BrowseUrl`/`WatchAndReact` classified as run/network actions for confirm/readonly mode enforcement.
- `config.rs` — `SendInput` enabled by default (read-style); `BrowseUrl` and `WatchAndReact` disabled by default (opt-in via `[actions]`).
- `ACTION_REGISTRY` — full JSON Schema + examples for LLM self-discovery.
- Display integration — category badges (`meta`/`net`/`exec`), collapsed-line key params, inline display in agent loop.

### Tests
- **+10 new tests** covering the three primitives:
  - `interactive.rs` (4): handle not found, completed process, no stdin pipe, running process write
  - `browse.rs` (6): SSRF blocking (loopback, private, metadata), public URL allow, HTML stripping, title extraction, truncation, scheme validation
  - `watch_react.rs` (3): nonexistent path rejection, handle creation, file-change trigger with event verification

### Added

#### Podman — first-class container backend (Linux + macOS)

- **`RunInSandbox` upgraded** — new `cpu_limit` (vCPU cap) and `collect_files` (extract files via `podman cp` post-execution) parameters. Resource limits (`memory_mb`, `cpu_limit`, `timeout_secs`) are now enforced against configurable maximums in `[sandbox.podman]`.
- **`RunContainerGroup` action** — run N ephemeral Podman containers in parallel, each with its own image, command, files, and env vars. Concurrency controlled by `tokio::Semaphore` (configurable `max_parallel`, default: 4). Each task is independently timed and cleaned up.
- **`[sandbox.podman]` configuration** — new TOML section with `allowed_images` (mandatory allowlist — **fail-closed**), `default_memory_mb`, `max_memory_mb`, `default_cpu_limit`, `default_timeout_secs`, `max_timeout_secs`, `default_network`, `security_profile`, `no_new_privileges`, and `max_parallel_instances`.
- **RAII `ContainerGuard`** — Drop-trait based cleanup guarantees forceful container removal even on panics, timeouts, or abrupt terminations. No zombie containers on the host.
- **Image allowlist enforcement** — both `RunInSandbox` and `RunContainerGroup` validate every image against `sandbox.podman.allowed_images` in the policy engine. Wildcard prefix matching supported (e.g. `python:3.*`). Empty allowlist = deny all.
- **Security hardening** — `--cap-drop=ALL`, `--no-new-privileges`, env var key validation (alphanumeric + underscore only), path traversal prevention on injected files. Security profile configurable (`seccomp`, `apparmor`, or `none`).
- **Display integration** — `RunContainerGroup` appears in the Execute category with task count in collapsed view and `ok/total/failed` in result preview. Denial hints guide users to the correct config section.
- **Discovery** — both actions registered in `ACTION_REGISTRY` with full JSON Schema and examples for LLM self-discovery via `ListActions` / `DescribeAction`.
- **macOS support** — Podman backend works via `podman machine` on macOS. No daemon required (rootless by default).

### Changed

- **Policy engine** — `RunInSandbox` image check migrated from `policy.allowed_images` to `sandbox.podman.allowed_images` (fail-closed: empty list = deny all, was previously allow-all-when-empty). `RunContainerGroup` added to `is_run_command()` for readonly/confirm mode enforcement.
- **Denial hints** — `RunInSandbox` fix message now points to `sandbox.podman.allowed_images` instead of `policy.allowed_images`.



#### Multimodal — vision & multi-model pipelines

- **Vision pipeline** — screenshots captured via the `Screenshot` action are now injected as base64 image attachments into the conversation. All three LLM backends (Anthropic, Ollama, OpenAI-compat) serialize them into their native multi-part formats. Previously, `build_screenshot_message()` was dead code — the image was encoded but discarded. Now the LLM actually *sees* the screenshot.
- **`OllamaChat` action** — multi-turn chat with Ollama models, with optional per-message image attachments for vision models (llava, bakllava). Uses `/api/chat` with full message history, unlike `LlmCall` which is single-shot.
- **`ImageAttachment` type** — new `images: Vec<ImageAttachment>` field on `Message` for attaching vision data to any conversation message. Backward compatible — empty by default, `#[serde(default)]`.
- **Multi-model pipeline pattern** — documented in root README: chain `Screenshot` → `OllamaChat` (vision) → `LlmCall` (code) in a single `Pipeline` to leverage different models for different subtasks.

#### Setup flow unification (CLI ↔ TUI ↔ `/setup`)

All 5 entry points now use the same LLM setup wizard (`llm_setup::setup_llm`):
- `x0ne` (first-run) → `setup.rs` → `setup_llm()`
- `x0ne --setup` → `setup.rs` → `setup_llm()`
- `x0ne --tui` → `tui_mode.rs` → `setup_llm()` (was `create_backend`, now auto-connect + interactive fallback)
- `/setup` (TUI slash) → `setup.rs` → `setup_llm()`
- `/setup` (CLI slash) → `setup.rs` → `setup_llm()`

Previously `setup.rs` had its own inline LLM picker (4 raw choices, no service picker, no model listing) while `llm_setup.rs` had the polished flow (Anthropic/OpenAI/Groq/OpenRouter picker, auto-connect, Ollama LAN scan, model fetching). The old flow is deleted (~120 lines removed).

#### LLM-UX: String primitive field aliases

- `StrCount` and `StrContains` now accept `input`, `text`, or `string` as aliases for the `haystack` field. Backward-compatible — existing JSON with `haystack` still works. Eliminates ~90% of first-attempt validation errors from LLMs that guess the wrong field name.

#### LLM-UX: Pipeline per-step parse error reporting

- Pipeline and Try step arrays now use a custom deserializer that reports the exact step index and type on parse failure. Error messages changed from `"missing field 'values'"` to `"Failed to parse step 2 (Min): missing field 'values'"`. Cuts LLM retry loops from 3-4 attempts to 1.

### Changed

- `StrSlice` now clamps `end` to string length instead of erroring when `end > len` (Python-style: `"short"[0:100]` → `"short"`). Still errors on `start > len`. LLMs should not need to know exact string length before slicing.
- `setup.rs` — removed dead `read_password()` function (superseded by `terminal::read_secret_line`).
- TUI mode — replaced silent `api::create_backend()` with `llm_setup::setup_llm()` for interactive recovery on LLM init failure.

### Tests

- **+13 new tests** in `x0ne-runtime`:
  - Field alias deserialization: `str_count_accepts_input_alias`, `str_count_accepts_text_alias`, `str_count_canonical_haystack_still_works`, `str_contains_accepts_input_alias`, `str_contains_accepts_string_alias`
  - Pipeline error reporting: `pipeline_step_error_includes_index_and_type`, `pipeline_valid_steps_parse_fine`, `try_step_error_includes_index_and_type`
  - Memory in Pipeline: `pipeline_with_remember_recall_forget_parses`
  - StrSlice clamping: `test_str_slice_out_of_bounds_end_clamps`, `test_str_slice_out_of_bounds_start_errors`

### Added

Twelve targeted fixes surfaced by two LLM-UX stress tests (Claude Opus + Mistral 7B, 2026-04-19). Scope discipline was strict: **no new primitives, no new features, no Recipes, no x0ne-web, no kernel work** — only the audit findings. Canonical X0NE action format (`{"type": "ActionName", ...}`) is preserved everywhere; the runtime is now tolerant at the input boundary with one-time per-session deprecation warnings.

##### T0.5 — Plan auto-approve in non-interactive context [security]
- `sysaicalls::display_plan_and_confirm` split into a pure decision core (`decide_plan_approval_inner`) and an I/O shell. New structured `PlanDecision { approved, decision_source, error }` and `PlanError` types are now serialised into the session JSONL.
- Decision sources: `tty_yes`, `tty_enter_default`, `env_auto_approve`, `rejected_non_interactive`, `rejected_subagent`, `rejected_eof`. Reviewers can now prove every Plan was either explicitly approved or explicitly auto-approved — never accidentally.
- Heuristic is **input-presence based**, not TTY-based: `echo "y" | x0ne` (piped intent) → honoured; `x0ne < /dev/null` (sub-agent style) → `plan_requires_human`; real terminal → standard prompt. Sub-agent contexts always reject (no human in the loop).

##### T1 — Host/sandbox context bleed [demo-killer]
- `prompt::boot_context_block(...)` gains a `listing_failed_remote: bool` flag. In remote-bridge mode (`stdio` / `http` / `sandbox`) when the boot `ListDir` probe fails, the prompt now emits an explicit `WARNING: sandbox state unknown` block instead of falling back to silent host paths. Stops the LLM from hallucinating host filesystem layout inside an Incus container.

##### T2 — Pipeline output blackhole [demo-killer]
- New helpers `pipeline_tally_from_step_results` + `augment_pipeline_result_with_tally` in `agent_loop.rs`. Every Pipeline result (orchestrator-handled or core-handled) now carries `{ total, ok, failed, failed_at }` plus a one-line `output_summary`. TUI mode applies the same augmentation.

##### T3 — BroadcastAgents receipt richness [demo-killer, correctness]
- `execute_broadcast_agents` rewritten. New optional `targets: [...]` array; receipt now reports `requested_targets`, `resolved_targets`, `delivered_to`, and `failed_targets` with structured reasons (`not_found`, `agent_completed`, `agent_failed`, `inbox_lock_poisoned`). `delivery_mode = "mailbox_append"`.
- New `BROADCAST_SEQ` (`AtomicU64`) + `BroadcastEntry { message_id, body, require_ack, delivered_at, acknowledged_at }` per inbox.
- `success` semantics tightened: `true` iff every *resolvable* target received the message (was previously fire-and-forget).

##### T4 — `actions_executed` always 0 → per-source breakdown
- `ActionStats` extended with `core_count` / `orchestrator_count`. Snapshot JSON now exposes `by_source: { core, orchestrator }` alongside the existing `by_type` histogram. Drives `agent_loop::action_source(action_type)` (single source of truth).

##### T5 — Pipeline step source tagging
- `normalize_step_result` now stamps `"source": "core" | "orchestrator"` on every step so the LLM can reason about where each Pipeline rung executed without re-deriving it.

##### T6 — Discovery merged with source tags
- `prompt::orchestrator_action_metas`, `merge_orchestrator_into_list_result`, `describe_orchestrator_action`. `ListActions` and `DescribeAction` now return orchestrator-only sysaicalls (`Plan`, `SpawnAgent`, `WaitAgent`, `ListAgents`, `BroadcastAgents`, `Reflect`, `Matrix`) merged with the core registry, each tagged `source`, `supports_pipeline`, `supports_streaming`. No more "action not found" on the orchestrator's own primitives.

##### T7 — Error-class collapse + fake "Parse error" labels [critical]
- `SystemFeedbackClass` enum extended: `parse_error`, `validation_error`, `policy_denied`, `not_found`, `runtime_error`, `internal_error`. New `from_core_class(&str)` lowering honours typed `error.class` from `x0ne-runtime` instead of substring-matching messages.
- Legacy `Conversation::add_error` shim now warns via `tracing::warn!` and routes through `add_system_feedback`. Unknown error classes fall back to `runtime_error` (never `policy_denied`).
- `agent_loop` and `tui_mode` audited end-to-end; substring matching is now only a fallback for legacy results without a `class` field.

##### T8 — Fabricated-data detector narrowed
- `parser::looks_like_fabricated_data` reduced to three high-precision patterns: fake `<r>` / `<result>` tags outside `<action>`, `action_type` + runtime-only fields outside `<action>`, and code fences containing `action_type` + ≥ 2 runtime fields. Negative tests added for markdown tables, math expressions, and path-like strings to prevent narrative false positives.

##### T9 — Action-only coercion + silent `[ACTION]{}` drops
- `LlmOutput::Action` now carries `wrapper: WrapperKind` (`Canonical`, `ActionTag`, `CodeFence`, `JsonOnly`, …). New silent-drop guard in `parse_llm_response`: `[ACTION]` markers without a parseable JSON body now return a `ParseError` with a corrective nudge instead of being dropped.
- `WrapperKind::nudge_text()` arms a one-time pre-result deprecation warning so the LLM gets one — and only one — gentle correction per session.

##### T10A — Lenient wrapper parser (input boundary)
- `LlmOutput::Action` gains `unwrapped_field: Option<String>`. New `try_unwrap_field_wrapper` flattens fields nested under `args`, `input`, `parameters`, `arguments`, or `payload` into the canonical action root — but only when the wrapper key is the *only* sibling of `type` (conservative, no surprise rewrites).
- One-time per-session deprecation warning is emitted in both `agent_loop.rs` and `tui_mode.rs` when an unwrap happens.

##### T10B — Diff-style validation errors
- New `agent_loop::enrich_validation_error(action, &mut result, discovered)` augments `validation_error` payloads from `x0ne-runtime` with `sent` (the original action), `expected_schema` (from the discovery cache), and a natural-language `diff` ("missing field `path`", "field `count` expected `integer`, got `string`", "unknown field `dest` — did you mean `to`?"). Wired into both CLI and TUI dispatch paths.

##### T11 — Repeated-action circuit breaker
- New module `x0ne-orchestrator/src/loop_tracker.rs`. `RepeatedActionTracker` keys consecutive failures by `(action_type, normalized_args_json, error_code)`. `LOOP_THRESHOLD = 5`.
- Pre-dispatch check `would_block_any_error` returns a structured `loop_detected` envelope (`retryable: false`) on the 5th identical failure, surfaced through `SystemFeedbackClass::PolicyDenied` framing ("do NOT retry, change your approach"). No new `SystemFeedbackClass` variant was added — the existing T7 "do NOT retry" wording reaches the LLM verbatim via the `policy_denied` lane.
- Post-dispatch `record_outcome` resets the counter on success or whenever `(args, error_code)` changes, so legitimate retries with a different signature aren't penalised.
- Argument normalisation strips `type`, recursively sorts object keys, so `{"a":1,"b":2}` and `{"b":2,"a":1}` collapse to the same loop signature.
- **TUI parity** — `tui_mode.rs` now wires the same per-turn `RepeatedActionTracker` as `agent_loop.rs`. Pre-dispatch `would_block_any_error` short-circuits stuck loops before the bridge spins up; post-dispatch `record_outcome` resets the counter on success or signature change. TUI users now get the same "Circuit breaker tripped" `policy_denied` framing as CLI users — Mistral can no longer burn a TUI session in identical retries.

##### Test harness — red → green discipline
- `x0ne-orchestrator/tests/integration_demo.rs` (Test A — GOSIM demo, 21 cases) covers T0.5, T1, T2, T3, T4, T5, T6, T7, T8.
- `x0ne-orchestrator/tests/integration_classroom.rs` (Test B — Mistral 7B classroom survival, 12 cases + 1 ignored live) covers T9, T10A, T10B, T11. Live row `mistral_recovers_from_validation_error_within_5_turns` runs with `--ignored` against a real endpoint.

#### ⫸ Parallel Worlds — shared substrate for CLI and TUI
- **`worlds_registry`** — process-global broadcast channel (`tokio::sync::broadcast`) + bounded replay buffer (512 events). Any code path that spawns work publishes `WorldEvent`s exactly once; all observers (CLI `/worlds`, TUI pane, future SSE) subscribe to the same stream.
- **`WorldsRenderer::render_lines()`** — ratatui-native sibling of the existing ANSI `render()`. Both renderers share the same layout math (column widths, history rows, status tags), so the CLI split-pane frame and the TUI pane are visually identical by construction.
- **CLI slash commands:**
  - `/worlds` — inline live split-pane renderer driven by the registry. Detach with Ctrl+C; workers keep running.
  - `/spawn [N] [prompt]` — seed N synthetic parallel worlds with deterministic outcomes (completed, rolled-back, failed). Great for demos, and the backbone of the zero-LLM integration tests.
  - `/worlds-reset` — clear the registry.
- **TUI Parallel Worlds pane (F10)** — dedicated pane replacing the Pipeline view when toggled. Subscriber task forwards live events into `AppState` so the draw path stays pure. F-key bar highlights `F10 ⫸ Worlds (active/total)` whenever any world is registered, even while the pane is hidden.
- **`spawn_subagent` auto-registration** — every real sub-agent now emits `ActionStarted("SpawnAgent") → ActionFinished → WorldCompleted(summary)` so it appears in both UIs automatically. Foundation for per-primitive lifecycle events in a future PR.

#### CLI / TUI complementarity
- CLI banner: `Safe Agentic Runtime` + live `⫸ N worlds live` counter (or `⫸ /worlds /spawn` hint when idle).
- TUI header tagline: `Safe Agentic Runtime · bridge` + live `⫸ N worlds live · M done` counter next to the token meter.
- Shared `⫸` parallel-arrows motif across both surfaces as the visual anchor of the parallel-worlds concept.
- `/help` in both CLI and TUI gain a dedicated `⫸ Parallel Worlds` section.

#### Staged Ctrl+C (double-press semantics)
- New `interrupt` module: process-global `AtomicU8` state + `AtomicU64` timestamp, `tokio::sync::Notify` for async waiters.
- First Ctrl+C gracefully stops the current LLM streaming, cancels in-flight actions through `tokio::select!`, and injects a system message asking the agent to acknowledge the interruption.
- Second Ctrl+C within a 1 s window quits the application.
- Works uniformly in the REPL, the autonomous loop, and during action execution.

#### Input queue
- New `input_queue` module: non-blocking stdin drain via `libc::poll(2)` with a zero timeout, then `libc::read`.
- Users can type while the agent is processing; pending lines are replayed on the next turn.

#### TUI upgrades
- X0NE logo header (two-line glyph) integrated into the ratatui layout alongside live provider/model/bridge/token status.
- F5 Anthropic provider switching now auto-resolves the API key from: explicit argument → `ANTHROPIC_API_KEY` → `config.anthropic.api_key_env` (previously failed silently when invoked without an explicit key).
- TUI slash routes for `/worlds`, `/spawn`, `/worlds-reset` — typing `/spawn 4` auto-opens the Worlds pane.

#### Sticky header (opt-in)
- New `sticky_header` module using VT100 scrolling regions (DECSTBM).
- Enabled with `X0NE_STICKY_HEADER=1`. Off by default to preserve native terminal scrollback. `Drop` guard restores terminal state on exit.

#### HTTP Serve Mode (`x0ne --serve`)
- **Headless HTTP/SSE server** — exposes the full agent loop as `POST /prompt` with Server-Sent Events streaming (tokens, actions, results).
- **Session isolation** — per-chat `X-X0ne-Session` header, LRU eviction (30 min TTL, 5000 max), separate `Conversation` and `AgentRegistry` per session.
- **Multi-agent in serve mode** — `SpawnAgent`/`WaitAgent`/`ListAgents`/`BroadcastAgents` work per-session with configurable caps.
- **LLM modularity** — per-request provider/model/base_url overrides via POST body fields.
- **Bearer auth** — optional `--serve-token` flag for API authentication.
- Gated by the `serve` feature (default: on). Build with `--no-default-features` to strip axum/tower-http entirely.

#### Security
- **SSRF protection** — core-level firewall (`is_ssrf_target()`) blocks cloud metadata (`169.254.169.254`), loopback, private networks, and link-local addresses on all network actions.
- **Try/Assert migration** — `Try` and `Assert` moved from orchestrator sysaicalls to native `x0ne-runtime` primitives, ensuring they pass through the policy engine and benefit from consistent logging.

### Changed
- System prompt standardised across CLI and TUI modes.
- Per-chat session isolation for the HTTP serve mode (`feature = "serve"`).
- CLI action display — full execution chronology is preserved; action cards no longer collapse/disappear after completion.
- `/help` reorganised into clearly labelled sections (General, Files & Context, Git Workflow, Actions & Handles, Parallel Worlds, CLI Modes).

### Fixed
- **Pipeline** — error attribution now pinpoints the failing step instead of marking the whole pipeline as failed.
- **RunCommand** — disambiguates policy denial, missing binary, and runtime failures in error messages.
- **Sandbox** — context auto-seeding so the default working directory / container profile doesn't block first-run.
- **CLI display** — reasoning text no longer disappears between actions.
- **CLI display** — UTF-8 boundary panic on multi-byte characters (═, ╔, etc.) in string truncation — fixed `format_param_value`, `print_reasoning`, and string result display to use `chars().take(n)` instead of byte slicing.
- **Parser** — hallucination detection no longer flags legitimate `DescribeAction` output as a false positive.
- **Parser** — `type` field is now strictly required in action JSON; a dedicated `near_miss` diagnostic catches the common `{"action_type": …}` mistake and surfaces a targeted error to the LLM for self-correction.
- **Plan action** — `Plan` primitive results are now written to the session log (previously bypassed the bridge-dispatch logging path).
- **Interrupt state machine** — double-press detection is now based purely on timing between presses, making it robust to `reset()` calls from the readline branch.


### Tests
- **+27 new tests** covering the parallel-worlds stack:
  - `worlds_registry` (10): pub/sub, replay, implicit registration, idempotent register, multi-subscriber broadcast, late-subscriber race-freedom, bounded replay, reset safety, global singleton.
  - `worlds_view::render_lines` (2): structural parity with plain render, spinner presence for running worlds.
  - `worlds_cli::spawn_demo` (3): immediate registration, N-clamping, deterministic termination.
  - `tests/worlds_e2e.rs` (3): end-to-end pipeline (spawn → registry → renderer → CLI/TUI frame parity), late-subscriber replay race, reset safety with active subscribers.
  - `interrupt` tests: `double_press_survives_reset`, `wait_returns_after_trigger`.
- Test-isolation guard: `static Mutex` serialises tests that touch the process-global registry so `cargo test --lib` stays deterministic.
- **Suite**: `315 passed` / `1 failed` (pre-existing `latex::tests::subscripts`, unrelated).

#### Incus bridge from sandbox mode
- **Dual-bridge architecture** — when running `make run-incus` (sandbox mode), the orchestrator now maintains a **secondary host-side x0ne-runtime bridge** dedicated to `RunIncus`/`RunIncusGroup`. The agent's standard actions execute inside the sandbox container while Incus parallel worlds run from the host, where the `incus` CLI is available.
- `dispatch_single_action` gains an `is_incus_action()` check: `RunIncus`/`RunIncusGroup` → incus bridge (sandbox mode) or primary bridge (host mode).
- `SpawnContext` gains an `incus_bridge: Option<SharedBridge>` field.
- `SharedBridge` type alias: `Arc<tokio::sync::Mutex<Box<dyn Bridge>>>`.
- Startup banner shows `⚡ Incus bridge ready` when the host-side bridge initialises.
- +5 unit tests: `is_incus_action` recognition/rejection, orchestrator-action exclusion, Pipeline routing with/without Incus steps.

#### Orchestrator-first dispatch hardening
- **`execute_spawn_agent` refactored to synchronous** — removes recursive async-future type cycle that caused `Send` bound compilation errors when sub-agents spawn further work.
- `run_agent_loop` signature extended: `registry`, `spawn_ctx`, `llm_arc` parameters for unified action routing.
- Pipeline interception now checks both `is_orchestrator_action()` and `is_incus_action()`.

#### Test fixes
- **`mcp_compliance::test_essential_tools_present`** — corrected tool name `search_in_files` → `search_code`.
- **`latex::tests::subscripts`** — fixed contradictory expectation; bare `_0` does not convert (by design), only `_{0}` does.
- **Scenario runner** — pre-flight Incus connectivity check (5s timeout); unreachable remotes are skipped with a warning instead of failing with 20s timeouts.

#### Documentation
- **Root README** — Sysaicalls section rewritten with orchestrator-first dispatch, Parallel Worlds restructured into 2 levels + sandbox mode, new Testing section (339 unit / 175 scenarios / 13 Incus), updated architecture diagram and project layout. New **Time Travel** subsection documenting Incus snapshot/rollback as instant undo for exploratory agents.
- **x0ne-runtime README** — new Testing section with scenario runner, categories, MCP compliance, and Incus integration test tables. New **Time Travel** subsection in Incus containers documenting `base_snapshot`, `snapshot_on_exit`, and `RestoreSnapshot` for btrfs-backed rollback.
- **x0ne-orchestrator README** — agent loop diagram updated with 4 dispatch paths, sysaicalls expanded with Pipeline example and safety boundaries, security model updated with dual-bridge architecture.

#### Security hardening
- **`x0ne-runtime-incus-only.toml`** — hardened config for the host-side Incus bridge. Only `RunIncus`/`RunIncusGroup` + meta actions enabled; all filesystem, command, and network actions denied. Prevents the host-side x0ne-runtime from being used as an escape vector even if routing logic is bypassed.
- Host-side bridge now uses `--config x0ne-runtime-incus-only.toml` explicitly instead of falling back to default/user config.

#### CI/CD
- **`ci.yml`** — added `Run Scenario Suite` step (175 scenarios without LLM or Incus).
- **`release.yml`** — release package now includes `config/` directory with all TOML configs (`x0ne-runtime.toml`, `x0ne-runtime-sandbox.toml`, `x0ne-runtime-incus-only.toml`, `x0ne-orchestrator.toml`).
- **`install.sh`** — fixed repo (`x0ne-labs/x0ne-runtime`), enabled macOS downloads, fixed tarball naming to match release artifacts (`x0ne-{os}-{arch}.tar.gz`), config path updated for new `config/` layout.

### Suite (current)
- **Unit tests**: 593+ passed (was 580+)
- **Integration tests**: 34 passed (Test A: 22 GOSIM demo, Test B: 12 classroom survival)
- **Scenario suite**: 175 passed
- **Incus integration**: 13 passed

## [0.2.0-alpha] — 2026-04-12

An internal milestone, never tagged or published; it was labelled 0.2.0 at
the time, before the 0.2.0 betas and 0.2.0 itself.

### Added
- **80+ typed action primitives** — filesystem, git, process, desktop, math, string, network, snapshots, pipelines
- **Policy engine** — deny-by-default with per-action enable switches, command/path whitelists, deny patterns
- **Three communication modes** — stdio, HTTP, MCP (Model Context Protocol)
- **Pipeline meta-action** — batch execution with `stop_on_error` and automatic snapshot/rollback
- **Handle system** — long-running process and event tracking with ring buffers
- **Desktop automation** — Wayland-native mouse, keyboard, screenshot (opt-in)
- **Orchestrator** — Anthropic, Ollama, OpenAI-compatible LLM backends
- **TUI mode** — ratatui-based terminal UI (experimental)
- **Sysaicalls** — orchestrator-side `Try`, `Assert`, `Plan` meta-actions
- **Session logging** — JSONL audit logs for all interactions
- **QR code pairing** — mobile connection via local network
- **Setup wizard** — interactive first-run configuration
- **HTTP bearer auth** — optional token-based API authentication
- **Configurable CORS** — localhost-only by default

### Security
- Deny-by-default policy engine with path canonicalization
- No shell access — `Command::new()` only, no `sh -c`
- Hardcoded deny patterns for `.ssh`, `.gnupg`, `*.pem`, `*.key`, `secrets/`
- Container image allowlist for `RunInSandbox`
- Environment variable whitelist for `GetEnv`
- Desktop rate limiter (100ms between inputs)
- Pipeline preflight — all steps policy-checked before execution
