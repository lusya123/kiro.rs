# Local live replay after parity fixes

All 38 captured ZTest request bodies were sent to a real local release process
after the structured-identity, strict-JSON, and standalone-ping fixes.

- 30 requests returned `200`.
- The three negative probes retained their expected `500`, `403`, and `404`.
- Five upstream-dependent requests returned `502` after Kiro rejected all six
  retries with a temporary suspicious-activity `429` limit on the old local
  credential. The upstream account identifier is redacted in this artifact.
- All deterministic fixed paths passed: identity `125/55`, strict JSON
  `115/30`, and five repeated pings each returned only `pong` with `32/4`.

The raw headers and bodies are retained. `results.tsv` records timing/status,
and the decoded files provide normalized SSE and error views.
