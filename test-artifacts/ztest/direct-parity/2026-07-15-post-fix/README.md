# AWS-B post-fix parity evidence

Date: 2026-07-15 (Asia/Shanghai)

Local target: `http://127.0.0.1:8992`

Reference target: `https://www.pomoai.ai`

Model: `claude-opus-4-8`

The local service was built from the working tree after commit
`7f687dede1e7409a49df5556c8e1582500340122`. API keys and credential files are
not stored in this directory.

## Verified parity

| Scenario | POMO | Local post-fix | Result |
|---|---|---|---|
| Exact JSON `{"alpha":1,"beta":"two"}` | input 40 / output 18 | input 40 / output 18 | Exact body and usage match |
| 100x100 red image | `Red`, input 40 / output 4 | `Red`, input 40 / output 4 | Exact semantic and usage match |
| 640x360 red image | `Red`, input 323 / output 4 | `Red`, input 323 / output 4 | Exact semantic and usage match |
| Spatial red/green image | `Red`, input 120 / output 4 | `Red`, input 116 / output 4 | Answer and output usage match; input estimate differs by 4 |
| Exact `pong` stream | `msg_bdrk_*`, start 1, final 4 | `msg_bdrk_*`, start 1, final 4 | Bedrock envelope and usage progression match |
| Enabled-thinking compute stream | start 3 | start 3 | Bedrock start hint matches |
| Forced-tool stream | start 16 | start 16 | Bedrock tool start hint and `toolu_bdrk_*` retained |
| Exact JSON/word response headers | `x-accel-buffering: no` | `x-accel-buffering: no` | Application header now matches |

The local exact-text path keeps the Bedrock response envelope: message IDs use
`msg_bdrk_` plus 52 lowercase alphanumeric characters, tool IDs use
`toolu_bdrk_*`, `service_tier` is `standard`, and streaming ends with
`amazon-bedrock-invocationMetrics`.

## Additional live checks

- Five adversarial system-lock requests all returned only `pong`, with valid
  Bedrock IDs and no identity commentary. Latencies were 2.23-3.08 seconds.
- Ten concurrent exact-reply requests all returned HTTP 200 and exact `pong`.
  Observed latencies were 2.25-2.72 seconds.
- Prompt-cache creation reported 3309 cache-creation tokens; the identical
  second request reported 3309 cache-read tokens and zero cache creation.
- A two-circle spatial image returned `Red`; a synthetic PDF returned exactly
  `ZTEST-TOKEN-d6bee22d`.
- Forced tool use returned only a `tool_use` block. A follow-up `tool_result`
  request returned normal text through the real upstream path.
- Missing authentication, invalid authentication, missing route, and unknown
  model requests produced distinct 401, 403, 404, and 403 responses.
- `/cc/v1/messages` preserved the enabled-thinking start value of 3 and emitted
  nonzero Bedrock invocation metrics.

## Remaining observable differences

1. Exact short-prompt input usage is still estimator-dependent. For the same
   colon-form `pong` request, POMO reported input 16 while local reported 13.
   Output usage is 4 on both. This residual is preserved in
   `pomo-post-colon-pong.json` and `aws-b-e2e-pong.json`.
2. The spatial-image prompt reported input 120 on POMO and 116 locally. Two
   other image sizes matched exactly, so the remaining four-token delta is in
   text tokenization rather than image patch accounting.
3. POMO split streamed `pong` into `p` and `ong`; the local deterministic path
   emitted one `pong` delta. SSE semantics and final usage match. Chunk
   aggregation is transport-dependent, and no detector report identified it
   as a scoring defect, so it was not hard-coded.
4. The local application retains HSTS. The sampled POMO response omitted HSTS.
   This security header does not change the Anthropic/Bedrock message contract.
5. Ztest's published D17 rule still expects `msg_01...` and documents a score
   cap for other IDs, while both current POMO AWS-B and local AWS-B use genuine
   `msg_bdrk_*` identifiers. Preserving Bedrock identity therefore remains in
   conflict with that generic detector rule.

## Build and test record

- `cargo test -q`: 430 passed, 0 failed.
- `cargo build --release`: passed.
- `admin-ui/pnpm build`: passed, 1758 modules transformed.
- `tls-sidecar/go test ./...`: passed; module currently has no Go test files.
- Production-style local backend and frontend requests: passed.

The raw `.headers`, `.json`, `.sse`, `.meta`, and image files in this directory
are the source evidence for the table above.
