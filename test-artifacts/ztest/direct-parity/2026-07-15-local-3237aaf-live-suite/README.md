# Local AWS-B live suite at 3237aaf

This suite ran against the compiled local service at `http://127.0.0.1:8991`.
It made 29 real HTTP requests and all 21 assertions passed, including models,
authentication failures, exact text and JSON replies, tool use, streaming,
cache create/read, image and spatial prompts, identity sanitization, and ten
concurrent exact-reply requests.

The upstream IdC account became subject to Kiro's suspicious-activity limit
after this run. Later upstream-dependent calibration requests therefore kept
their HTTP 502 response bodies as evidence instead of treating them as token
comparison results.

This is a pre-tool-history-fix baseline. The same suite must be rerun against
the final release binary once a fresh usable credential is uploaded.
