# Q2 CCTest baseline

- Date: 2026-07-16 (Asia/Shanghai)
- Target: `https://q2.quietfox.sbs/v1/messages`
- Model: `claude-opus-4-8`
- Deployed revision: `c19fc20c0a1b0972ff8755ef402382e1cf20cc41`
- Task ID: `db413948-183d-48a0-9bbf-6da15193d876`
- Token-usage check: disabled

No API keys are stored in this directory.

## Authoritative observations

The task was accepted and returned one live intermediate status before the
CCTest origin stopped responding. At that point:

- `tag_check`: `10/10`
- `stream_structure`: `5/5`
- all five remaining groups had started
- Q2 credential #3 had six additional successful upstream calls
- Q2 had no active request left, no restart, no OOM, and no error log

The CCTest origin then returned no response for the task endpoint or its own
homepage from two independent networks. Fifteen bounded resume polls were made
without submitting a second paid task. The first ten-poll outage record was
documented separately; the later five-poll retry exposed and motivated a fix so
future resume runs append rather than truncate polling history.

## Manual parity findings

The exact public request was replayed against POMO/Bedrock and Q2 for the
server-tool boundary:

| Probe | POMO/Bedrock | Q2 c19fc2 | Local action |
|---|---|---|---|
| `code_execution_*` | HTTP 400, unsupported tool type | HTTP 502, upstream `Invalid tool use format` | Fixed locally: AWS-B preflight now returns a Bedrock-shaped 400 |
| `web_search_20250305` | HTTP 400, unsupported tool type | HTTP 200 with search result and citations | Preserved: this is an existing user-facing Q2 feature |
| `output_config.format` | HTTP 400, extra input | HTTP 200 with schema-conforming JSON | Preserved: this is an existing user-facing Q2 feature |

The code-execution fix is narrowly keyed by the four verified official
`tools[].type` versions. A normal client-defined tool named `code_execution`, a
similar custom type, normal client tools, WebSearch, structured output, chat,
and coding requests stay on their existing paths.

## Local compiled-service regression

The modified release binary and the repository TLS sidecar were compiled and
run directly on macOS without Docker. A temporary copy of the Q2 credential
set was used and deleted immediately after shutdown.

| Request | HTTP | Assertion |
|---|---:|---|
| Models catalog | 200 | `claude-opus-4-8` present |
| Exact normal chat | 200 | exact `PONG_OK` |
| Rust coding task | 200 | complete `fn clamp_i32`, 1,449 text characters |
| Structured output | 200 | valid `{"name":"Alice","score":7}` |
| Client-defined tool named `code_execution` | 200 | one matching `tool_use`, `stop_reason=tool_use` |
| WebSearch server tool | 200 | `server_tool_use`, result, cited text |
| Four `code_execution_*` server-tool versions | 400 | Bedrock-shaped unsupported error; no private runtime text |

Only credential #3 was enabled. It recorded four successful model calls, zero
failures, and zero refresh failures; credentials #1 and #2 stayed disabled.
Both local listeners were closed and the temporary credential directory was
removed after the run.

## Repository gates

- `cargo fmt --all -- --check`: passed
- `cargo test --all-targets`: 476 passed, 0 failed, 1 diagnostic ignored
- `cargo build --release`: passed
- `cargo clippy --all-targets`: exit 0; existing repository warnings remain
- `admin-ui` `pnpm build`: passed
- CCTest runner mock submit/resume, history append, overwrite guard, timeout
  guard, response echo guard, and secret scans: passed

## Pending gate

The baseline score is not known until CCTest recovers and this task reaches a
terminal state. Resume it with `scripts/run-cctest.sh --resume` rather than
submitting a new paid check.
