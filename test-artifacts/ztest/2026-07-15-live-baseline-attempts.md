# Q2 live Ztest baseline attempts

Date: 2026-07-15 (Asia/Shanghai)

Target: `https://q2.quietfox.sbs`

Model: `claude-opus-4-8`

Mode: complete, parallel x3

Deployed commit: `7f687dede1e7409a49df5556c8e1582500340122`

The API key is intentionally omitted.

## Submission evidence

The Ztest form was submitted three times. Cloudflare Turnstile completed
normally after explicit user authorization, but the hidden browser automation
surface did not retain the new-window report URL returned by the site.

Q2's read-only Nginx access log proves that all three submissions reached the
service and executed complete probe batches:

| Execution node | Request window (+08:00) | 200 `/v1/messages` | 403 auth probe | 404 path probe | 500 error probe | Total |
|---|---|---:|---:|---:|---:|---:|
| `154.64.249.217` | 03:07:46-03:08:29 | 35 | 1 | 1 | 1 | 38 |
| `45.192.104.25` | 03:08:28-03:09:01 | 35 | 1 | 1 | 1 | 38 |
| `154.44.25.233` | 03:30:52-03:31:32 | 35 | 1 | 1 | 1 | 38 |

All requests used Ztest's `claude-cli/2.1.98 (external, cli)` user agent. The
403, 404, and 500 responses are expected diagnostic probes, not service
outages. Each run completed all 38 HTTP interactions with no transport-level
failure visible in the public access log.

## Unresolved artifact

The three report IDs and report JSON are not recoverable from the target access
log because Ztest creates them on its own server. A subsequent submission must
retain the normal `window.open(/report/<id>)` destination in a user-visible tab
or expose a supported response-capture mechanism. The browser's page-evaluation
surface is intentionally read-only, its event API supports downloads only, and
claiming the user's blank in-app tab timed out while attaching the webview.

An older URL found in the desktop log, `01KXGWNE3JJCCP5JETWEVBQYY4`, decodes to
2026-07-15 02:01:43 +08:00 and was already expired (`/api/reports/<id>` returned
404). It is unrelated to these three 03:07-03:31 runs and must not be treated as
their baseline report.

## Relevant current rule

Ztest's public scoring page states that an Anthropic ID should look like
`msg_01...` and applies the `d17_signature_invalid` hard cap of 40 when the ID
is invalid. Direct POMO AWS-B and Q2 samples both use `msg_bdrk_...`; the next
captured report must confirm whether the runtime applies this documented rule
to genuine Bedrock-shaped IDs.
