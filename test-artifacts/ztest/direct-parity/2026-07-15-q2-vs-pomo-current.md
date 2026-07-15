# Q2 versus current POMO Bedrock parity (2026-07-15)

## Scope

The same live-suite requests were sent through the public HTTPS endpoints to:

- Q2 AWS-B, deployed commit `8630f64`, using only the newly uploaded IdC
  credential.
- Current POMO AWS-B, used as the external reference.

The request fixtures are byte-identical between both runs. API keys are not
stored in either artifact directory.

Raw evidence:

- `2026-07-15-q2-deployed-8630f6-new-credential-live-suite-rerun/`
- `2026-07-15-pomo-current-live-suite-rerun/`

Each directory contains the exact request JSON, response headers, response
JSON or SSE bytes, curl status, elapsed time, and assertion table.

## Outcome

Both targets returned the expected HTTP status for all 29 requests and passed
all 21 behavioral assertions. This includes:

- authentication and malformed/unsupported request handling;
- deterministic text and JSON output;
- buffered and streaming responses;
- Bedrock message and tool-use ID shapes;
- thinking request handling;
- tool use and tool-result history;
- prompt-cache creation/read transitions;
- image and spatial input;
- identity sanitization;
- ten simultaneous exact-response requests.

The Q2 credential state was checked after the run: credential 1 remained
manually disabled, credential 2 was the only available/current credential,
and only credential 2's success counter and last-used time advanced.

## Exact usage comparison

| Request | Q2 input | POMO input | Q2 cache create/read | POMO cache create/read | Q2 output | POMO output |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| exact pong | 16 | 16 | 0 / 0 | 0 / 0 | 4 | 4 |
| exact JSON | 40 | 40 | 0 / 0 | 0 / 0 | 18 | 18 |
| tool use | 564 | 564 | 0 / 0 | 0 / 0 | 58 | 58 |
| identity JSON | 61 | 61 | 0 / 0 | 0 / 0 | 43 | 43 |
| adversarial system | 50 | 62 | 0 / 0 | 0 / 0 | 4 | 4 |
| cache, first call | 18 | 18 | 3376 / 0 | 3404 / 0 | 9 | 9 |
| cache, second call | 18 | 18 | 0 / 3376 | 0 / 3404 | 9 | 9 |
| image | 40 | 40 | 0 / 0 | 0 / 0 | 4 | 4 |
| spatial image | 111 | 112 | 0 / 0 | 0 / 0 | 4 | 4 |
| tool result | 116 | 116 | 0 / 0 | 0 / 0 | 15 | 19 |

The tool-result output difference is model wording, not an envelope mismatch:
Q2 returned `Paris is currently 18 C and clear.` while POMO returned `The
current weather in Paris is 18 C and clear.` Both reported `end_turn`; the
different output token count is consistent with the different text.

## Confirmed protocol drift

1. **Cache prefix accounting:** the same request SHA-256
   `b682b30ccb70762f14298f50d90966ddd86c69c127cf1aabd6a545ad62b6df62`
   reports 28 fewer cached tokens on Q2 (`3376` versus `3404`). Cache state
   transitions and token conservation are otherwise correct.
2. **System-prompt accounting:** the byte-identical adversarial-system request
   reports 12 fewer input tokens on Q2 (`50` versus `62`). Output and all other
   envelope fields match.
3. **Spatial media accounting:** the byte-identical request reports one fewer
   input token on Q2 (`111` versus `112`). The answer is correct on both.
4. **Malformed JSON wording:** Q2 exposes Axum/Serde wording (`Failed to
   deserialize ... expected i32`); POMO exposes Go wording (`json: cannot
   unmarshal ... ClaudeRequest.max_tokens of type uint`). Status, content type,
   request-ID suffix, and public error shape are otherwise compatible.
5. **Public proxy headers:** Q2 adds `Strict-Transport-Security:
   max-age=31536000`; POMO does not currently return that header. POMO
   occasionally adds `Vary: Accept-Encoding`. These are deployment-layer
   differences rather than model protocol fields.
6. **Model catalog:** Q2 intentionally exposes its ten supported Bedrock
   aliases in the existing `object/created/owned_by/supported_endpoint_types`
   schema. Current POMO exposes thirteen mixed models in a
   `type/created_at/display_name` schema. Adding unsupported POMO catalog
   entries to Q2 would violate the requirement to retain AWS-B behavior, so
   this is recorded rather than copied blindly.

## Non-actionable variation

Natural-language wording, SSE chunk boundaries, invocation latency, and
generated request/message/tool IDs varied between runs as expected. Their
required shapes and accounting invariants matched. The fresh ZTest report is
the authority for deciding which confirmed drift affects the composite score.
