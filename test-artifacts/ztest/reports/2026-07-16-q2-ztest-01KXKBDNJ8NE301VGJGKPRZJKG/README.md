# ZTest report 01KXKBDNJ8NE301VGJGKPRZJKG

- Report: <https://ztest.ai/report/01KXKBDNJ8NE301VGJGKPRZJKG>
- Captured: `2026-07-15T17:23:16Z`
- Status: `completed`
- Risk level: `low`
- Composite score: `97`
- Model: `claude-opus-4-8`
- Raw SHA-256: `7742e0515e3131b02426167f020926d8243574383eaceac1d8ad3dfc3be1c095`
- Non-full probes: `6`

The exact API response is preserved in `report.raw.json`. Diagnostic subsets are in `non-full-probes.json`, `anomaly-index.json`, and `exceptions.json`. Each probe is also preserved independently under `probes/`; non-full probe objects and their untouched `details` values are under `non-full-probe-json/` and `non-full-detail-json/`.

## Non-Full Probes

| Probe | Name | Status | Score | Latency ms | Error | Diagnosis |
| --- | --- | --- | ---: | ---: | --- | --- |
| IMG | 出图质量 | skipped |  |  |  |  |
| D3 | 身份一致性 | success | 94 | 731 |  |  |
| D21 | Fable 协议特征 | skipped |  |  |  |  |
| D22 | GPT-5.6 协议证明 | skipped |  |  |  |  |
| D7 | 工具调用 | success | 60 | 3737 |  | {"category":"tool_use","suggestions":["结构化输出测试未完全通过, 查看下方 attempts 详情"],"title":"工具调用未生效"} |
| S1 | Token 注入 | success | 80 |  |  |  |

## Interpretation

- D3 `94` is identical to the fresh POMO official result.
- D7 `60` is also identical to POMO. ZTest recorded `{}` for both targets,
  while `packet-capture/d7-validation.json` proves that Q2's four JSON deltas
  reconstruct to valid, exact expected arguments.
- S1 `80` is 20 points above the fresh POMO result and has no suspicious
  overhead samples.
- The three skipped probes are inapplicable to this text model and do not
  reduce the composite score.

The full Q2/POMO comparison is in
`../2026-07-16-q2-non-full-probe-coverage.md`.
