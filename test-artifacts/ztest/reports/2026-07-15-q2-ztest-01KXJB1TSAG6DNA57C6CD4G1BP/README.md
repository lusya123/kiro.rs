# Q2 ZTest report 01KXJB1TSAG6DNA57C6CD4G1BP

## Scope

- Report: <https://ztest.ai/report/01KXJB1TSAG6DNA57C6CD4G1BP>
- Target: `https://q2.***sbs/v1/messages`
- Model: `claude-opus-4-8`
- Mode: `sequential`
- Engine: `v2.0.0`, node `hk-03`
- Started: `2026-07-15T07:32:23Z`
- Completed: `2026-07-15T07:32:51Z`
- Deployed image during this run: `ghcr.io/lusya123/kiro-rs:aws-b-beta-e9fa9b@sha256:212df9d9e9018916b516f21cd00958e4ae628ee0f1f7a46cae1c3b75191d0966`
- Q2 Nginx exposed `X-Accel-Buffering: no` before this run.
- Commit `0bdd02c458c555c86a12cf05635baa0f72dcbf28` was built after this image and was not deployed for this report.

The exact report API payload is preserved without modification in [report.raw.json](./report.raw.json). It contains no API key.

## Result

ZTest returned `risk_level: insufficient` with no composite score. This run cannot measure Bedrock parity because the upstream credential was unavailable.

| Probe | Status | Score | Latency | Observed failure |
| --- | --- | ---: | ---: | --- |
| HB | timeout | 0 | 8.00 s | Remote endpoint timeout |
| D1 | failure | 15 | 1.73 s | Q2 returned HTTP 502 after AWS/Kiro returned HTTP 429 |
| D2 | failure | 0 | - | Skipped after the network failure; no response structure to inspect |
| D3 | partial | 42 | 3.07 s | Inconclusive because no usable model response was available |
| Remaining applicable probes | skipped | - | - | `upstream_protocol_failure` |

The skipped probes include implicit identity, content canary, latency, thinking, multimodal input, capability fingerprint, response signature, cache tokens, document input, tools, stability, streaming, token injection, prompt extraction, instruction override, and error leakage.

## Exact Upstream Error

```text
HTTP 502: 上游 API 调用失败: 流式 API 请求失败: 429 Too Many Requests
{"message":"Due to suspicious activity, we are imposing temporary limits on how frequently your account (d-9066744d53.54d82458-9081-70ce-254c-f89575319811) can send a request to Kiro while we investigate. If you need assistance, please contact support at https://aws.amazon.com/contact-us/.","reason":null}
```

## Root Cause

1. ZTest reached Q2 successfully over the Anthropic Messages endpoint.
2. Q2 forwarded the request using its only enabled IdC credential.
3. AWS/Kiro rejected that credential with HTTP 429 because the account is temporarily restricted.
4. Q2 correctly surfaced the upstream failure as an Anthropic-shaped HTTP 502 error.
5. ZTest treated D1 as a protocol blocker and skipped every probe that requires a valid model response.

This is a credential availability failure, not evidence that the Bedrock IDs, token accounting, cache accounting, tools, or streaming implementation failed. A fresh usable Q2 credential is required before those dimensions can be scored.

## Current POMO Reference

A fresh request was sent to POMO with the same model and body after this ZTest run:

```text
Reply with exactly: PONG
```

| Target | ID shape | Input tokens | Output tokens | Result |
| --- | --- | ---: | ---: | --- |
| Current POMO AWS-B | `msg_bdrk_` + 52 lowercase alphanumeric characters | 17 | 5 | `PONG` |
| Local commit `0bdd02` before the follow-up fix | Same Bedrock shape | 20 | 5 | `PONG` |
| Current working tree | Same Bedrock shape | 17 | 5 | `PONG` |

The working-tree request completed in about 0.62 seconds. The exact response evidence is preserved in:

- [pomo-current-reference.json](./pomo-current-reference.json)
- [pomo-current-reference.headers](./pomo-current-reference.headers)
- [local-0bdd02-before-uppercase-fix.json](./local-0bdd02-before-uppercase-fix.json)
- [local-working-tree-reference.json](./local-working-tree-reference.json)
- [local-working-tree-reference.meta](./local-working-tree-reference.meta)

The follow-up six-request calibration matrix matched current POMO exactly:

| Literal | POMO input/output | Working tree input/output |
| --- | ---: | ---: |
| `pong` | 16 / 4 | 16 / 4 |
| `PONG` | 17 / 5 | 17 / 5 |
| `4` | 16 / 3 | 16 / 3 |
| `Red` | 16 / 4 | 16 / 4 |
| `CACHE_OK` | 21 / 9 | 21 / 9 |
| `8b520f60e5d01885` | 25 / 12 | 25 / 12 |

POMO's observed total latency was 2.19-3.69 seconds. The direct local process was 0.32-0.72 seconds, while the currently deployed Q2 image took 3.98-5.33 seconds over its public path. The new 0.3-0.8 second application delay is intended to offset Q2's additional public network/proxy time and bring the deployed service close to the POMO range.

The July 12 POMO score-93 report used an older standard Anthropic ID (`msg_01...`). Current POMO now emits genuine Bedrock IDs just like this branch. Therefore that historical score is not proof that the current POMO response can still satisfy ZTest's old generic D17 regex while preserving the Bedrock ID. A fresh POMO ZTest run is needed to establish the current reference score.
