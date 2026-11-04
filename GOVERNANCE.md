# Governance

A small project with one maintainer. This file says who decides and how a
change gets in, so that nothing depends on a conversation nobody can find.

## Roles

- **Maintainer**: the owner of the `x0ne-labs` GitHub organization. Decides
  scope, merges, releases, and acts as Community Moderator for
  `CODE_OF_CONDUCT.md` and as the contact of `SECURITY.md`. The maintainer
  is x0ne, the copyright holder named in `LICENSE`; there is no other
  maintainer today.
- **Contributor**: anyone who opens an issue or a pull request. No CLA; by
  contributing you license your change under Apache-2.0 (`CONTRIBUTING.md`).

## How a change gets in

1. Every change goes through a pull request, including the maintainer's own.
2. CI must be green: the required checks listed in `docs/BRANCHING.md`.
3. A change to the policy, the isolation or an action's side-effect class
   needs a test that fails without it, and a line in `CHANGELOG.md`.
4. The maintainer reviews contributors' pull requests; with a single
   maintainer, the maintainer's own pull requests are merged on green CI.
5. Dependency updates (Dependabot) are merged on green CI; a major version
   bump of a cryptographic or attestation crate is reviewed like a code change.
   The Engine (`x0ne-confidential`, a git dependency) is not Dependabot's: it
   moves only through `scripts/repin-public.sh`, and its code changes are
   reviewed like ours.

## Decisions

Product decisions (what the harness promises, what the policy and the
isolation cover, the action vocabulary) are the maintainer's and are written down
in `README.md`, `SECURITY.md` or `CHANGELOG.md` when taken.
Disagreement goes in an issue; the maintainer's answer closes it.

## Changing this file

By pull request, like everything else.
