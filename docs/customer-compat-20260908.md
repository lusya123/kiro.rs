# AWS-B 客户反馈排查与本地实测

> 最新公共接口变更：`/v1/messages` 已恢复拒绝显式 `output_config.format`，本地 Schema 适配保留于 `/cc/v1/messages`，见 [Opus 公共接口回归修复](opus-public-capability-regression-20260908.md)。

> 后续更新：message ID、tool ID 和 JSON 身份清洗已在同一 Worktree 继续修复，见 [POMO ID 与 JSON 身份输出兼容修复](pomo-id-json-compat-20260908.md)。下文保留前一轮的历史状态与证据。

2026-09-08。工作分支：`codex/aws-b-customer-compat-20260908`。基线：GitHub `origin/aws-b` 的 `68dd35c1e5c15bcf1e6328495f2858cc9e2f6c17`。

## 结论与验收状态

**尚未达到“所有输出与 POMOAI 完全一致”的验收条件。** 已修复本地可确认的问题，并使用用户提供的两份账号凭证完成导入、真实请求、工具续聊和签名篡改测试。请求链路保持“客户端 → 本地容器 → Kiro”；POMOAI 只用于单独的参照测试，没有配置为业务上游。Kiro 上游仍会拒绝部分应用人设并改变输出格式。POMOAI 自身也有格式错误和不可用模型，不能把逐字相同作为全部正确性的标准。

本仓库默认使用 Kiro 接口，不能通过修改 ID、替换品牌名或调整 usage 把它变成原生 Bedrock。特别是签名不能重写：修改后会破坏它作为不透明上游值的完整性。对代码、JSON key/value、路径和工具参数进行全局 `Kiro` 替换，同样不能满足用户要求的内容保真。

## 本次修复

1. **Claude 客户应用人设冲突。** 以前只有 GPT 的特定人设得到保护，Claude 的 `You are Bob...` 后仍追加最高优先级的 Claude 身份指令，部分请求还被本地固定回复截走。现在识别有效的客户应用人设后，保留客户指令，由上游生成回复；这些请求的输出不再进入身份替换路径。这消除了本地冲突，但不能保证 Kiro 上游接受 Bob。没有客户应用人设、也没有显式 JSON Schema 的请求仍保留历史兼容逻辑。
2. **4.6 手动 thinking 被过早拒绝。** 公共入口误用了“禁止 assistant prefill”的模型集合来禁止 `thinking.type=enabled`。现在按实际手动 thinking 支持集合判断；Sonnet/Opus 4.6 可进入后续预算校验和上游请求。非法预算继续返回 400。
3. **显式原生 Bedrock 路由发生隐式回退。** 以前 JSON Schema 请求会改走 Kiro。现在显式选中的原生模型始终走原生 provider，参数校验和错误由该上游负责，响应字节保持透传；本地 Kiro 参数校验不再抢先拒绝原生请求。模型别名仍按该 provider 既有规则加 `anthropic.` 前缀。此路径未在本次真实 AWS Bedrock 账号上测试，因为用户提供的是 Kiro 凭证。

4. **显式 JSON Schema 被统一拒绝、或只靠提示词而缺乏验证。** Kiro 路径现在接受有效 schema，生成前离线编译并拒绝无效或无法解析的外部引用 schema，生成后校验 JSON 语法和 schema。合法的响应字节原样返回；若整个答案只是给合法 JSON 加了一层 Markdown 围栏，仅移除这层围栏。JSON key/value、代码、路径、数字以及非文本元数据保持原样；其他无效最终答案返回 502，不补造字段。显式 schema 请求绕过本地固定身份回复和身份替换。工具调用、截断、拒绝和上游错误保留原有语义。这个实现是本地验证适配器，不是上游原生约束解码；Schema SSE 会缓冲完整结果（上限 32 MiB / 360 秒），因此会延后首字节，普通对话不进入此路径。原生 Bedrock 选中路由继续透传，由原生上游验证。

5. **格式提示干扰工具执行。** 显式 Schema 请求不再追加“先向用户说明再调用工具”的普通文本要求；有工具时，格式要求明确只约束工具执行完成后的最终答案。读写工具策略只在实际提供 `Write` / `Edit` 工具时追加。真实复测中，Sonnet、Opus、Haiku 均完成读取、自动选择写入工具、最终 JSON 三个步骤，随机代码内容与路径保持一致。工具结果由测试客户端模拟；这验证了真实上游的调用选择与参数，没有执行外部文件服务。

6. **thinking 的 display 默认值错误，而且原生请求丢失 thinking 控制。** 原逻辑把所有模型都当成默认 omitted，同时只向 Kiro 传 effort。现在 Sonnet 4.6 按模型默认返回摘要，较新模型默认 omitted，显式 display 优先。已为用户指定的 Sonnet 4.6 / Sonnet 5 / Opus 5 / Opus 4.7 / Opus 4.8 加入原生 `additionalModelRequestFields.thinking` 转发和非法 display 校验。直接请求 Kiro 已证明 `adaptive` / `display` 字段确实被验证；Sonnet 4.6 与 Opus 4.7 在较复杂任务中可返回摘要和签名，Sonnet 5 的 summarized 样本仍只有签名。**Kiro 的 Sonnet 4.6 不接受原生 enabled/budget_tokens；本地接受的手动请求采用 adaptive 适配，不能精确执行客户端指定的内部思考预算。** HTTP 200 不代表预算已经原样传给上游。

没有改造 message ID 的随机算法，没有伪造新签名，也没有扩大品牌替换范围。

## 当前默认的 AWS-B 原生请求

代码路径：`src/kiro/endpoint/ide.rs`、`src/kiro/model/requests/kiro.rs`、`src/anthropic/converter.rs`。

```http
POST https://q.us-east-1.amazonaws.com/generateAssistantResponse
Authorization: Bearer <Kiro access token>
Content-Type: application/json
x-amzn-kiro-agent-mode: vibe
x-amzn-codewhisperer-optout: true
x-amz-user-agent: aws-sdk-js/1.0.34 KiroIDE-<version>-<machineId>
```

本次直接实测使用的请求结构（账号标识已脱敏）：

```json
{
  "conversationState": {
    "chatTriggerType": "MANUAL",
    "conversationId": "<uuid>",
    "currentMessage": {
      "userInputMessage": {
        "content": "Compute 19+23. Reply with the integer only.",
        "modelId": "claude-sonnet-4.6",
        "origin": "AI_EDITOR",
        "userInputMessageContext": {}
      }
    }
  },
  "profileArn": "<account profile ARN>"
}
```

HTTP 200，响应实际为 AWS EventStream 二进制帧；已核验帧 CRC。三帧 payload 为：

```json
[
  {"content": "42", "modelId": "claude-sonnet-4.6"},
  {"contextUsagePercentage": 0.4116000235080719},
  {"unit": "credit", "unitPlural": "credits", "usage": 0.017557542371475953}
]
```

这次样本没有原生 Anthropic message ID，也没有精确 input/output token breakdown。不能把这三个事件重新包装得到的数字当成 AWS 原生 token 账单。

客户的 Anthropic `system` 在默认转换路径中被放入历史 `userInputMessage`，并追加 assistant 确认消息；它不是原生 Messages API 的独立 `system` 字段。这也是本地保留指令后，上游仍可能不服从应用人设的结构性限制。

## 真正的 Bedrock Messages 请求

仓库已有可选 `bedrockMantleEnabled` provider，对应：

```http
POST https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages
x-api-key: <Bedrock API key>
anthropic-version: 2023-06-01
Content-Type: application/json
```

```json
{
  "model": "anthropic.claude-sonnet-4-6",
  "max_tokens": 8192,
  "system": "You are Bob. Begin every answer with BOB:.",
  "thinking": {"type": "enabled", "budget_tokens": 4096},
  "messages": [{"role": "user", "content": "Calculate 19+23."}]
}
```

这是另一种账号和鉴权体系，不能用 Kiro refresh token 替代 Bedrock API key。[AWS Messages API 文档](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-messages-api.html)

当前官方文档区分：4.6 仍接受手动 thinking，4.7 及以后拒绝 `enabled` 并使用 adaptive。因而不能把 Opus 4.8 的所有 400 都当成代理故障。[Claude thinking 文档](https://platform.claude.com/docs/en/build-with-claude/extended-thinking)

## 验证结果

- 前端：`pnpm install --frozen-lockfile && pnpm build` 成功。
- Rust：`cargo test --no-default-features --quiet`，994 通过、0 失败、3 跳过；`cargo build --no-default-features` 成功。构建仍有 80 项 unused / dead-code 等警告；本次不宣称无警告。
- 新增回归先在修复前失败，再在修复后通过：Claude 应用人设、代码/JSON/工具内容保留、4.6 手动预算公共入口、原生 provider 不回退及不抢先校验、JSON Schema 能到达 provider、合法输出字节保真、非法 schema/无效最终 JSON 拒绝。
- 原生 provider 的本地 HTTP 集成测试覆盖请求体（除模型映射）保留、JSON/SSE 响应字节保留、原生 400 错误保留、客户端鉴权与上游鉴权隔离。这些是本地测试服务器，不是真 AWS 或 POMOAI。
- 两份用户凭证通过 `/api/admin/credentials` 实际上传成功，账号 ID 为 1、2。
- 首轮真实请求 30 项，最初断言通过 16 项。失败包含本地预算分类、上游围栏、人设及不支持的 JSON Schema；首轮也错误地把 Opus 4.8 的 enabled 设为应成功，已根据当前官方文档纠正测试预期，原始结果保留。
- 修正预算分类后，针对两模型的工具、thinking 和非法预算复测 12 项全部符合当前预期。Sonnet 4.6：1024/2048/4096 和 adaptive 均为 200；Opus 4.8：adaptive 为 200，enabled 为 400。
- 两模型真实工具结果续聊均为 200，原工具路径和内容保持不变。工具结果由测试客户端模拟，不代表执行了外部写文件服务。
- 两模型原始 thinking signature 回传均为 200；改动签名中间一个字符后均为 400（4 项全部通过）。这验证的是本服务签名登记，不是 Anthropic 对 Kiro 历史消息进行了验签。

仍然失败的真实样本：Sonnet 和 Opus 均曾在 Bob 人设下返回 `{"name":"Kiro"}`；Opus 的一次 SSE 回复明确拒绝 `BOB:` 前缀，但正确算出 42。代码和 JSON 中的 Kiro 字符串在所测样本里保留；部分响应额外添加 Markdown 围栏，不能算严格格式通过。

## 本地运行与后续对照

本地测试实例监听 `127.0.0.1:61999`，未部署或修改远端。运行配置、已导入凭证和完整请求/响应位于 Worktree 的 `.codex-tmp/customer-runtime/`（Git 忽略目录）。不得提交或同步其中的凭证文件。

主要证据：`live-01/`、`live-03/`、`continuations/`、`signature-replay/`、`native-kiro.request.redacted.json`、`native-kiro.events.json`。`live-02/` 是启动尚未就绪时的连接失败，未作为上游结果计入。

## POMOAI 实测对照

全部使用同一组请求体和相同模型名。完整原始响应保存在本地忽略目录中，报告不包含凭证。

| 项目 | POMOAI | 本地 AWS-B | 结论 |
| --- | --- | --- | --- |
| Sonnet 4.6 普通对话、BOB 前缀、代码、工具参数 | 成功 | 成功 | 所测常规样本可用；不能据此保证所有任务无回归 |
| Bob 人设，JSON 返回应用名称 | `Bob`，带 JSON 围栏 | `Kiro`，带 JSON 围栏 | 本地冲突已消除，上游人设覆盖仍未解决 |
| 仅用文本要求裸 JSON | 部分加 Markdown 围栏 | 部分加 Markdown 围栏 | 两边都有，不是 Kiro 独有证据 |
| 简单 `output_config.format` | 200，`{"Kiro":"Kiro"}` | 修复前 400；最后一轮 Sonnet/Opus/Haiku 的普通和 SSE 均 200 | 本地入口与完整外层围栏问题已修复 |
| 代码字符串含双引号的嵌套 Schema | 4 个新增样本均未通过内容/JSON 断言；存在未转义双引号，Haiku SSE 还把指定整数改成 1 | 最后一轮 Sonnet/Opus/Haiku 的普通和 SSE 均正确保留 | POMOAI 也不是无错误的参照；不能照搬无效 JSON |
| Sonnet 4.6 thinking 1024/2048/4096/adaptive | 均 200 | 修复后均 200 | 参数误拒绝已解决 |
| Sonnet 4.6 工具 ID | `toolu_bdrk_…` | `tooluse_…` | 上游/封装差异仍在；工具参数和续聊实测正常 |
| message ID | 同轮出现 `msg_bdrk_011Ceq…` 及较长小写字母数字形式 | 随机 `msg_bdrk_011C…` | 本地仍为自造 ID；POMOAI 也并非只有一个 ID 样式 |
| usage / metrics / 错误 | 响应字段和数值因样本、渠道不同 | 存在本地计数、组装 metrics 和错误包装 | 未消除，不能当作原生 AWS 计量 |
| Opus 4.8 / Sonnet 5 | 当前分别 16/16、5/5 请求 503，无可用渠道 | Opus 4.8 本地正常；Sonnet 5 未做本轮同请求完整对照 | 无法比较 POMOAI 不可用模型的实际生成行为 |

Haiku 4.5 的 POMOAI 补充样本中普通对话、BOB 前缀、工具、简单 Schema 均成功；应用名称为 Bob，但加了 JSON 围栏。

对照产物：`pomo-reference-01/`、`local-paired-01/`、`pomo-extra-models-01/`、`pomo-schema-02/`。新增 `scripts/run-customer-contract.py` 支持 `--models`、`--cases`、`--base-url`，密钥仅从本地配置读取，阻止跨源重定向，原始响应不做改写。数字“passed”包含文本格式与内容断言，不可把所有失败都解释为服务不可用。

## 修复后的本地实测

每次使用新二进制前均重启本地实例并等待管理接口就绪；最后一次启动确认两份已导入凭证均可用。下列轮次保留完整原始证据，较早的失败没有删除：

| 证据目录 | 范围 | 实测结果 |
| --- | --- | --- |
| `local-schema-02/` | 早期 27 项回归 | 21 通过、6 失败；当时 Haiku 4 项 Schema 均为 502，属于历史状态 |
| `local-schema-04/` | 三模型 36 项广泛回归 | 24 通过、12 失败；包含人设、普通代码/JSON 围栏及当时的一项 Haiku Schema 失败 |
| `local-schema-05/` | 三模型 × 简单/嵌套 Schema × 普通/SSE | **12/12 通过**；JSON 内代码双引号、`Kiro`、`.kiro/specs/test.json` 和整数 `1234567890123456789` 保留 |
| `schema-workflow-02/` | 数组、字符串、整数及单工具续聊 | 14/15 通过；当时 Haiku 工具完成后最终 Schema 答案为 502，随后继续修复格式/工具提示冲突 |
| `schema-multitool-01/` | 读取 → 写入 → 最终 Schema 答案 | Sonnet 读后跳过写入、直接返回 `saved:true`，断言正确报失败；Opus/Haiku 完成三步 |
| `schema-multitool-02/` | 调整为“仅最终答案受 Schema 约束”后的同流程 | **9/9 通过**；三个模型都实际返回正确的读、写工具调用，然后才给最终 JSON；第二步没有强制工具选择 |

多工具测试的读取结果包含请求时无法预知的随机代码字符串；写入参数和最终 JSON 都必须逐字包含该字符串，不能仅凭 `saved:true` 判定成功。工具结果由客户端模拟，未真正调用外部文件系统服务。

此阶段 Rust 全量测试为 992 通过、0 失败、3 跳过，构建成功。日志：`/tmp/awsb-schema-workflow-final-tests-20260908.log`、`/tmp/awsb-schema-workflow-build-20260908.log`。后续原生 thinking 转发修复的全量测试为 994 通过、0 失败、3 跳过；日志：`/tmp/awsb-five-native-thinking-tests-20260908.log`、`/tmp/awsb-five-native-thinking-build-20260908.log`。本地实例持续监听 `127.0.0.1:61999`。

这些有限样本不能证明任意 Schema 必定生成成功。显式 Schema 的格式错误仍会返回 502；没有显式 Schema 的普通对话不做外层围栏移除，人设与格式失败仍需如实记录。

## 五个指定模型的独立测试

用户指定 Sonnet 4.6、Sonnet 5、Opus 5、Opus 4.7、Opus 4.8 必须分别测试。以下为 thinking 原生字段修复前的新基线，每模型 13 项：普通对话、人设、代码、普通 JSON、4 项 Schema、工具、手动/自适应 thinking 和非法预算。原始证据 `five-model-contract-01/`。

| 模型 | 断言通过 | 失败内容 |
| --- | --- | --- |
| Sonnet 4.6 | 11/13 | 普通 JSON 添加围栏；应用名称返回 Kiro |
| Sonnet 5 | 11/13 | 普通 JSON 添加围栏；应用名称返回 Kiro，并额外否认 Bob 人设 |
| Opus 5 | 12/13 | 应用名称返回 Kiro |
| Opus 4.7 | 13/13 | 此轮样本未失败；不能推断所有人设和任务均服从 |
| Opus 4.8 | 11/13 | 拒绝 BOB 前缀；应用名称返回 Kiro |

合计 58/65；其中五模型的 20 项显式 Schema 检查全部通过。手动 thinking 在 Sonnet 4.6 的预期为 200，在另外四个模型的预期为 400，并非要求所有型号接受相同参数。

同期 POMO 可用性检查 `pomo-five-model-availability-01/`：Sonnet 4.6 为 200；Sonnet 5、Opus 4.7、Opus 4.8 为 503；Opus 5 为 403 authentication_error。不能将这四个错误响应当成模型正常行为的参照。

签名补测 `signature-text-01/`（初始响应）与 `signature-text-02/`（续聊）：本地和 POMO 都是原签名 200、修改 thinking 文本 200、改动签名字节 400，工具续聊正常完成。本地原块文本为空，POMO 原块文本非空，不能由这一组样本推断所有原生服务均如何处理被修改的摘要。[官方说明](https://platform.claude.com/docs/en/about-claude/models/extended-thinking-models)区分普通摘要与 omitted 块，并要求客户端原样回传；不能把“修改文本被接受”直接等同于“签名字节未验证”。测试脚本同时修复了无参数工具仅返回初始 `{}`、附带空 JSON delta 时的解析错误。

原生字段证据 `five-native-thinking-01/` 与 `native-thinking-hard-02/`：五模型分别执行直接 Kiro 请求，原始 AWS EventStream 校验 CRC 后落盘。非法 display 返回 400；Sonnet 4.6 的 enabled 类型被原生接口拒绝，adaptive 被接受。较复杂算术请求中，Sonnet 4.6 / Opus 4.7 的 summarized 返回非空 reasoning 文本与签名，omitted 返回空文本与签名；Sonnet 5 两种模式都只返回签名。Opus 5 / Opus 4.8 在前一组样本中返回了原生 reasoning 文本与签名。

新二进制的最终分项实测：

| 模型 | thinking 显示检查 | thinking + Schema 连续工具步骤 |
| --- | --- | --- |
| Sonnet 4.6 | 5/5 | 3/3 |
| Sonnet 5 | 3/5：普通/SSE summarized 均无摘要 | 3/3 |
| Opus 5 | 5/5 | 3/3 |
| Opus 4.7 | 5/5 | 3/3 |
| Opus 4.8 | 5/5 | 3/3 |

显示检查为默认 SSE、summarized SSE、summarized 普通响应、omitted SSE、disabled SSE，合计 **23/25**。任务要求求解并验证四个同余条件，五模型各模式的最终答案均正确；Sonnet 5 的失败属于缺失请求的摘要，不能隐藏成通过。证据 `five-thinking-display-02/`。

连续工具测试开启 adaptive thinking 和 JSON Schema，读和写都使用自动工具选择，没有强制首个工具。五模型的读取、写入、最终 JSON 共 **15/15** 步骤通过；回传了全部 assistant 内容块（包括 thinking/signature），随机代码字符串、路径和最终 JSON 均通过内容断言。客户端仍模拟工具结果，不代表调用了外部文件服务。证据 `five-thinking-multitool-01/`。

新增可复现脚本 `scripts/run-customer-tool-workflow.py`，默认覆盖上述五模型；只请求配置端口的本地容器，凭证从本地配置读取，原始请求/响应落盘，任何步骤失败即停止该模型流程并返回非零退出码。例如：

```sh
python3 scripts/run-customer-tool-workflow.py \
  --config .codex-tmp/customer-runtime/config.json \
  --out .codex-tmp/customer-runtime/five-thinking-multitool-new
```

这些是分项修复验证，不会覆盖或抹去前述人设、普通 JSON 围栏及来源/计量差异；没有把五个模型统称为“全部修好”。

## 尚未解决与验收边界

- 人设服从性和普通 JSON/代码围栏仍有真实失败，未彻底修复。显式 Schema 的最后一轮所测样本通过，但本地校验和围栏适配不能提供原生约束解码的成功率保证；Schema SSE 缓冲也有首字节延迟的取舍。
- Kiro 的隐藏系统提示及优先级仍由 Kiro 控制。本次没有完整复现客户全部逐字提示词探针，不能宣称这些探针已失效。
- 本地签名篡改拒绝证明的是签名登记校验，不等于 Anthropic 原生验签。签名内的模型部署名不应修改。
- 现有代码在上游缺少签名时仍可能生成本地 HMAC 回退签名（`model_uses_local_thinking_signature_fallback` / `generate_model_signature`）；不能宣称所有 thinking signature 均来自上游。本次没有扩展或仿造这类签名。
- Sonnet 4.6 的手动内部预算无法通过已验证的 Kiro 原生字段精确透传。Sonnet 5 的显式 summarized 在直接原生样本中仍没有可读摘要，不会通过补写内容伪装成成功。
- 默认链路仍有自造 message ID、虚拟 usage/cache、组装 invocationMetrics、错误归一化及部分旧身份处理。没有把这些伪装成已修复。
- 代码、JSON 和工具参数中的业务字符串以保真为验收条件。全局删改 Kiro、路径或字符串会破坏用户正常写代码的要求。
- 真正的独立 system、原生签名和原生 usage 需要相应原生 provider。已修复它的透传路由，但本次没有可用原生 AWS 账号实测，不能宣称已验证原生整链路。
- 当前改动仅在独立 worktree 与本地测试实例，未提交、未推送、未部署远端。
