# Bedrock token calibration evidence (2026-07-15)

Reference: `https://www.pomoai.ai`, model `claude-opus-4-8`.

Target for the first two matrices: local AWS-B at commit `3237aaf`. API keys are
never written to these artifacts. Each matrix keeps the exact request JSON,
response headers, response body, HTTP status, curl exit code, and wall time.

## Runs

| Directory | Purpose | Important outcome |
| --- | --- | --- |
| `2026-07-15-token-calibration-matrix` | Baseline cache, system, conversation, and tool history | Local cache prefixes were 49-79 tokens low. Local tool-history calls reached the upstream account restriction and returned HTTP 502 with the original Kiro suspicious-activity 429 preserved in the body. |
| `2026-07-15-token-calibration-matrix-2` | More system and one-tool history variants | POMO references captured; local tool-history calls again returned the same upstream restriction. |
| `2026-07-15-token-calibration-matrix-3` | Prompt length, 0-4 input fields, long result/name, and 2-3 tool pairs | All 11 POMO calls returned HTTP 200. This isolated structural tool-history billing. |
| `2026-07-15-token-calibration-matrix-4` | Long and nested values with one top-level input field | POMO returned 69, 95, and 105 input tokens, separating field framing from serialized input size. |
| `2026-07-15-token-calibration-matrix-5` | Existing system and three-turn regression guards | POMO returned 30 and 36, exactly matching the pre-existing AWS-B tests. No global system or conversation correction was justified. |
| `2026-07-15-token-calibration-matrix-6` | Tool history plus a complex schema | POMO returned 116 without the schema and 523 with it. |
| `2026-07-15-token-calibration-matrix-7` | First schema-combination attempt | The first duplicate baseline wrote a complete HTTP 200 response (`Content-Length: 518`, input usage 69), but curl did not exit for more than two minutes and was manually interrupted. `results.tsv` therefore contains only its header and the next request never started. The complete body/headers and zero-byte next-request header file are retained as the raw connection anomaly. |
| `2026-07-15-token-calibration-matrix-7b` | Retry with only the missing schema combinations | POMO returned 426 for one simple schema and 615 for two schemas. |
| `2026-07-15-token-calibration-matrix-8-live-e2e` | Same exact-reply tool-history requests against POMO and the rebuilt local HTTP service | POMO returned 73 and 430. Local reached the real upstream path but returned HTTP 502 because the old IdC account was still under Kiro's suspicious-activity restriction; raw errors are retained. |
| `2026-07-15-token-calibration-matrix-9-sequential-tools` | Two sequential one-block tool rounds | POMO returned 140, proving sequential and parallel tool histories use different framing. |

## Confirmed reference values

| Request shape | POMO input tokens |
| --- | ---: |
| One tool pair, empty input | 46 |
| One tool pair, one simple field | 69 |
| One tool pair, two simple fields | 94 |
| One tool pair, long input value | 95 |
| One tool pair, nested input value | 105 |
| Two tool pairs | 179 |
| Three tool pairs | 260 |
| One simple schema plus history | 426 |
| One complex schema plus history | 523 |
| Two schemas plus history | 615 |
| Exact-reply tool history | 73 |
| Exact-reply tool history plus one simple schema | 430 |
| Two sequential tool pairs | 140 |

## Implemented model

Single-tool history uses ordinary message tokens plus top-level field framing,
canonical input JSON tokens, tool-name tokens, and tool-result tokens. The
captured single-tool matrix is within two tokens of every POMO reference.

Parallel tool pairs use canonical complete block JSON plus Bedrock block
framing; the two- and three-tool references are exact. Sequential one-block
tool rounds reuse the single-tool structural model with a five-token framing
increment for every additional pair; the two-round reference is exact at 140.

When schemas are repeated on a history request, the shared estimator's fixed
454-token Opus overhead is removed. The history schema envelope is then 327
tokens for the first tool, 44 for every subsequent tool, plus 0.93 times the
visible schema tokens. This reproduces 426, 523, and 615 exactly while leaving
the initial complex-tool request at its established 564-token value.
