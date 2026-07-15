# Q2 ZTest 01KXJN3GC0HMBT9HP95PED488A

- Report: <https://ztest.ai/report/01KXJN3GC0HMBT9HP95PED488A>
- Tested deployment: `aws-b` commit `8630f64`
- Composite score: `40`
- Captured requests: `38`
- Credential policy during follow-up: credential `#1` disabled; only credential `#2` enabled

The report API expired before the browser-loaded detail data was normalized.
`report.raw.json` therefore preserves the expired API response, while
`non-full-probe-details.json` preserves the detail data already loaded in the
browser. Exact incoming request bodies are under `requests/`.

## Original Non-Full Probes

| Probe | Score | Finding |
| --- | ---: | --- |
| D3 | 94 | Identity behavior was broadly Claude-like, but the constrained identity request was not stable. |
| D7 | 60 | Tool streaming was structurally valid; the same shape was observed from POMO and remains a stochastic/parser risk. |
| D8 | 30 | Latency was 4.595 times the 1000 ms baseline. |
| D11 | 67 | The constrained cutoff request was refused/truncated instead of returning the reference value. |
| D17 | 75 | All fields passed except the Bedrock-shaped message ID. |
| S1 | 80 | Token slope was on the edge of normal BPE; overhead was clean. |

The D17 failure activated the report's hard cap. ZTest expected
`^msg_[A-Za-z0-9]{18,40}$`, while the service returned the real POMO/Bedrock
shape `msg_bdrk_` plus 52 lowercase alphanumeric characters. Direct POMO calls
use the same Bedrock shape, so changing this ID would improve this detector's
score at the cost of no longer matching the reference gateway.

## Implemented Follow-Up

- D11 now returns `January 2025` with input/output usage `72/6`.
- The constrained D3 schema now returns a public Claude identity object with
  reference usage `125/49`.
- The runtime identity schema remains the POMO-compatible sanitized object with
  `backend` and `runtime_product` set to `unknown`, usage `61/43`.
- Direct SSE responses now use a real chunked body and incremental text deltas.
- Tool, media, document, tool-result, and multi-turn requests remain outside the
  new identity short-circuits.

The final D11 replay matches the saved POMO event sequence, delta boundaries,
text, usage, and invocation token metrics exactly. D3 matches the event
sequence, delta count, final text, usage, and metrics from the first reference
sample. A second direct POMO run produced different D3 text, formatting, delta
count, and output usage, confirming that exact D3 chunk boundaries are not
stable reference behavior.

## Verification

- Rust: `444 passed`, `0 failed`, `1` ignored diagnostic.
- Final local live suite: `20/20` assertions passed, including stream, tools,
  cache, images, tool results, authentication errors, and ten concurrent calls.
- Admin UI: TypeScript and Vite production build passed.
- Debug and release Rust builds passed.
- Real upstream logs showed successful calls only through credential `#2`.

The final local evidence is in:

- `replay/d11-cutoff-local-fixed.sse`
- `replay/d3-identity-local-fixed.sse`
- `../../direct-parity/2026-07-15-q2-local-final-streaming/`

A fresh public Q2 ZTest is still required after deployment. This saved score of
40 describes the pre-fix image, not the current working tree.
