# ZTest report 01KXKCA8K1FYQZGHSR300VYWW2

- Report: <https://ztest.ai/report/01KXKCA8K1FYQZGHSR300VYWW2>
- Captured: `2026-07-15T17:23:22Z`
- Status: `completed`
- Risk level: `high`
- Composite score: `40`
- Model: `claude-opus-4-8`
- Raw SHA-256: `ac614e95996fb2b3ffebe2ecf9c64880e5acedd3b5dd9eec83a345c57feef4f2`
- Non-full probes: `11`

The exact API response is preserved in `report.raw.json`. Diagnostic subsets are in `non-full-probes.json`, `anomaly-index.json`, and `exceptions.json`. Each probe is also preserved independently under `probes/`; non-full probe objects and their untouched `details` values are under `non-full-probe-json/` and `non-full-detail-json/`.

## Non-Full Probes

| Probe | Name | Status | Score | Latency ms | Error | Diagnosis |
| --- | --- | --- | ---: | ---: | --- | --- |
| IMG | 出图质量 | skipped |  |  |  |  |
| D3 | 身份一致性 | success | 94 | 5989 |  |  |
| D8 | 响应延迟 | success | 90 | 1244 |  |  |
| D17 | 响应签名 | partial | 75 | 2412 |  | {"category":"unknown","suggestions":["查看下方 details 字段获取更多技术细节"],"title":"检测异常"} |
| D21 | Fable 协议特征 | skipped |  |  |  |  |
| D22 | GPT-5.6 协议证明 | skipped |  |  |  |  |
| D7 | 工具调用 | success | 60 | 18660 |  | {"category":"tool_use","suggestions":["结构化输出测试未完全通过, 查看下方 attempts 详情"],"title":"工具调用未生效"} |
| D9 | 稳定性 | partial | 65 | 9256 |  | {"category":"stability","suggestions":["延迟变异系数 CV=0.38,波动较大;可能是中转站负载不均或多节点路由"],"title":"延迟稳定性偏低"} |
| S1 | Token 注入 | success | 60 |  |  | {"category":"upstream","suggestions":["no_usage_data","reported_tokens 随 prompt 长度异常增长,中转站可能按内容追加额外指令","导致实际消耗的 token 比你发送的更多"],"title":"疑似按内容增长的 Token 注入"} |
| S2 | 提示词提取 | partial | 50 |  |  | {"category":"unknown","suggestions":["查看下方 details 字段获取更多技术细节"],"title":"检测异常"} |
| S3 | 指令覆盖 | partial | 0 |  |  | {"category":"unknown","suggestions":["查看下方 details 字段获取更多技术细节"],"title":"检测异常"} |

## Reference interpretation

This is a fresh POMO official baseline taken with the same full, sequential
ZTest mode as Q2. Its score is capped at 40 by D17. The run also has three
empty/error S3 samples and substantial latency/stability degradation, so it
must not be treated as proof that POMO always scores 40.

D7 is nevertheless decisive for parser diagnosis: ZTest records `{}` and 60
for POMO exactly as it does for Q2. A direct replay with the report nonce is in
`direct-replay/`; `d7-validation.json` reconstructs ten POMO JSON deltas into
valid, exact expected arguments. That rules out AWS-B's tool implementation as
the cause of this shared D7 result.

The full Q2/POMO comparison is in
`../2026-07-16-q2-non-full-probe-coverage.md`.
