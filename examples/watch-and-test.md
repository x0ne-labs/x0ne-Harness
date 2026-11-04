# Example: Watch and Test

**Scenario:** You're iterating on code and want the agent to watch for changes and re-run tests automatically.

---

```
User: watch src/ and re-run tests whenever something changes
```

```
[Reasoning] I'll set up a file watcher and start a streaming test run in parallel.
Both will return handles that I can monitor.

[ACTION] Subscribe
  event_type: {kind: "file_changed", path: "./src", recursive: true}

[RESULT] ✓ handle watch_f1a2b3 (2 ms)
```

```
[ACTION] RunCommand {command: "cargo", args: ["test"], stream: true}
[RESULT] ● handle proc_c4d5e6 (streaming)
```

```
I'm now watching ./src/` for file changes and running the test suite.
I'll report results as tests complete and re-trigger whenever you save a file.

Active handles:
  ● watch_f1a2b3 — file_changed watcher on ./src (recursive)
  ● proc_c4d5e6  — cargo test (running)
```

---

*[45 seconds later — cargo test finishes]*

```
[System] Auto-check handle proc_c4d5e6 (idle 60s)

[ACTION] CheckHandle proc_c4d5e6
[RESULT] ✓ status: completed, exit_code: 0

Output:
  running 12 tests
  test parser::tests::parse_action_tag ... ok
  test parser::tests::parse_pipeline ... ok
  ... (10 more)
  test result: ok. 12 passed; 0 failed; finished in 0.43s
```

```
✓ All 12 tests passing. Watching for changes…
```

---

*[User saves src/lib.rs]*

```
[System] Event on watch_f1a2b3: file_changed — ./src/lib.rs modified

[ACTION] StopHandle proc_c4d5e6
[RESULT] ✓ stopped

[ACTION] RunCommand {command: "cargo", args: ["test"], stream: true}
[RESULT] ● handle proc_g7h8i9 (streaming)
```

```
Change detected in src/lib.rs — re-running tests…
```

---

*[30 seconds later — tests fail]*

```
[System] Auto-check handle proc_g7h8i9

[ACTION] CheckHandle proc_g7h8i9
[RESULT] ✓ status: completed, exit_code: 101

Output:
  running 12 tests
  test parser::tests::parse_action_tag ... ok
  test parser::tests::parse_pipeline ... FAILED

  failures:
    parser::tests::parse_pipeline
      thread 'parser::tests::parse_pipeline' panicked at 'assertion failed: result.is_ok()'
      at src/parser.rs:89

  test result: FAILED. 11 passed; 1 failed
```

```
✗ Test failure after your change to src/lib.rs:

  parser::tests::parse_pipeline failed at src/parser.rs:89
  The assertion result.is_ok() failed.

Want me to read the affected test to help debug?
```

---

*[User: yes]*

```
[ACTION] Pipeline
  Step 0: ReadFile ./src/parser.rs  (offset at line 80, limit 20)
  Step 1: ReadFile ./src/lib.rs  (offset at line 1, limit 50)

[RESULT] 2/2 steps completed

[Reasoning] Looking at the test: it expects parse_pipeline() to return Ok(...) but
after your change to lib.rs, the function signature changed — it now returns
Result<Pipeline, ParseError> but the test calls it as if it returns Option<Pipeline>.

The fix is to update the test assertion to unwrap the Result.
```

---

**What happened technically:**

1. `Subscribe` sets up a file watcher — returns immediately with a handle, doesn't block
2. `RunCommand stream:true` starts tests in the background — also returns immediately with a handle
3. Orchestrator tracks both handles in `active_handles`
4. Auto-check every 60s drains buffered output from `CheckHandle`
5. When the file-changed event fires, the orchestrator receives it via the next `CheckHandle` on the watcher
6. Old test handle is stopped cleanly before launching a new run — no zombie processes
7. Failure output is delivered verbatim to the LLM for analysis

**Cleanup:** On `/quit`, the orchestrator calls `StopHandle` on all active handles before exiting.
