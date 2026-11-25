# clean-room-opcua-mcp-demo

Two small Rust crates that illustrate patterns for industrial data plumbing:

| Crate | What it shows |
|---|---|
| [`crates/staleness-watchdog`](crates/staleness-watchdog) | Detecting a *silently stalled* OPC UA subscription (session "connected", data frozen) with a deterministic state machine, a cooldown, and a rolling burst cap on reconnects. |
| [`crates/readonly-mcp`](crates/readonly-mcp) | A minimal, read-only [Model Context Protocol](https://modelcontextprotocol.io) server (JSON-RPC 2.0 over stdio) with RBAC scopes that can only ever **narrow**. |

## Clean-room statement

This is a **synthetic demo written from scratch to illustrate patterns; it contains no
employer code, data, plant names, table names or network details.** Every site
(`site-alpha`, `site-beta`, `site-gamma`), machine (`line-a1` ...), tag (`filler.speed` ...)
and number is invented. The only external identifiers are public OPC UA specification
node ids (e.g. `ns=0;i=2258`, `Server_ServerStatus_CurrentTime`).

Dependencies are limited to `tokio`, `serde`, `serde_json` and `thiserror`. There is no
OPC UA SDK and no MCP SDK; the minimal protocol pieces are implemented here. All crates are
`#![forbid(unsafe_code)]`.

## Running

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# Replay synthetic stalls in virtual time (hours of simulated time in milliseconds)
cargo run --example simulate -p staleness-watchdog

# Run the MCP server; the principal is a synthetic stand-in for a token (required, no default)
DEMO_PRINCIPAL=operator-site-alpha cargo run -p readonly-mcp < docs/smoke-session.jsonl
```

Synthetic principals: `viewer-all` (all three sites), `operator-site-alpha` (site-alpha
only), `operator-line-b1` (only machine `line-b1` on site-beta). An unset or unknown
`DEMO_PRINCIPAL` makes the server refuse to start (exit code 2). It does not fall back to a default.

## Design notes: staleness watchdog

**Problem.** An OPC UA session can stay "connected" while its subscription has stopped
publishing. Keep-alives still succeed, so connection-state checks miss it. The watchdog probes a
value the server must keep advancing, `Server_ServerStatus_CurrentTime` (`ns=0;i=2258`), and
reacts when it stops changing.

**Timings (defaults, all configurable and validated):**

| Setting | Default | Why |
|---|---|---|
| `probe_interval` | 30 s | Cheap enough to run forever; three probes fit inside the stale threshold. |
| `stale_threshold` | 90 s | Must exceed the probe interval (validated), so one late or lost probe never triggers a reconnect. A reconnect happens only when `age > 90 s`; exactly 90 s is still `Stale`. |
| `reconnect_cooldown` | 300 s | Lets a reconnected subscription settle, and stops reconnect storms against a struggling server. |
| `burst_cap` / `burst_window` | 4 per rolling 3600 s | Hard ceiling on reconnects per hour. The 5th reconnect is refused until the oldest one leaves the window. |
| `io_timeout` | 10 s | Bounds each probe and each reconnect. A timeout counts as a failed probe. Must be shorter than the probe interval. |

**Decisions per probe:** `Healthy` (value changed), `Stale { age }` (unchanged, within
threshold), `Reconnect { age }`, `SuppressedByCooldown { remaining }`,
`SuppressedByBurstCap { retry_after }`. A failed or timed-out probe counts as "no change".
A reconnect resets the freshness baseline, and the next changed value returns the watchdog to `Healthy`.

**Structure.**
- The core `Watchdog::observe(now, outcome)` is pure and takes the time as input, so tests run on scripted telemetry with no clock.
- The Tokio driver `run(source, reconnector, config, shutdown)` adds an interval ticker using `MissedTickBehavior::Delay`, so overruns push the schedule back instead of firing a catch-up burst at the server.
- Probes and reconnects run one at a time in a single task. There is no internal queue.
- Shutdown is a `tokio::sync::watch<bool>` polled with priority. Dropping the sender also shuts down.
- The reconnect is "in place": it reuses the existing session rather than opening a new one.
- Driver tests use `#[tokio::test(start_paused = true)]` with `tokio::time::advance` to step virtual time.

## Design notes: read-only MCP server and monotone narrowing

**Read-only by construction.**
- Every tool implementation receives `&Catalog`, never `&mut`, so no request can modify the dataset.
- Every tool advertises `annotations.readOnlyHint: true` and `destructiveHint: false`.
- Tests check that `tools/list` contains only these read tools and that the catalog is unchanged after every call.
- OEE (availability x performance x quality) is computed deterministically in Rust from counters.
- When OEE is aggregated across machines, the counters are summed first (time-weighted). Per-machine percentages are not averaged.

**Monotone scope narrowing.**
- A `Scope` is a role, a set of permitted sites and an optional set of permitted machines.
- A session starts from its principal's scope.
- Each `tools/call` may pass an optional `scope` argument, but `Scope::narrow` can only intersect:
  - Every requested site or machine must already be permitted, or the request is rejected (`-32001`).
  - A rejected request is never silently clipped, and it never widens the scope.
- Narrowing applies to one call only; the session scope never changes.
- So for any narrowing, `child.permits(x) => parent.permits(x)`.
- Unit tests check this exhaustively over every pair of subsets of a 3-site universe, and transitively over chains of narrowings.
- A machine that is out of scope gets the same `-32002` as a machine that does not exist, so a caller cannot probe for what exists.

**Transport policy.**
- Messages are newline-delimited JSON-RPC 2.0. Batches are rejected, and notifications are never answered.
- Handling is strictly sequential: read a line, handle it, write the response, flush.
- Lines are capped at 1 MiB. A longer line is drained and answered with `-32600`.
- Backpressure is the OS pipe. If the client stops reading stdout, the server stops reading stdin.
- stdout carries protocol only; diagnostics go to stderr.
- The handler `server::handle(&mut Session, Request) -> Option<Response>` is pure and tested without stdio.

**Error codes:**

| Code | Meaning |
|---|---|
| `-32700` | Parse error |
| `-32600` | Invalid request |
| `-32601` | Method not found |
| `-32602` | Invalid params, including an unknown tool |
| `-32001` | Scope escalation |
| `-32002` | Not found in scope |

## Example JSON-RPC session

Run as `operator-site-alpha` (responses abbreviated):

```jsonc
// -> client to server, <- server to client
-> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}
<- {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","serverInfo":{"name":"readonly-mcp-demo","version":"0.1.0"},"capabilities":{"tools":{"listChanged":false}}, ...}}
-> {"jsonrpc":"2.0","method":"notifications/initialized"}
   (no response: notification)
-> {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_machines","arguments":{}}}
<- {"jsonrpc":"2.0","id":4,"result":{"structuredContent":{"effective_scope":{"role":"operator","sites":["site-alpha"],"machines":null},"machines":[{"id":"line-a1",...},{"id":"line-a2",...}]},"isError":false, ...}}
-> {"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_oee_summary","arguments":{"scope":{"machines":["line-a1"]}}}}
<- {"jsonrpc":"2.0","id":5,"result":{"structuredContent":{"aggregate":{"oee":{"availability":0.9,"performance":0.95,"quality":0.97997...,"oee":0.83788...}}, ...}}}
-> {"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_machines","arguments":{"scope":{"sites":["site-beta"]}}}}
<- {"jsonrpc":"2.0","id":6,"error":{"code":-32001,"message":"site 'site-beta' is outside the current scope"}}
```

The full request file is [`docs/smoke-session.jsonl`](docs/smoke-session.jsonl).

## License

MIT. See [LICENSE](LICENSE).
