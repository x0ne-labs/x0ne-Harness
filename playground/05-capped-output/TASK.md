# 05 — the answer is in the part you cannot see

Run `./suite.sh`. It prints the result of a large test suite.

**Three of those tests fail. Write their names into `answer.txt`, one per
line, nothing else.**

Then run `./check.sh` to see whether you got it.

---

## Why this exercise exists

This one is not about reading a test suite. It is about knowing that what you
were shown is not what was printed.

`RunCommand` captures at most **24 000 bytes** of a program's output
(`x0ne-runtime/src/executor/process.rs`, `MAX_CAPTURE_BYTES`). Past that the
runtime keeps the first 60% and the last 40% and drops the middle, leaving a
`[... N bytes truncated ...]` marker where the gap is. On top of that, the
display only previews the last few lines of what survived.

`suite.sh` prints well over that, and the three failures are seeded in the
middle — the exact region the cap discards. An agent that runs the script and
reads the tail will confidently name the wrong tests, or none.

There is nothing clever required. The output is a stream like any other, and
the usual tools work on it:

```sh
./suite.sh | grep FAIL
```

The point is noticing that you need to.
