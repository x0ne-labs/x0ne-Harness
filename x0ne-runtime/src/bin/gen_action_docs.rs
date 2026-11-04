//! Emits the action reference from the shared vocabulary.
//!
//! `docs/actions.md` used to be written by hand. By the time it was replaced it
//! documented 22 actions that had been removed and omitted 53 that existed, and
//! its own header claimed a count that had been wrong for several releases.
//! Nothing detects that: prose does not fail to compile.
//!
//! So the reference is generated. `action_reference_is_up_to_date` in the
//! runtime's test suite compares the checked-in file against this output, which
//! turns documentation drift into a failing test with a one-command fix:
//!
//! ```text
//! cargo run -p x0ne-runtime --bin gen-action-docs > docs/actions.md
//! ```

fn main() {
    print!("{}", x0ne_runtime::docs::action_reference_markdown());
}
