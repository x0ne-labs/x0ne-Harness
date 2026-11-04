# Contributing to X0NE

Thanks for your interest in X0NE! Here's how to contribute.

## Getting Started

```bash
git clone https://github.com/x0ne-labs/x0ne-Harness.git
cd x0ne-Harness
cargo build --locked
cargo test --locked --workspace
```

`x0ne-confidential` comes from the x0ne-Engine repository, pinned to one
commit in `Cargo.toml`; cargo fetches it over the network. To work on it
against a local Engine checkout, put a patch in `.cargo/config.toml`, which is
ignored by git:

```toml
[patch."https://github.com/x0ne-labs/x0ne-Engine"]
x0ne-confidential = { path = "../x0ne-Engine/x0ne-confidential" }
```

With the patch, `Cargo.lock` changes and `--locked` fails: drop the patch
before committing.

## Commit messages

`scripts/check-commits.sh --msg <file>` checks one message the way the
maintainer's hook does: a subject of 72 characters at most, no trailing
whitespace, none of the words listed in `FORBIDDEN` at the top of the
script. You can install it as your own commit-msg hook
(`cp scripts/hooks/commit-msg .git/hooks/`); nothing else is required of a
contributor.

`make hooks` and the pre-push hook (`scripts/hooks/pre-push`) are the
maintainer's: they refuse any author other than `x0ne <dev@x0ne.co>` and
need a private deny list kept outside the repository, so they stop every
push from another clone. Do not install them in a fork.

## Development Workflow

1. **Fork** the repository
2. **Create a branch** named as in [docs/BRANCHING.md](docs/BRANCHING.md): `git checkout -b feat/my-feature`
3. **Make your changes** following the coding guidelines below
4. **Run tests**: `cargo test --workspace`
5. **Run lints**: `cargo clippy --locked --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
6. **Submit a PR** against `main`

## Coding Guidelines

- **Rust edition**: 2021 (stable toolchain only — no nightly features)
- **Error handling**: Use `thiserror` in library code, `anyhow` in binaries
- **`unsafe`**: no new block without a `// SAFETY:` comment stating the invariant it relies on
- **No `unwrap()` in production code**: Use `?`, `.unwrap_or()`, or `.expect("reason")`
- **Tests**: Add `#[cfg(test)]` inline tests for new logic
- **Format**: Run `cargo fmt` before committing

## Adding a New Action

Adding a new typed action to x0ne-runtime requires changes in several files:

1. `x0ne-runtime/src/action.rs` — Add a variant to the `Action` enum
2. `x0ne-runtime/src/executor/` — Implement the handler in the appropriate module
3. `x0ne-runtime/src/result.rs` — Add output variant to `ActionOutput` if needed
4. `x0ne-runtime/src/policy.rs` — Add policy rules (is_read/is_write, path extraction)
5. `x0ne-runtime/src/executor/meta.rs` — Add description and examples for `DescribeAction`
6. `docs/actions.md` — Document the new action

## Architecture

```
x0ne-runtime          → Runtime: policy engine + executor + handle manager
x0ne-orchestrator  → LLM loop: connects to x0ne-runtime via stdio/HTTP/SSH
```

See [docs/architecture.md](docs/architecture.md) for the full design.

## Reporting Issues

- **Bugs**: Open an issue with reproduction steps and OS/Rust version
- **Security**: See [SECURITY.md](SECURITY.md) — do NOT open public issues for vulnerabilities

## License

Contributions are accepted under the Apache License, Version 2.0 (`LICENSE`),
the licence of this repository. There is no contributor licence agreement.
Each commit carries a Developer Certificate of Origin sign-off
(<https://developercertificate.org/>): `git commit -s` adds the
`Signed-off-by: Name <email>` trailer, which states that you wrote the change
or have the right to submit it under that licence.

Third-party licenses are listed in [THIRD-PARTY.md](THIRD-PARTY.md), generated from
`Cargo.lock` by cargo-about (`make third-party`). Regenerate it in the same change
that touches `Cargo.lock`; the `deny` CI job checks the same allow-list (`deny.toml`).
