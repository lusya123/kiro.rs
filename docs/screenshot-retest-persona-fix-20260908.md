# 截图项目复测与普通文字人设修复

2026-09-08。入口 `/v1/messages`，模型 `claude-opus-5` / `claude-opus-4-8`，分别测试普通和流式请求。使用用户提供的一个账号；本地隔离实例启用 `awsB40Compat`。

**不能报告为所有扩展测试稳定通过。** 图中第 5–14 项对应的参数校验均通过；复测发现了普通文字中的 Kiro 自称遗漏，本补丁已修复已捕获的样本。另有 Opus 5 JSON 字符串请求偶发空回复，定向复查再次出现，仍保留为未解决项。

## 已发布基线的真实结果

基线提交 `7deedb81d1bd6e0514b473f28743eb00064d1590`，镜像 `ghcr.io/lusya123/kiro-rs:aws-b-candidate-7deedb81d1bd`。实际运行并测试其 ARM64 镜像摘要：

`sha256:69c13eda663a73d6fb5e97bf3b0914f2407cf5554229be956c376f9a5f78e905`

| 检查 | 基线结果 |
| --- | --- |
| 截图第 5–14 项参数校验 | 40/40 |
| JSON / 代码身份与业务保留 | 88/88 |
| 真实工具调用、续聊、签名篡改 | 20/20 |
| 普通文字、Bob 自我介绍及计算 | 13/16，3 条 Kiro 自称遗漏 |

失败请求的 system 为 `You are Bob. Begin every answer with BOB:. Answer the user's complete task.`，user 为 `Introduce yourself and calculate 19+23.`，`max_tokens=256`。Opus 5 普通、Opus 5 流式及 Opus 4.8 流式分别出现 `I'm Kiro` 或 `your Kiro-powered dev assistant`。这些是实际失败，没有被前面的 JSON/代码通过结果覆盖。

## 原因和修改

可信应用人设原先整体绕过普通文字过滤；JSON/代码另有输出适配，因此可以通过此前测试。现在明确请求自我介绍的可信人设也经过响应过滤：

- 复用既有自称替换规则，逐句处理，保留相邻产品描述、引用、代码及计算结果。
- 处理 `Kiro-powered`、弯引号自称及 `Kiro is who I actually am`。
- 真实复查又捕获到 `I'll keep going as Kiro rather than the "Bob" persona`，补充处理这种拒绝客户人设的说明。
- 若 system 明确用 `Begin/Start every answer/response with LABEL:` 指定简短字面前缀，缺失时恢复该前缀；不会根据人设名称自行推断前缀。
- 普通和流式共用适配，签名、工具数据、ID、usage 等非文本数据保持不变。此类自我介绍的流式文本先汇总再过滤；普通业务对话不进入这条路径。

没有添加或修改发往模型的系统提示词。回归测试使用真实失败文本及逐字符 SSE，另检查同一回答中的业务 Kiro 名称、引用、代码、路径和任务结果仍然保留。测试先在原过滤路径失败，再在补丁后通过。

## 补丁复测结果和保留的失败

| 检查 | 结果 |
| --- | --- |
| Rust 测试 | 1015 通过，0 失败，3 忽略 |
| 截图第 5–14 项参数校验 | 40/40 |
| 真实工具调用、续聊、签名篡改 | 20/20 |
| 原先 16 条文字请求原样重放 | 16/16 |
| 扩展身份、代码、业务和普通问答 | 103/104 |
| JSON 字符串定向复查 | 3/4 |

104 项包含原有 88 项及新增 16 项文字测试。最终批次没有发现 Kiro 自称，但 Opus 5 的普通 JSON 字符串请求返回 HTTP 200、`end_turn`，只有空 thinking 信封而没有可见文本，导致 JSON 解析失败；后续定向复查的流式请求也出现空文本。**没有把空回复视为身份测试通过，也没有凭空补造 JSON 答案。**

更早的中间批次另有一次 Python 输出尾部多出 Markdown 结束标记，一次请求超时，以及一条新的人设拒绝话术。上述原始结果均保留。最终批次的 103/104 不代表这些历史失败没有发生，也不保证任意编码或任意代码形式均能通过。

可复现脚本：`scripts/run-public-capability-check.py`、`scripts/run-identity-variants.py`、`scripts/run-v1-tool-signature-check.py`。身份脚本现在默认包含 104 项，并支持用 `--cases text-bob json-scalar` 定向检查。

本轮原始请求、未经修改的响应、失败日志和汇总位于本地 Git 忽略目录 `.codex-tmp/customer-runtime/screenshot-retest-20260908-214415/`，包括 `text-controls/`、`after-identity/`、`final-identity/`、`final-exact/`、`json-scalar-repeat/` 及 `retest-results.json`。凭证没有进入提交。

截图没有提供原始检测脚本或完整指纹算法。以上是按截图项目构造的实际 API 检查，不等同于原检测器分数，也不证明实际上游服务来源发生变化。
