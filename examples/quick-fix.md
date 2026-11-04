# Example: Quick Fix

**Scenario:** You have a compile error and want the agent to fix it automatically.

---

```
User: fix the compile error in src/main.rs
```

```
[Reasoning] I'll start by reading the file and running cargo check to see the error.

[ACTION] Pipeline
  Step 0: ReadFile ./src/main.rs
  Step 1: RunCommand cargo check

[RESULT] Pipeline 2/2 steps
  Step 0 ✓ ReadFile (3 ms) — 2.1 KB read
  Step 1 ✗ RunCommand (1840 ms) — exit 1
```

```
[Reasoning] cargo check failed. Let me look at the stderr to identify the error location.

The compile output shows:
  error[E0308]: mismatched types
   --> src/main.rs:47:27
    |
47  |     let x: u32 = get_signed_value();
    |            ^^^   ^^^^^^^^^^^^^^^^^ expected `u32`, found `i32`

The variable x is declared as u32 but get_signed_value() returns i32. I'll fix the type.

[ACTION] ReplaceInFile
  path: ./src/main.rs
  old:  "let x: u32 = get_signed_value();"
  new:  "let x: i32 = get_signed_value();"

[RESULT] ✓ 1 replacement made (2 ms)
```

```
[ACTION] RunCommand cargo check
[RESULT] ✓ exit 0 (2.1 s)
```

```
Fixed! The variable `x` on line 47 was declared as `u32` but `get_signed_value()`
returns `i32`. Changing the type annotation to `i32` resolved the mismatch.
```

---

**What happened technically:**

1. Pipeline read the file and ran `cargo check` in one round-trip (saving 1 LLM call)
2. The agent parsed the compiler error output from the failed step result
3. `ReplaceInFile` did a targeted string replacement — no full file rewrite
4. Second `cargo check` confirmed the fix compiles cleanly
