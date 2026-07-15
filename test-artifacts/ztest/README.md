# Ztest compatibility evidence

This directory preserves Ztest evidence used while comparing the AWS-B gateway
with the AWS-P/POMO-compatible behavior. API keys are intentionally not stored.

## Current authoritative result (2026-07-16)

Two fresh, user-authorized full sequential runs used all 23 probes and ZTest
engine `v2.0.0`:

| Target | Report ID | Score | Risk | Cap |
|---|---|---:|---|---|
| Q2 AWS-B, deployed commit `2663aa1` | `01KXKBDNJ8NE301VGJGKPRZJKG` | **97** | low | none |
| POMO official | `01KXKCA8K1FYQZGHSR300VYWW2` | **40** | high | `d17_signature_invalid` at 40 |

Q2 was equal to or better than POMO on every applicable probe. Its only
non-full scoring results were D3 `94`, D7 `60`, and S1 `80`; POMO scored `94`,
`60`, and `60` on those same probes. Q2 received 100 for D17 while retaining
the visible `bdrk` marker in its detector-compatible message ID.

Both reports claimed D7 `raw_arguments={}`. Raw evidence disproves that claim:
the exact Q2 packet-captured stream and a same-nonce POMO direct replay both
reconstruct to valid, exact `get_weather` arguments. This is current ZTest
aggregation behavior affecting the reference as well as Q2, so no protocol
change was made for D7.

Primary comparison:

`reports/2026-07-16-q2-non-full-probe-coverage.md`

Complete raw reports:

- `reports/2026-07-16-q2-ztest-01KXKBDNJ8NE301VGJGKPRZJKG/`
- `reports/2026-07-16-pomo-ztest-01KXKCA8K1FYQZGHSR300VYWW2/`

Sections below preserve the investigation history. Statements that a fresh
post-deployment report was still required are superseded by the result above.

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

The old Q2 run reports two score caps but does not name the cap rules. A current
run now confirms the causal link rather than leaving it as historical
correlation.

## Current detector evidence (2026-07-15)

Report `01KXJN3GC0HMBT9HP95PED488A` retested Q2 against engine v2.0.0. Its
preserved probe details, 38 exact request bodies, packet-capture index, and
same-request POMO replays are under
`reports/2026-07-15-q2-ztest-01KXJN3GC0HMBT9HP95PED488A/`.

The report again scored 40 and explicitly named `d17_signature_invalid` as the
hard-cap rule. D17 observed a 61-character `msg_bdrk_...` identifier and
expected `^msg_[A-Za-z0-9]{18,40}$`. This proves that the current detector
runtime applies the documented generic Anthropic rule to this Bedrock gateway.

The public report API had already expired when its top-level response was
saved, so `report.raw.json` preserves that API error verbatim. The complete
non-full probe JSON was recovered separately before the page data disappeared
and is preserved in `non-full-probe-details.json`; a fresh post-deployment
report still must be captured immediately.

## Detector contract and compatibility resolution

Ztest's current public scoring-rules page still documents all of the following:

- D17 allocates 25 points to the response ID format.
- An Anthropic response ID is expected to look like `msg_01...`.
- `d17_signature_invalid` applies a hard composite-score cap of 40 when the
  ID, stop reason, or finish reason is considered invalid.

Direct POMO AWS-B calls contradict that expectation: every sampled
Opus 4.8 non-stream and stream response used a 61-character Bedrock identifier
of the form `msg_bdrk_<52 lowercase alphanumeric characters>`. Q2 uses the same
shape. That creates a real detector-contract conflict; the original Q2 ID was
not an inaccurate imitation of POMO.

The required product behavior is both a high detector score and a visibly
Bedrock-specific ID. The post-report implementation therefore uses
`msg_01bdrk<18 Base62 characters>`:

- the suffix after `msg_` is 24 alphanumeric characters and passes the current
  D17 regex;
- the literal `bdrk` marker remains visible in every message ID;
- Bedrock usage fields, signatures, event-stream metrics, tool behavior,
  credential selection, and cache semantics are unchanged.

`direct-parity/2026-07-15-q2-local-d17-compatible/` is a real local HTTP run of
the resulting binary. It passed all 20 assertions, including non-stream and
stream responses, tools, cache creation/read, images, identity cleaning, and
ten concurrent requests. This is not yet proof of a higher public score; the
new immutable image must be deployed and a fresh Ztest report captured.

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

## Deployed Q2 replay (2026-07-15)

`direct-parity/2026-07-15-q2-deployed-e9fa9b/` contains the complete live suite
against the immutable Q2 deployment. The matching POMO reference is in
`direct-parity/2026-07-15-pomo-live-suite-reference/`, and the next local
token/latency fix is in `direct-parity/2026-07-15-local-next-fix/`.

The deployed gateway, deterministic responses, streams, cache, and concurrency
worked. Complex model-backed calls exposed a separate infrastructure blocker:
AWS temporarily restricted Q2's sole upstream account and returned 429, which
the gateway surfaced as 502. A fresh usable credential is required before a
complex-call or Ztest result can be interpreted as a protocol-quality score.

## Capturing a new report

Run the report capture immediately after Ztest produces a report URL, because
the public report API expires its data after a short interval:

```bash
scripts/capture-ztest-report.sh \
  https://ztest.ai/report/REPORT_ID
```

The script does not accept or persist an API key. It saves the byte-for-byte
report API response, its SHA-256, normalized report data, every non-full probe,
all exception details, JSON/TSV anomaly indexes, and a Markdown summary. It
also writes one exact normalized object per probe under `probes/`, with every
non-full probe and its complete `details` value duplicated under
`non-full-probe-json/` and `non-full-detail-json/` for direct inspection. An
already saved response can be replayed without network access:

```bash
scripts/capture-ztest-report.sh \
  --input test-artifacts/ztest/reports/RUN/report.raw.json \
  /tmp/ztest-report-replay
```

When ZTest opens reports in a short-lived browser popup, start the watcher
before submitting the test:

```bash
scripts/watch-ztest-reports.sh
```

The watcher reads only new Codex desktop log lines by default. It extracts
strict `https://ztest.ai/report/<26-character ULID>` URLs, immediately invokes
the report capture script, retries transient fetch failures, and records only
processed report IDs in its state file. It does not persist source log lines,
browser storage, API keys, or request credentials. Use `--scan-existing` only
when deliberately recovering still-live report URLs from existing log data.
