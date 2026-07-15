# Q2 2663aa public E2E evidence

- Target: `https://q2.quietfox.sbs`
- Branch/commit: `aws-b` / `2663aa137d42d4075d6078fdbd02cda5f6627fc5`
- Image: `ghcr.io/lusya123/kiro-rs:aws-b-beta-2663aa@sha256:c1f210838d50d21d00dac08e1d4c50339ef8c360039c7cf94cc8df8b92eae78d`
- Model: `claude-opus-4-8`
- Secrets: omitted or replaced with `[REDACTED]`

## Scope

The smoke run exercises the deployed admin credential state, model catalog,
HTTP/2 ingress, and a real upstream forced-tool stream. All 13 smoke
assertions passed.

The replay sends all 38 exact request bodies recovered from ZTest report
`01KXJN3GC0HMBT9HP95PED488A` to the public Q2 endpoint. It preserves each
request, response header set, raw JSON/SSE body, timing record, and SHA-256.

## Results

- 38/38 requests completed over HTTP/2 with no curl failure.
- Expected statuses matched: 35 x `200`, one `403`, one `404`, and one `500`.
- 35/35 successful streams ended with `message_stop` and valid event order.
- 30/30 content and protocol assertions passed.
- All successful IDs retain the detector-compatible Bedrock marker
  `msg_01bdrk...` and all successful model fields are `claude-opus-4-8`.
- Exact deterministic POMO parity is preserved for structured identity,
  strict JSON, context-window, ping, and the saved forced-tool reference.

The main machine-readable summaries are `smoke-assertions.json`,
`ztest-replay/summary.json`, `ztest-replay/assertions.json`,
`ztest-replay/content-summary.json`, and
`ztest-replay/pomo-comparison.json`.

## Limit

This directory proves deployed request/response behavior for the recovered
probe corpus. It does not prove a new public ZTest composite score. A fresh
report URL and its `/api/reports/<id>` JSON must still be captured immediately
after a normal user-tab submission.
