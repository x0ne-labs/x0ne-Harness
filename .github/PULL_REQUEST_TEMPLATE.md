<!-- Title: Conventional Commits, e.g. "fix(policy): refuse an empty allow-list".
     It becomes the squashed commit on main (docs/BRANCHING.md). -->

## What and why

## How it was checked

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo test --locked --workspace`
- [ ] A change to policy, isolation or an action's side-effect class has a test that fails without it
- [ ] `CHANGELOG.md` updated if users see the change

Not a vulnerability report: see `SECURITY.md`.
