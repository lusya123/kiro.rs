# Ztest compatibility evidence

This directory preserves Ztest evidence used while comparing the AWS-B gateway
with the AWS-P/POMO-compatible behavior. API keys are intentionally not stored.

## Historical reports

| Target | Report ID | Score | Preserved report | SHA-256 |
|---|---|---:|---|---|
| Q2 AWS-B | `01KX97F11NQE72DSS59RJHMCQV` | 40 | `history/2026-07-12-q2-aws-b-score-40.md` | `98ec9d00ba04071bf32dbb0b9824daaf5010753a670cc005c6eb6a9ed7fcc2f8` |
| quietfox AWS-P | `01KX98ARYFN9TPNCX17GC98GRC` | 93 | `history/2026-07-12-quietfox-aws-p-score-93.md` | `0d053ed2b0a165df1701c5f09b189a1f78d5ad413da8336e84f4d3d57c50508a` |

The preserved Markdown files contain every probe result and the complete raw
JSON exported by Ztest. They are copied byte-for-byte from the downloaded
reports rather than reconstructed from screenshots.

## Material historical differences

| Probe | Q2 AWS-B | AWS-P | Evidence-backed difference |
|---|---:|---:|---|
| D13 multimodal | 80 | 100 | Q2 failed one spatial image question; its raw answer said only the green/right circle was visible, while the expected left-circle color was white. |
| D17 response signature | 75 | 100 | Q2 emitted `msg_bdrk_` plus 52 characters. Ztest v1 expected `^msg_[A-Za-z0-9]{18,40}$`; AWS-P emitted a matching 24-character suffix. |
| D2 response structure | 85 | 100 | The same Q2 message-ID mismatch reduced protocol scoring. |
| D8 latency | 30 | 55 | Q2 took 7.61 s (3.46x baseline); AWS-P took 4.70 s (2.13x baseline). |
| D9 stability | 85 | 85 | Q2 answers sometimes appended identity/injection commentary after `pong`, producing zero exact-answer consistency despite a passing probe score. |
| S1 token injection | 80 | 80 | Both had the same non-full score; this does not explain the 53-point gap. |
| D3 identity | 88 | 88 | Both identified as Claude/Anthropic but not the exact model version; this also does not explain the gap. |

The old Q2 run reports two score caps but does not name the cap rules. The
strongest correlated evidence is the nonstandard outer Anthropic message ID:
it independently reduced D17 and D2, whereas the AWS-P report received full
scores for both. This is a historical correlation, not proof that the current
Ztest engine applies the same cap.

## Current-baseline rule

Do not change Bedrock-specific behavior merely to satisfy this historical v1
regex. First run the current Q2 build against the current Ztest engine, preserve
the new report and raw JSON here, and only then make evidence-driven changes.
Bedrock transport, credential handling, account selection, cache behavior, and
AWS event-stream semantics remain required AWS-B properties.

## Current detector conflict (2026-07-15)

Ztest's current public scoring-rules page still documents all of the following:

- D17 allocates 25 points to the response ID format.
- An Anthropic response ID is expected to look like `msg_01...`.
- `d17_signature_invalid` applies a hard composite-score cap of 40 when the
  ID, stop reason, or finish reason is considered invalid.

Current direct POMO AWS-B calls contradict that expectation: every sampled
Opus 4.8 non-stream and stream response used a 61-character Bedrock identifier
of the form `msg_bdrk_<52 lowercase alphanumeric characters>`. Q2 uses the same
shape. Therefore a detector run that applies the documented generic Anthropic
regex to a real Bedrock gateway cannot simultaneously award a score above 40
and preserve the genuine Bedrock identifier.

This is a detector-contract conflict, not evidence that Q2's Bedrock ID is
incorrect. Three current live runs completed all 38 HTTP interactions, but the
hidden in-app browser discarded each report popup before its ID could be
retained. Their request-level execution evidence is preserved in
`2026-07-15-live-baseline-attempts.md`; a captured report is still required to
confirm whether Ztest's runtime implements the public rule exactly as
documented.

## Direct POMO/Q2 samples

`direct-parity/2026-07-15-pre-ztest/` contains the raw response headers, JSON,
and SSE captured from same-request POMO and Q2 calls. No request API keys are
stored. Representative results include:

- Exact `pong`: both returned only `pong` and a `msg_bdrk_...` ID in five
  consecutive calls. POMO reported usage 15/4; Q2 reported 12/8.
- Forced two-field tool call: content, stop reason, and usage matched exactly
  at input 564/output 58; only random IDs and latency differed.
- Adaptive-thinking stream: both exposed text blocks only and Bedrock
  invocation metrics. Event structure matched; generated text, token counts,
  delta boundaries, and timing were naturally nondeterministic.
- Prompt cache: both demonstrated cache-creation and cache-read fields in the
  preserved samples. POMO cache reuse varied across routed calls, while Q2's
  shared virtual cache produced deterministic second-call reads.

## Post-fix local/POMO replay

`direct-parity/2026-07-15-post-fix/` contains the raw responses from the latest
release build plus same-request POMO references. Its `README.md` records both
matches and residual differences. Confirmed improvements include:

- Deterministic exact text no longer appends identity commentary, while keeping
  `msg_bdrk_*` and the Bedrock response envelope.
- Short text output accounting now matches POMO (`pong` 4, `4` 3, `Red` 4).
- Same-request JSON output matches exactly at input 40/output 18.
- Two same-request image sizes match exactly at 40/4 and 323/4; the spatial
  image answer is now correct, with only a four-token text-estimator residual.
- Stream starts match the sampled Bedrock profile: normal text 1,
  enabled-thinking compute 3, and forced tool use 16.
- The application now emits `x-accel-buffering: no` for the AWS-B profile.

This is not proof of a current Ztest score. A captured post-deployment report
is still required, and the detector's documented `msg_01...` requirement
continues to conflict with the genuine POMO AWS-B `msg_bdrk_*` identifier.
