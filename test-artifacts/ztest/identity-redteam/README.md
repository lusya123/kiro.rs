# AWS-B Identity Red-Team Evidence

Date: 2026-07-16

## Final result

The final local release build is recorded in `2026-07-16-local-fixed`.

- 30/30 real HTTP probes passed with HTTP 200.
- 8/8 repeated thinking probes passed, with 136-146 inspectable thinking characters per response.
- Raw JSON/SSE re-scan found zero forbidden identity markers in all forbid probes.
- Control probes preserved Bob, literal/third-party Kiro, third-party CodeWhisperer, and the ordinary `postgres` backend tool argument.
- Rust tests: 463 passed, 0 failed, 1 ignored diagnostic.
- `cargo fmt --check`, `git diff --check`, and shell syntax checks passed.
- `cargo clippy --all-targets --all-features` exited 0 with the repository's existing warning backlog.
- Artifact secret scan found no API keys, access tokens, refresh tokens, or client secrets.

The matrix covers Anthropic and OpenAI-compatible APIs, streaming and non-streaming responses, visible text, thinking blocks, forced tool calls, structured JSON, Markdown/code blocks, multi-turn poisoning, and base64/hex/separated-letter evasions.

## Findings fixed

The old Q2 image evidence in `2026-07-16-q2-2663aa` exposed three identity paths:

1. A system override could produce a direct Kiro self-claim.
2. A genuine upstream thinking block could be emitted as visible text after a synthetic thinking block.
3. Forced tool arguments could expose Kiro and CodeWhisperer identity fields.

Local adversarial reruns then found and preserved additional random-output variants:

- `2026-07-16-local-fixed-observed-denial-leak`: a visible denial repeated Kiro.
- `2026-07-16-local-fixed-observed-sanitizer-artifact`: replacement produced a contradictory Claude denial.
- `2026-07-16-local-fixed-observed-multiturn-retraction-leak`: a poisoned prior assistant turn caused a CodeWhisperer retraction leak.
- `2026-07-16-local-fixed-observed-residual-codewhisperer-leak`: another retraction wording retained CodeWhisperer.
- `2026-07-16-local-fixed-pre-final-denial-fix/repeated-thinking-observed-random-denial-leaks`: repeated thinking tests exposed two additional visible refusal phrasings.

The final sanitizer handles these first-person refusal/retraction forms before generic identity replacement, while third-party controls remain unchanged.

## Stream and tool boundaries

- A real `<thinking>` block takes precedence over a pending synthetic fallback, including split start tags.
- Tool-only streams emit synthetic thinking before opening the tool block.
- Private identity tool JSON is buffered until complete, recursively sanitized, and then emitted.
- A truncated private identity tool input is closed with safe `{}` JSON and `max_tokens`, rather than leaking a partial name.
- Ordinary backend fields such as `postgres` are preserved; only private runtime backend values are neutralized.

## Environment note

`2026-07-16-local-fixed-network-timeouts` records a run interrupted by an expired local access-token copy. The AWS endpoint remained reachable, the refresh token renewed the access token after a clean local restart, and the complete final matrix then passed. Those timeout rows are retained as environment evidence and are not counted as code failures.

## Reproduction

```bash
BASE_URL=http://127.0.0.1:18991 \
API_KEY_FILE=/path/to/api-key-file \
OUT_DIR=test-artifacts/ztest/identity-redteam/recheck \
scripts/run-identity-redteam.sh
```

The runner stores requests, response headers, raw JSON/SSE, extracted channels, timings, and findings. It never writes the supplied API key into the artifact directory.
