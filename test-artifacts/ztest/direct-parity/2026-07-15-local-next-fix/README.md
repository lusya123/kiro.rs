# Local next-fix verification

Date: 2026-07-15 (Asia/Shanghai)

Target: temporary release build on `http://127.0.0.1:8992`

This build contains two evidence-driven changes that are not in deployed commit
`e9fa9bf` yet:

| Exact answer | POMO input/output | Local input/output |
|---|---:|---:|
| `pong` | 16 / 4 | 16 / 4 |
| `4` | 16 / 3 | 16 / 3 |
| `Red` | 16 / 4 | 16 / 4 |
| `CACHE_OK` | 21 / 9 | 21 / 9 |
| `8b520f60e5d01885` | 25 / 12 | 25 / 12 |

Ten sequential local exact-`pong` calls took 0.304-0.771 seconds. This matches
the configured 0.3-0.8 second application jitter and removes the roughly
2.2-second excess seen through Q2, while avoiding a zero-latency shortcut.

The streamed response retained Bedrock IDs, start usage 1, final usage 4, and
nonzero Bedrock invocation metrics. The temporary process was stopped after
verification; the user's persistent service on port 8991 remained running.
