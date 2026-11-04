# 19 — a process you have to keep hold of

Start `server.py` on port `8765` in the background, confirm it answers on
`/ping`, then stop it **cleanly** (SIGTERM, not kill -9). Then run
`./check.sh`.

---

## Why this exercise exists

A running process is not a command that returns. X0NE's model for it is the
**handle**: `RunCommand` with `stream: true` returns a handle instead of
waiting; `ReadLogs` shows what it printed so far; `CheckHandle` says whether
it is still alive; `StopHandle` asks the process to stop (SIGTERM, a grace
period, then SIGKILL — since 2026-08-17; before that it went straight to the
kill and this exercise's `stopped.txt` was never written). Everything about
the process stays typed and inside the policy — no `nohup … &`, no `kill` by
pid guessed from `ps`.

To confirm the port answers: `PortCheck` on `127.0.0.1` is **refused** by the
SSRF guard unless the deployment sets `X0NE_ALLOW_LOOPBACK=1` (an audit
decision: unguarded, PortCheck was a blind local port scanner). Use `ReadLogs`
(the server prints `listening on …` and `served /ping`) or `ss -ltn` (an
allowlisted command), or ask for the loopback opt-in — the refusal is not a
bug, and routing around it is not the point.

The server writes `started.txt` when it is up and `stopped.txt` **only** when
it is stopped by SIGTERM (or SIGINT). A `kill -9`, or leaving it running,
leaves no `stopped.txt`.

`check.sh` grades: `started.txt` exists, `stopped.txt` exists, and nothing is
listening on 8765 any more. Leaving a server behind fails; so does the
kill-by-pid shortcut.
