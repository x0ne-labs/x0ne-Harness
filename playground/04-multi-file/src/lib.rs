//! Exercise 04 — one change, three files.
//!
//! `Money` is an i64 count of CENTS everywhere, but nothing in the type says
//! so, and `parse.rs` already got it wrong once (see its test). The task:
//! make the unit impossible to get wrong, and update every call site.
//!
//! Suggested shape (any equivalent is fine, as long as it compiles and the
//! tests below still describe true things):
//!
//!   - a `Cents(i64)` newtype in `money.rs`, with `from_euros` / `as_cents`
//!   - `parse::parse_amount` returning `Cents`
//!   - `report::format_total` taking `Cents`
//!
//! This exercises editing several files coherently in one pass — a rename
//! that compiles in one file and breaks another is the failure mode.

pub mod money;
pub mod parse;
pub mod report;
