# V1 Messages：POMO 实时对照与本地修复

> 后续按用户要求修复了 Opus 5 fallback 放行与新增身份遗漏。当前 fallback 返回 400，JSON/代码扩展测试 88/88。见 [最新修复报告](identity-and-fallback-fix-20260908.md)。下面保留此前 POMO 对照结果，不能作为当前 fallback 行为说明。

2026-09-08。按用户最终要求，验收主入口为 `/v1/messages`，目标为 Opus 5、Opus 4.8。只修改并运行指定 Worktree：

`/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908`

## 实测结论

**本地修复已生效。Opus 5 与可用 POMO 渠道的 28 组重点对照全部通过；Opus 4.8 本地回归通过，但 POMO 渠道不可用，不能声称它也已完成实时一致性验证。** 最后复查 POMO Opus 4.8 仍为 503 `No available channel`，未将该错误作为模型测试通过。

上轮“全部返回 400”的历史测试不能代替 POMO 实测。本轮发现：

| `/v1/messages` 请求 | POMO Opus 5 | 修复后本地 Opus 5 |
| --- | --- | --- |
| temperature > 1、异常 role | 400，`<nil>`，Bedrock 校验错误封装 | 相同状态、类型和校验文案 |
| web_search、advisor、code_execution 服务端工具 | 400，不支持该工具类型 | 相同 |
| 显式 `output_config.format` | 400，Extra inputs are not permitted | 相同 |
| manual `thinking.type=enabled` | 400，要求 adaptive | 相同 |
| 有效 URL 图片，普通/流式 | 400，URL content sources are not yet supported | 相同 |
| `fallbacks` | 200；包括无效名称、数组和对象等值 | 接受并忽略该元数据，继续请求原模型 |
| 普通问答、应用人设、代码身份 | 200，有效答案及正确身份 | 相同语义，流完整 |

因此，**“与 POMO 相同”和“fallbacks 必须报错”无法同时作为本轮样本的标准**。本次按用户要求优先采用真实 POMO 结果；没有实施跨模型回退。Opus 4.8 的 fallbacks 保留原有拒绝行为，待其 POMO 通道恢复后才能做模型专属判断。

## 本次修改

1. 在代理转换之前统一 Opus 的 Messages 参数校验，两个 Messages 入口都生效，避免 CC 扩展绕过校验。
2. 校验失败按 POMO 的错误类型、字段和普通/流式操作名称封装，HTTP 400 返回 JSON，不再错误标为 SSE。兼容错误和 request ID 由本地生成，**不表示实际执行过 AWS InvokeModel**。
3. Opus 5 message ID 使用 52 位小写 Base32 编码的 256-bit 值。本轮 28/28 个 POMO message ID 均采用该形态。Opus 4.8 保留此前 Base58 UUIDv7 规则，没有在通道不可用时猜测新规则。
4. 保留工具 ID 的稳定映射、工具结果回传和 JSON/代码身份清洗。没有新增或修改系统提示词，没有改写上游签名字节。
5. 顺带修复已实测的 OpenAI Chat 参数漏检：此前 temperature 被丢弃、非法 role 被改成 user；现在两个目标模型的这类请求被拒绝。用户明确主验收为 V1 Messages 后，没有继续扩展 Claude 的 Responses API。

源码：`src/anthropic/pomo_compat.rs`、`handlers.rs`、`id.rs`、`openai_compat.rs`、`router.rs`。

## 验证

- **Opus 5：28/28 对照通过。** 相同请求，比较状态码、错误类型、去除两种动态 request ID 后的完整错误文案、message ID 编码、用户要求的答案/身份及流完成状态。18 个校验错误的规范化文案全部一致。未将 usage 或普通自然语言逐字一致算入该分数。
- **本地两模型能力检查：40/40。** 普通/流式均覆盖；Opus 5 fallbacks 的期望是 200，Opus 4.8 保留旧期望 400。此分数不等同于截图原测试器的分数。
- **代码身份：6/6。** Python/JavaScript 的 Bob/Claude 名称正确，语法有效；未执行生成代码。
- **工具与签名：本地 20/20 语义检查。** 工具 ID 与调用参数正确，4 条工具续聊中随机业务字符串逐字保留，签名中间/末尾字节修改及清空共 12 次全部 400。
- **POMO Opus 5：14/14 工具/签名语义检查。** 工具调用不一定带 thinking；另用计算任务取得真实签名，原签名续聊成功，6 次修改/清空均被拒绝。其末尾修改与空签名的具体错误文案与本地仍有差别。
- **有参数自定义工具及 adaptive：4/4。** 两个模型均完成合法请求。
- Rust：**1011 通过，0 失败，3 忽略**；构建成功，`git diff --check` 通过。

工具测试保留了所有早期失败。最初 POMO 的 `lookup(key)` 样本出现空参数/缺失工具块，未算通过。随后使用既有 `read_fixture` 场景。该场景要求“报告业务值且保持原值”，双方都可能加介绍和代码围栏；最初检查器错误地要求裸字符串，修正后的语义检查只允许完整随机值不变。另外，签名测试接受实际观察到的相关 400 错误变体，不再只认一种错误句子。原始 `summary.json` 未覆盖，重审记录独立保存在 `validated-semantics.json`；API 请求/响应没有编辑。

## 新凭证隔离复验

2026-09-08 17:47（北京时间），从用户新提供的 5 个账号中选取第 1 个，仅加载这一个凭证，在 `127.0.0.1:61997` 启动同一 Worktree、同一 SHA-256 的二进制。原来的 `61999` 服务继续运行。本轮没有修改业务代码或系统提示词。

Opus 5、Opus 4.8 的 `/v1/messages` 共 **84/84 项检查符合当前判定标准**：

| 检查 | 结果 |
| --- | --- |
| 普通问答、有参数自定义工具、adaptive thinking | 6/6 |
| 参数及能力校验，普通/流式 | 40/40 |
| Python/JavaScript 身份及语法 | 6/6 |
| 工具 ID、原签名续聊、业务值保留、签名篡改 | 20/20 |
| 问答、JSON 默认身份与 Bob 人设，普通/流式及 message ID | 12/12 |

8 条 JSON 身份响应、6 条代码身份响应均没有自报 Kiro；默认为 Claude，指定 Bob 时为 Bob。部分 JSON 响应带 Markdown 围栏，这一分数验证身份语义，不代表裸 JSON 格式保证。12 次真实签名修改或清空全部返回 400。工具结果中的随机业务字符串原样保留。

管理接口确认实例只有 1 个可用账号，成功调用计数为 **34**，调用失败和刷新失败计数均为 **0**。另外 50 条请求在本地校验时被拒绝，不计作成功调用上游；因此这次正向验证确实使用了新凭证。

再次使用原有 POMO API key 检查 Opus 4.8，普通/流式均为 **503 `No available channel`**。新提供的是 Kiro 凭证，没有发送给 POMO；本地 Opus 4.8 可用不代表 POMO 的渠道已恢复。上述 40 项仍按模型专属规则判断：Opus 5 的 `fallbacks` 返回 200，Opus 4.8 返回 400，不等同于截图原测试器的分数。

证据目录：`.codex-tmp/customer-runtime/selected-credential-07/`，包含各项原始请求/响应、`messages-identity/validated-semantics.json`、`final-verification.json`。凭证和本地访问密钥仅保存在 Git 忽略目录中。复验结束后停止隔离实例，保留 `61999` 主测试服务。

## 边界

- 截图原脚本未提供，无法证明任何未知探针都通过；以上范围和判定规则可复核。
- POMO Opus 4.8 不可用。其模型列表存在该名称不代表存在可用渠道。
- POMO `/cc/v1/messages` 返回网页而非模型 API，不能把 HTTP 200 当成功。当前主要验证的是 `/v1/messages`。
- POMO Opus 5 的 OpenAI `response_format` 示例返回 200 和普通文本，不等于遵循 Schema；Claude `/v1/responses` 示例在 POMO 返回 400，本地该非主要入口仍不支持 Claude Responses。
- 普通 JSON 和代码有时带 Markdown 围栏，POMO 自身也会这样返回。应用身份已经正确，不代表所有输出格式请求都可保证。
- usage、invocationMetrics 仍由本地适配逻辑计量/组装；Kiro 隐藏提示和实际 provider 不会因这些修复变成原生 Bedrock。签名回传拒绝测试证明的是当前代理校验行为，不是 Anthropic 原生验签认证。

## 运行与复核

当前本 Worktree 的 `target/debug/kiro-rs` 监听 **127.0.0.1:61999**，PID **98971**；SHA-256：`17f1edc8d6a323574d6a36767a3fd273079cc1b4bd0c74326e2a586ae98895df`。未提交、推送或部署远端。

本地 `.codex-tmp/customer-runtime/` 中的关键证据：

- `pomo-v1-reference-03/`、`pomo-v1-local-fixed-04/`：重点原始请求/响应。
- `pomo-v1-paired-comparison-04.json`：28 组逐项比较结果。
- `pomo-v1-capability-fixed-04/`、`pomo-v1-code-identity-fixed-05/`、`pomo-v1-positive-fixed-06/`。
- `pomo-v1-tools-local-fixed-05/validated-semantics.json`、`pomo-v1-tools-fixture-reference-05/validated-semantics.json`，及同目录原始证据。
- `pomo-all-interfaces-reference-01/`、`pomo-all-interfaces-local-before-01/`：最初跨接口审计，包括真实失败。
- `pomo-edge-reference-02/`：有效 URL 图片及不同 fallbacks 值。
- `pomo-opus48-final-availability.response.raw`：最后的 Opus 4.8 渠道状态。
- `pomo-interface-full-tests-03.log`、`pomo-interface-build-03.log`、`pomo-v1-final-verification-06.json`。

可复现脚本：`scripts/run-pomo-interface-audit.py`、`scripts/run-public-capability-check.py`、`scripts/run-v1-tool-signature-check.py`。密钥只从本地配置读取，不进入请求证据文件或日志。
