# Q2 aff5e6 exact ZTest replay

Replay of all 38 request bodies captured from ZTest report
`01KXJN3GC0HMBT9HP95PED488A` against the deployed `aff5e6` Q2 image.

- 35 requests returned `200`.
- The three negative probes returned their expected `500`, `403`, and `404`.
- Exact raw SSE/JSON responses and headers are under `responses/`.
- `decoded.jsonl`, `decoded.json`, and `decoded.tsv` provide normalized views.
- `pomo-reference/` retains ten matching requests replayed against POMO for
  direct text, usage, stop-reason, and content-block comparison.

This is intentionally pre-fix evidence. It captured the structured-identity
value mismatch, the one-character strict-JSON mismatch, and stochastic extra
identity commentary on repeated `ping` requests that motivated the next code
change.
