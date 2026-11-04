# WebSearch and FetchUrl

## WebSearch: backends and privacy

Backends are tried in order; the first answer wins.

| Order | Backend | Condition | What leaves the machine |
|---|---|---|---|
| 1 | Exa public MCP endpoint, `https://mcp.exa.ai/mcp` | always, unless `X0NE_EXA_DISABLE=1` | the query text, the `x0ne/<version>` user agent and your IP address. **No API key is sent.** |
| 2 | Brave Search API | `X0NE_BRAVE_API_KEY` is set | the query and your Brave key, sent to Brave |
| 3 | DuckDuckGo HTML (`html.duckduckgo.com`) | always | the query, sent to DuckDuckGo |

- The result's `source` names the backend that answered. `fallbacks` lists each backend that failed before it, with the error.
- When every backend fails, the error names every cause.
- A DuckDuckGo anti-bot page is an error, not "0 results". On 2026-10-06, from one host, it answered 200 with a challenge page (67 `anomaly` markers).
- Exa's terms and data handling apply to what it receives. If a query must not leave for a third party, set `X0NE_EXA_DISABLE=1`.
- In the sandbox, `mcp.exa.ai` is in `scripts/sandbox-egress/allow.default`.
- After a suspected prompt injection, WebSearch needs confirmation like any other network egress: the query itself is outbound data.

## FetchUrl

| Field | Default | Notes |
|---|---|---|
| `format` | `markdown` | `markdown`, `text` or `html`. Applies to `text/html` bodies; other types come back as-is. |
| `timeout` | 30 s | clamped to 1..120 s |
| `offset` | 0 | byte offset into the converted text; use the previous `next_offset` |

- Each call returns at most 64 KiB of text. A cut body ends with a note and the result carries `next_offset`.
- Bodies up to 5 MiB are read.
- The converted text is cached for 15 minutes per URL and format, so `offset` reads do not refetch. Requests with caller `headers` are never cached.
- A 403 carrying `cf-mitigated: challenge` is retried once with a plain `x0ne/<version>` user agent, unless the caller set one.
- The SSRF guard (private ranges, metadata endpoints, DNS pinning, redirect re-checks) applies before every fetch, cache hits included.

## Citations

Every URL returned by WebSearch or read by FetchUrl gets an id `S<n>` in the `source` field, stable for that URL for the life of the runtime process. The TUI Sources panel shows the same ids.
