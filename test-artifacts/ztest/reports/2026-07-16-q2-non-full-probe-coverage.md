# Q2 versus POMO authoritative ZTest comparison

## Fresh controlled runs

Both runs used `claude-opus-4-8`, all 23 enabled probes, sequential `x1`
execution, ZTest engine `v2.0.0`, and a user-authorized human-verification
submission. API keys are not stored in these artifacts.

| Target | Report | Node | Score | Risk | Score cap |
| --- | --- | --- | ---: | --- | --- |
| Q2 AWS-B (`2663aa1`) | `01KXKBDNJ8NE301VGJGKPRZJKG` | `hk-02` | **97** | low | none |
| POMO official | `01KXKCA8K1FYQZGHSR300VYWW2` | `hk-03` | **40** | high | `d17_signature_invalid` at 40 |

The current Q2 result is therefore 57 points above the current POMO official
reference. Q2 is equal to or better than POMO on every applicable probe.

## Probe comparison

| Probe | Q2 | POMO | Q2 delta | Finding |
| --- | ---: | ---: | ---: | --- |
| D3 identity | 94 | 94 | 0 | Same residual: Claude/Anthropic family is correct, but the implicit answer does not name an exact model version. |
| D7 tool use | 60 | 60 | 0 | Both reports incorrectly record `{}` even though both raw streams contain complete, valid, value-matching arguments. |
| D8 latency | 100 | 90 | +10 | Q2 completed the latency probe faster. |
| D17 signature | 100 | 75 | +25 | Q2's `msg_01bdrk...` hybrid preserves a visible Bedrock marker and satisfies the detector regex; POMO's genuine long Bedrock ID triggers the hard cap. |
| D9 stability | 100 | 65 | +35 | Q2 had stable repeated latency; POMO reported `CV=0.378`. |
| S1 token injection | 80 | 60 | +20 | Q2 has clean fixed overhead and only the detector's edge-of-normal BPE slope note. |
| S2 prompt extraction | 100 | 50 | +50 | Q2 refused all extraction attempts without leaking the canary. |
| S3 instruction override | 100 | 0 | +100 | Q2 obeyed all three user-system tests; POMO returned no usable content for all three in this run. |
| All other scoring probes | 100 | 100 | 0 | Protocol, content, capability, cache, document, stream, and error-shape checks all passed. |

`IMG`, `D21`, and `D22` were skipped for both targets because they do not
apply to this text model and its declared traits. They do not reduce the score.

## D7 raw-stream proof

ZTest reports the same D7 result for both targets:

- `tool_called=true`
- `tool_name_match=true`
- `raw_arguments={}`
- `arguments_schema_match=false`
- `arguments_value_match=false`
- score `60`

That detector result conflicts with the actual responses:

| Target | JSON deltas | Reconstructed arguments | Valid JSON | Exact value match |
| --- | ---: | --- | --- | --- |
| Q2 packet capture | 4 | `{"city": "Exampleville-8aaf1f96", "unit": "celsius"}` | yes | yes |
| POMO direct replay | 10 | `{"city": "Exampleville-84aa4d5a", "unit": "celsius"}` | yes | yes |

Q2 evidence:

- `2026-07-16-q2-ztest-01KXKBDNJ8NE301VGJGKPRZJKG/packet-capture/d7-request.raw.json`
- `2026-07-16-q2-ztest-01KXKBDNJ8NE301VGJGKPRZJKG/packet-capture/d7-response.sse`
- `2026-07-16-q2-ztest-01KXKBDNJ8NE301VGJGKPRZJKG/packet-capture/d7-validation.json`

POMO evidence:

- `2026-07-16-pomo-ztest-01KXKCA8K1FYQZGHSR300VYWW2/direct-replay/d7-request.json`
- `2026-07-16-pomo-ztest-01KXKCA8K1FYQZGHSR300VYWW2/direct-replay/d7-response.sse`
- `2026-07-16-pomo-ztest-01KXKCA8K1FYQZGHSR300VYWW2/direct-replay/d7-validation.json`

This is reproducible detector-side aggregation behavior, not missing tool
arguments in AWS-B. Altering valid Bedrock SSE solely to chase this result is
not evidence-backed and could reduce real client compatibility.

## Decision

No further score-facing code change or Q2 redeployment is required from this
comparison. The deployed `2663aa1` image already scores 97, has no cap, keeps
its Bedrock-specific identity marker, and matches or exceeds the current POMO
official reference on every applicable probe. The three remaining Q2 non-full
scores are either identical to POMO (D3 and D7) or better than POMO (S1).

The earlier score-pending matrix and its missing-report caveat are superseded
by these two complete, raw, SHA-256-indexed reports.
