# Q2 ZTest submission and report evidence

## Scope

- Target: `https://q2.quietfox.sbs`
- Model: `claude-opus-4-8`
- Detection mode: full (`23` enabled probes, `19` scoring probes)
- Concurrency mode: sequential (`x1`)
- ZTest scoring engine: `v2.0.0`
- Submission authorization: explicitly confirmed by the user
- Human verification: Cloudflare Turnstile completed before submission

No API key, SSH password, account credential, or request body is stored in
this note.

## Live execution evidence

Three authorized full sequential submissions reached the public Q2 endpoint:

| Run | UTC window | Asia/Shanghai window | Purpose |
| --- | --- | --- | --- |
| 1 | `2026-07-15T16:13:22Z`-`16:14:37Z` | `2026-07-16 00:13:22`-`00:14:37` | Initial controlled-browser submission |
| 2 | `2026-07-15T16:31:59Z`-`16:33:07Z` | `2026-07-16 00:31:59`-`00:33:07` | User-confirmed submission and human verification |
| 3 | `2026-07-15T16:35:44Z`-`16:36:53Z` | `2026-07-16 00:35:44`-`00:36:53` | Report-window capture retry |
| 4 | `2026-07-15T16:58:06Z`-`16:59:10Z` | `2026-07-16 00:58:06`-`00:59:10` | Final popup-capture submission; report retained |

For each run, Nginx recorded exactly `38` requests with the same distribution:

| Result | Count |
| --- | ---: |
| HTTP 200 | 35 |
| HTTP 403 | 1 |
| HTTP 404 | 1 |
| HTTP 500 | 1 |
| Total per run | 38 |

Path distribution:

| Path | Count |
| --- | ---: |
| `/v1/messages` | 37 per run |
| `/v1/messages/nonexistent` | 1 per run |

The application handler logged `37` `/v1/messages` requests per run. The
remaining request was the expected nonexistent-path router probe. Each checked
handler batch had:

- `0` error log entries
- `0` warning log entries
- `1` fake-model request
- `2` non-streaming requests

The status distribution matches the expected ZTest corpus behavior: successful
probes plus one invalid-key response, one nonexistent-path response, and one
intentional server-error response.

## Deployed Q2 fingerprint

- Container: `kiro-rs-q2`
- Container ID: `b52233195496beab2709479cdbc951f6ff138963e4a3506595ed9c661be22df2`
- Container image ID: `sha256:9c0ab36a1e17991fbddab5253cd658724668e7f189cb047cea8bed2b74d38a98`
- Immutable image: `ghcr.io/lusya123/kiro-rs:aws-b-beta-2663aa@sha256:c1f210838d50d21d00dac08e1d4c50339ef8c360039c7cf94cc8df8b92eae78d`
- State after the run: running, restart count `0`

No container, Nginx, network, firewall, or AWS-P setting was changed during
this verification.

## Final retained report

Run 4 used a sandboxed popup-capture page that let the ZTest frontend expose
its own popup-blocked fallback link without reading browser storage or
persisting the API key. Cloudflare Turnstile completed successfully before the
submission.

- Report: <https://ztest.ai/report/01KXKBDNJ8NE301VGJGKPRZJKG>
- Status: `completed`
- Composite score: `97`
- Risk: `low`
- Score caps: none
- Raw report SHA-256: `7742e0515e3131b02426167f020926d8243574383eaceac1d8ad3dfc3be1c095`
- Preserved report directory:
  `test-artifacts/ztest/reports/2026-07-16-q2-ztest-01KXKBDNJ8NE301VGJGKPRZJKG/`

The report directory contains the byte-preserved API response, every normalized
probe object, all non-full details, anomaly indexes, and the exact D7 request
and response reconstructed from the server-side packet capture. The existing
38-request replay and response assertions remain under:

`test-artifacts/ztest/direct-parity/2026-07-15-q2-2663aa-public-e2e/`
