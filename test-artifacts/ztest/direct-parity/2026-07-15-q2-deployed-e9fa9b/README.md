# Q2 deployed-image live evidence

Date: 2026-07-15 (Asia/Shanghai)

Target: `https://q2.quietfox.sbs`

Model: `claude-opus-4-8`

Deployed commit: `e9fa9bf8f6a8ffb892a6645e3635ce43339b6652`

Deployed image:
`ghcr.io/lusya123/kiro-rs:aws-b-beta-e9fa9b@sha256:212df9d9e9018916b516f21cd00958e4ae628ee0f1f7a46cae1c3b75191d0966`

API keys are not stored in this directory. `requests/` contains the exact JSON
sent by the live suite. `responses/` contains raw HTTP headers, JSON or SSE,
curl status, elapsed time, and concurrent-call outputs.

## Passed behavior

- Models, missing auth, invalid auth, unsupported model, and malformed JSON
  returned the expected 200/401/403/403/400 status boundaries.
- Exact `pong` and exact JSON returned HTTP 200 with Bedrock `msg_bdrk_*` IDs.
- Exact `pong` reported output usage 4; exact JSON reported output usage 18.
- Exact streaming included start usage, final usage 4, and nonzero
  `amazon-bedrock-invocationMetrics`.
- Prompt cache creation reported 3376 tokens and the identical second request
  reported 3376 cache-read tokens.
- Ten concurrent exact-`pong` calls all returned HTTP 200 and only `pong`.
- Public `/v1/messages/count_tokens` returned 404 with
  `{"error":"Not Found"}`. The same endpoint and body were observed from POMO,
  so this is an intentional AWS-B contract rather than a regression.

## Failed behavior and root cause

Thinking, forced tool use, streamed tool use, identity JSON, solid-color image,
and spatial-image requests all returned public HTTP 502. Every raw body records
the same upstream cause: AWS Kiro returned HTTP 429 and stated that the account
was temporarily limited because of suspicious activity. This is an account
restriction, not a response-conversion difference.

The Q2 admin status at the time showed one enabled IdC credential. It could
refresh, but the upstream account itself rejected generation. Credential
secrets and admin JSON are intentionally not stored here.

## Observable reference differences

- Q2 exact `pong` input usage was 13 while the same POMO request was 16.
- Q2's ten concurrent exact replies took 4.25-5.82 seconds; POMO's same-request
  samples took 2.18-3.64 seconds.
- The application origin emitted `x-accel-buffering: no`, but Q2 Nginx consumed
  the header. POMO exposed it publicly. Passing this header through requires a
  separately confirmed Nginx change.
- Q2 retains HSTS; the sampled POMO response did not.
- Malformed `max_tokens` produced Rust/Axum wording on Q2 and Go wording on
  POMO. Both were HTTP 400 with the same gateway request-ID envelope.
- POMO listed three unrelated entitlement models in addition to the common
  Claude/Bedrock catalog. This can be API-key entitlement data, so it is not
  treated as an AWS-B protocol defect.

## Follow-up local fix

The next uncommitted local build calibrates the four measured short literal
inputs to the POMO values and reduces the AWS-B application-side delay from
2.2-3.1 seconds to 0.3-0.8 seconds. Its raw verification is preserved in
`../2026-07-15-local-next-fix/`. It has not yet replaced the deployed image.
