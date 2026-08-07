# GPT-5.6 Codex account health check

This is a report-only check for the Sub2API accounts named `gpt-51892` through
`gpt-51999`. It never changes `schedulable`, credentials, containers, or
networks. Each probe uses the Responses shape that previously caused customer
502s:

- a replayed opaque `reasoning` input item;
- `text.format: {"type":"text","strict":true}`;
- `store:false`, encrypted-reasoning inclusion, and an SSE response.

The default timer interval is 15 minutes. Account pagination preserves the
2.2-second Sub2API admin throttle. Failed probes are retried once and written to
JSON without API keys.

## Local tests

```sh
python3 -m unittest scripts/tests/test_gpt56_codex_health_check.py
```

For an offline end-to-end run, create a mode-0600 accounts JSON file and pass
`--accounts-file`. The file may either be an array or contain an `accounts`
array. Production normally discovers accounts through the server-local admin
API and reads their direct keys from `sub2api-postgres` in memory.

## Installation layout

The unit templates assume the audited repository is installed at
`/opt/kiro-rs` and results belong in
`/var/lib/kiro-rs/gpt56-health-check-runs`. Install the environment example as
`/etc/kiro-rs/gpt56-health-check.env`, replace the placeholder through the
server's secret-management workflow, and set mode `0600`.

Before enabling the timer, run the service once and inspect `latest.json`.
Deployment to the shared Sub2API host must use the Sub2API production-safety
preflight and requires a separate explicit approval.

Useful manual variants:

```sh
# Default report-only check (gpt-5.6-sol)
python3 scripts/gpt56_codex_health_check.py --out-dir ./gpt56-health-check-runs

# Check every GPT-5.6 model on one run
python3 scripts/gpt56_codex_health_check.py \
  --model gpt-5.6-sol --model gpt-5.6-terra --model gpt-5.6-luna \
  --out-dir ./gpt56-health-check-runs

# Broaden or narrow the account series without changing code
python3 scripts/gpt56_codex_health_check.py \
  --account-regex '^gpt-519(3[0-9]|4[0-9])$' \
  --out-dir ./gpt56-health-check-runs
```

Exit status is `0` only when all selected probes pass, `1` for failed or
uncheckable accounts, and `2` for configuration errors. Use `--exit-zero` only
when a separate monitoring system reads `latest.json` and owns alerting.
