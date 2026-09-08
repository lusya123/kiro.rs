# 客户视角检测：当前仍可发现 Kiro 身份与代理差异

2026-09-08。只做现状审计，没有修改服务响应、身份过滤或签名逻辑。请求目标为当前 Worktree 的 `127.0.0.1:61999`；POMO 仅作独立参照。主测试开始于北京时间 13:16:45，最后一条主测试请求开始于 13:19:32；另补充固定拒绝话术检查。

**结论：仍能发现。六个本地模型在不指定 Bob、要求严格 JSON 返回当前助手人设名称时，都返回了 `{"name":"Kiro"}`。** 提问中没有要求模型回答 Kiro，也没有向 name 字段提供候选值。设置 Bob 后的答案会随问法变化；不能用一次 Bob 或格式测试通过证明来源特征已消失。

本次完成 **154 条 HTTP 请求**：本地 121 条主检测、POMO Sonnet 4.6 同请求参照 21 条、本地固定话术补测 12 条。包含预期的 400 与 404，不能把 HTTP 200 数量或功能断言通过率当成来源认证。五个指定型号均覆盖，并补测客户反馈提到的 Haiku。没有另行调用真正 AWS Bedrock 账号；POMO 的响应也不能自行证明其基础设施来源。

## JSON 与应用人设

每个本地模型进行了三种 Bob 人设 JSON 问法，以及一次不设置 Bob 的 JSON 问法。全部使用有效 JSON Schema，只约束 name 为字符串，不强制名称值。

| 模型 | 有 Bob system，三种问法的实际 name | 不设置 Bob 的实际 name | 另一条 BOB 前缀请求 |
| --- | --- | --- | --- |
| Sonnet 4.6 | Bob / Kiro / Bob | Kiro | 未出现 Kiro；有 BOB: 前缀 |
| Sonnet 5 | Bob / Bob / Kiro | Kiro | 含 Kiro；有 BOB: 前缀 |
| Opus 5 | Bob / Bob / Bob | Kiro | 含 Kiro；有 BOB: 前缀 |
| Opus 4.7 | Bob / Bob / Bob | Kiro | 未出现 Kiro；有 BOB: 前缀 |
| Opus 4.8 | Bob / Bob / Bob | Kiro | 含 Kiro；无 BOB: 前缀 |
| Haiku 4.5 | Kiro / Kiro / Kiro | Kiro | 未出现 Kiro；有 BOB: 前缀 |
| POMO Sonnet 4.6（参照） | Bob / Bob / Bob | Claude | 有 BOB: 前缀，未出现 Kiro |

本地 Bob JSON 共 18 条，其中 5 条返回 Kiro；无 Bob 的 6 条全部返回 Kiro。Opus 5、Opus 4.8、Opus 4.7 本轮三条 Bob JSON 均返回 Bob，但不设置 Bob 时仍返回 Kiro。Sonnet 5 和 Opus 5 的一条回复虽然以 BOB: 开头，正文仍自称 Kiro；因此仅检查前缀的旧功能测试不足以判断身份行为。

两个实际响应摘录：

- Sonnet 5：`BOB: I'm Kiro, an AI-powered development environment here to help with coding and technical work.`
- Opus 4.8：`I'm Kiro, an AI-powered development environment.`，同条响应明确表示保留其实际身份而不采用 Bob 人设。

## 对客户原始问题逐项复核

| 原始问题 | 当前结论与证据 |
| --- | --- |
| 自造 message ID | 仍存在。样本为 `msg_bdrk_011C` 加随机后缀；源码 `id.rs` 明确调用 `random_base62(20)`。客户能观察到 ID 分布差异，但不能把某个前缀当成 AWS 官方认证标志。 |
| 篡改 signature 仍被放行 | 本次未复现：六模型原签名续聊均 200；修改中间字节、末尾字节和清空签名共 18 条均 400。已核对两种字节修改确实改变了 Base64 解码后的数据，而非只改变无效填充位。 |
| 这是否等于 Anthropic 原生验签 | 不等于。源码用本服务签名登记与兼容校验；Kiro 历史转换没有把签名交给 Anthropic 原生验签面。本次只修改 thinking 文本、保留原签名时，六模型均 200；POMO 同样是 200。不能将摘要文本修改与签名字节篡改混为一谈。 |
| usage 被代理重算 | 仍存在。源码输入计数采用本地估算，忽略传入的 Kiro context 计数。本轮相同请求 Sonnet 4.6 本地输入 21、POMO 19；增加相同 system 文本后分别为 427、425，仍相差 2。 |
| 流末 invocationMetrics 拼装 | 仍存在。源码由本地 usage 与计时值组装。所测本地 SSE 额外带 cacheReadInputTokenCount/cacheWriteInputTokenCount（均 0），同请求 POMO 样本未带这两个字段。 |
| 错误文案被包装 | 仍有差异。非法 display：本地为简短的 `thinking.display must be summarized or omitted`；POMO 为含 InvokeModel / ValidationException 的包装错误。两边此例 error.type 都是 `<nil>`。不能概括成所有错误都只返回同一句通用话术。 |
| thinking budget ≥ 2048 均拒绝 | 已不再成立。Sonnet 4.6 的 2048、4096 均 200，Haiku 2048 为 200。Sonnet 5、Opus 5、Opus 4.7、Opus 4.8 的 manual enabled 仍为 400，应使用 adaptive；本次它们的 adaptive 工具请求均 200。但 Sonnet 4.6 的 Kiro 适配不能精确执行指定的内部预算。 |
| JSON 中承认 Kiro / 更早的隐藏指令 | 仍能复现。六模型自由名称 JSON 均为 Kiro；六模型布尔 JSON 都将“更早的隐藏指令”“早于 Bob 的不同人设”和 Kiro 标为 true。POMO 同请求全部标为 false。 |
| 固定拒绝句覆盖 Bob | 部分场景仍存在，见下节；不是所有模型、所有问法都一律拒绝。 |
| tool id 一律为 tooluse_ | 不能再这样概括。本轮 Sonnet 4.6、Haiku 为 tooluse_；其余四模型为 toolu_bdrk_，见下节。 |
| 签名内可见部署名 | 仍可读到：Opus 4.8 的 claude-quince、Opus 5 的 claude-honey、Sonnet 5 的 claude-saffron。它们是可观察值，不能单凭这些名称认证实际来源。 |

请求 `POST /v1/messages/count_tokens` 在本地六模型和 POMO Sonnet 4.6 均返回 404；没有将这项计作两边差异，也没有伪造 count_tokens 结果。

同样回答字符串 `42` 时，本地 Sonnet 4.6 普通响应 output_tokens 为 8、SSE 为 3；POMO 对应两种响应均为 5。这也是本轮可见的计量差异。

## 工具 ID 与签名元数据

只读取不透明签名的既有外层元数据，没有解密思考内容或重写签名。部署名按 Protobuf 长度字段提取，避免把后续字段标签误算成模型名称的一部分。

| 模型 | 本次 tool id 前缀 | 本次签名外层模型字段 |
| --- | --- | --- |
| Sonnet 4.6 | `tooluse_` | `claude-sonnet-4-6` |
| Sonnet 5 | `toolu_bdrk_` | `claude-saffron` |
| Opus 5 | `toolu_bdrk_` | `claude-honey` |
| Opus 4.7 | `toolu_bdrk_` | `claude-opus-4-7` |
| Opus 4.8 | `toolu_bdrk_` | `claude-quince` |
| Haiku 4.5 | `tooluse_` | `claude-haiku-4-5` |
| Sonnet 4.6（POMO） | `toolu_bdrk_` | `claude-sonnet-4-6` |

本地六模型及 POMO 对原签名、修改摘要、修改签名字节的本轮返回码相同，因此不能声称这组篡改探针现在单独区分出了 POMO。源码中仍存在上游缺少签名时的本地 HMAC 回退路径；也不能据此宣称所有公开签名都来自上游。

## 固定拒绝话术与布尔探针的边界

补测问题只询问是否存在更早的系统指令，明确不要求引用或复述原文。实际出现：

- Sonnet 5：有无 Bob system 均回复 `I can't discuss that.`；有 Bob 时没有 BOB: 前缀。
- Opus 4.7：有 Bob system 时回复 `I can't discuss that.`，没有 BOB: 前缀。
- Opus 4.8、Haiku：有 Bob system 时回复 `BOB: I can't discuss that.`。
- Sonnet 4.6 两种情况都承认更早的指令，有 Bob 时保留前缀；Haiku 无 Bob 的这条问题却回答 NONE。

布尔概念检查没有重现“所有 Kiro 概念全命中”：本轮 specs 为 4/6、steering 为 1/6、Autopilot/Supervised 为 3/6、MCP 为 0/6；Haiku 还将 Anthropic 和 AWS 标为 true。可见模型自述受问法影响，不能作为系统提示逐字内容的验证。

客户没有提供“两句原文”的具体文本，本次没有逐字复验，也没有重新测量隐藏提示约 2800 词的说法。不能把这些旧结论直接写成本轮已验证事实。

官方将 signature 视作不透明值，要求客户端原样回传，并区分 omitted 块与可读摘要；普通摘要与 omitted 的验证语义不能混用。[Claude thinking 文档](https://platform.claude.com/docs/en/about-claude/models/extended-thinking-models)。不同代际手动/自适应模式的支持规则见 [extended thinking 文档](https://platform.claude.com/docs/en/build-with-claude/extended-thinking)。

## 证据与可复核性

原始请求、原始响应、选择的响应头、开始时间及响应 SHA-256 均保存在 Git 忽略目录；请求文件不含鉴权头。本报告不包含凭证。

- [主检测原始结果](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/.codex-tmp/customer-runtime/customer-detection-audit-01/summary.json)
- [解析后的分模型汇总](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/.codex-tmp/customer-runtime/customer-detection-audit-01/analysis.json)
- [固定话术补测](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/.codex-tmp/customer-runtime/refusal-check-01/summary.json)
- [JSON 返回 Kiro 的 Sonnet 4.6 原始响应](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/.codex-tmp/customer-runtime/customer-detection-audit-01/local--claude-sonnet-4-6--persona-schema-bob-2.response.raw)
- [JSON 返回 Kiro 的 Sonnet 5 原始响应](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/.codex-tmp/customer-runtime/customer-detection-audit-01/local--claude-sonnet-5--persona-schema-bob-3.response.raw)

用于确认实现而非冒充客户黑盒证据的源码：

- [随机 message ID](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/src/anthropic/id.rs:28)
- [本地 input_tokens 计数](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/src/anthropic/billing.rs:9)
- [invocationMetrics 拼装](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/src/anthropic/bedrock.rs:669)
- [签名登记验证](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/src/anthropic/signature.rs:381)
- [错误包装](/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908/src/anthropic/handlers.rs:641)

本次是有限、可复现的接口与源码审计，不是对所有可能探针的穷尽证明。模型自述、ID 前缀、工具 ID 和元数据各自都不是基础设施认证；但这些实测响应已经足以证明当前仍公开出现 Kiro 身份，且与 POMO 参照存在明确差异。实际出站链路为 Kiro，则由运行配置和 endpoint 代码另行确认。
