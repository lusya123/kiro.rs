# POMO ID 与 JSON 身份输出兼容修复（2026-09-08）

> 最新 POMO 实测更新：Opus 5 的 message ID 使用 256-bit Base32；Opus 5 / 4.8 的两个 Messages 入口均执行能力校验。最新结果见 [V1 Messages POMO 对照](pomo-v1-validation-20260908.md)。

> 最新公共接口变更：已恢复 `/v1/messages` 对显式 `output_config.format` 的拒绝，本地 Schema 适配保留于 `/cc/v1/messages`，见 [Opus 公共接口回归修复](opus-public-capability-regression-20260908.md)。下文为此前历史结果。

> 后续代码格式修复及最新运行状态见 [代码身份清洗报告](code-identity-compat-20260908.md)：原 18 个代码身份请求已全部通过，原先 7 个 Kiro 自称均已消除。本文保留前一轮 JSON/ID 修复的历史证据。

工作目录：`/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908`。保留了此前未提交的客户兼容修复。本次只更新本地代码和本地 61999 实例。

## POMO 实际规律

参考数据是当天保存的真实 POMO 响应，另外重新请求 Sonnet 4.6 / Haiku 4.5 各种普通、流式和工具响应，共 6 个新样本。没有把 POMO 配成业务上游。

| 字段 / 模型 | 观察到的格式 | 本地实现 |
| --- | --- | --- |
| Sonnet 4.6 message ID | `msg_bdrk_01` + 固定 22 位 Bitcoin 字母表 Base58，解码为 UUIDv7 | `Uuid::now_v7()`，大端 128 位编码为 22 位 Base58；同进程按生成顺序递增 |
| Haiku 4.5 message ID | `msg_bdrk_` + 52 位小写 RFC 4648 Base32，解码为 32 字节 | 32 随机字节、小写 Base32、无 padding |
| 工具 ID | `toolu_bdrk_01` + 22 位 Base58 | 已有该形态的上游 ID 原样保留；Converse `tooluse_…` 确定性映射到该形态 |

可复核的 POMO 样本：

- `msg_bdrk_011CeqQaFreWDrY7uDPP1193` → UUID `01a07f74-3397-7758-818b-e1d730e5e9f2` → `2026-09-08T05:18:36.951Z`。
- `msg_bdrk_011CeqQaWPWhrhrChA3Robdd` → UUID `01a07f74-40de-761c-aa29-2adf11c68844` → `2026-09-08T05:18:40.350Z`。
- 新采样 Sonnet 普通和流式 ID 同样解码为请求期间的 UUIDv7；Haiku 新样本均为 52 位 Base32。

因此 `011Ceq…` 是时间变化产生的前缀，不能写死成 `011C` 后随机生成。Base58 字母表为 `123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz`，排除 `0/O/I/l`。

POMO 没有公开其生成器源码；上述结论区分可解码的结构与不可证明的内部算法。Haiku 的 32 字节载荷未发现可用的时间字段，本地匹配其编码和长度，不声称恢复了 POMO 的随机/散列算法。较新 Opus / Sonnet 模型在此前 POMO 对照中无可用渠道，本地采用 Claude 的 UUIDv7 规则，尚不能声称完成这些型号的实时 POMO 同渠道验证。

工具映射使用上游 ID 的 SHA-256 前 128 位再编码，不修改工具名称、参数或客户回传结果。映射是确定性的，不需要进程内表或过期时间。Kiro 请求包含完整历史：assistant tool_use 与后续 tool_result 都使用客户端看到的同一 ID。原生 Bedrock provider 保持整条响应透传。

## JSON 身份输出

原问题是 Claude 应用人设及 JSON Schema 请求全部绕过旧身份替换。因此有效 JSON `{"name":"Kiro"}` 会通过 Schema 校验。

新增一条仅针对明确询问助手自身身份的 JSON 输出路径：

1. 沿用现有请求身份分类和客户应用人设提取。
2. 解析完整 JSON，再对 `name` / `persona` / `self_name` / `assistant_name` 等身份字段，以及已有规则支持的供应商、运行环境和身份布尔字段，调用现有清洗规则。客户提供 Bob 等有效人设时，名字替换为该应用名；否则沿用 Claude 身份规则。
3. 按原始 JSON 中字段值的位置替换。JSON key、业务字段、代码、路径、数字字面量和空白保持原样。不会全局替换字符串 `Kiro`。`identity` / `assistant` / `self` 对象中的身份字段也覆盖；任意业务对象不递归清洗。
4. JSON 后若仅追加身份/人设拒绝说明，识别该说明并保留清洗后的 JSON；包含其他任务结果的尾部文本不会被丢弃。
5. SSE 身份 JSON 缓冲完成后再处理，支持字符或 Unicode 转义跨分片。只替换 text 字段；thinking、signature、tool 参数、ID、usage 的原始响应字节保留。随后继续执行已有 Schema 验证。

仅身份 JSON 请求增加缓冲（360 秒 / 32 MiB 上限）。错误、截断、拒绝和工具中间轮次保留原语义。普通业务 JSON、代码任务、第三方产品资料以及显式原生 Bedrock 路由不会进入新路径。

## 验证与证据

原始请求/响应在 Git 忽略目录 `.codex-tmp/customer-runtime/`。配置和凭证不提交。

- `pomo-id-reference-02/`：本次 6 项 POMO 原始参照。
- `pomo-envelope-fixed-01/`：第一轮六模型测试，58 个实际请求、54 通过。所有 HTTP 200 响应的 ID 编码检查通过；35/36 身份请求通过，Sonnet 5 的“JSON + 拒绝人设说明”暴露了新边界并补回归修复。其他失败为 Sonnet / Haiku 非流式无参数工具未返回工具块、Haiku 一条工具续聊 502；原记录保留，没有重写成成功。
- `pomo-envelope-sonnet-recheck-02/`：上述非流式无参数工具问题在同请求复测中仍出现。
- `pomo-json-business-01/`：Sonnet 4.6、Opus 4.8、Haiku 4.5 的普通/流式简单及嵌套 Schema 12/12 通过，Kiro 字面量、代码和路径保留。
- 新增回归先失败后通过：时间可解码的 message ID、工具分片的 ID、Haiku 编码、Unicode 转义身份字段、JSON 后的人设拒绝说明。已有 native provider 字节透传与签名相关回归继续执行。

最终版本验证：

- `cargo test --no-default-features --quiet`：**1001 通过，0 失败，3 跳过**；日志 `pomo-json-final-tests-02.log`。
- `cargo build --no-default-features` 成功，仍有既有的 80 项 unused / dead-code 等警告；日志 `pomo-json-final-build.log`。
- `pomo-identity-final-03/`：六模型 × 应用名 / 默认名 / 普通 JSON × 普通 / SSE，**35/36 通过**。全部成功响应的名字与 ID 格式正确；Haiku 的一条中文流式 Schema 请求为 502。没有把 502 当成通过，不能据此声称该上游的格式服从已稳定。
- 首轮工具验证：12 个调用请求，10 个返回工具块，**这些工具 ID 10/10 符合 POMO 格式**；10 个续聊中 9 个成功，另一个是 Haiku 的 Schema 502。未返回工具的 2 项不能验证 ID，仍然计失败。
- 最终进程 PID `76266`，监听 `127.0.0.1:61999`；可执行文件位于本 Worktree 的 `target/debug/kiro-rs`。启动后管理接口 200，两份已导入凭证可用。
- `git diff --check` 和新实测脚本的 Python 语法检查通过。

复测入口：`scripts/run-pomo-envelope-check.py --config <local-config> --out <new-directory> --checks identity`。`--checks tools` 验证工具 ID 和工具结果续聊，结果由测试客户端模拟，不实际运行外部工具。

## 边界

本次是可见 ID 和身份输出的兼容处理，不会把 Kiro 上游变成 Anthropic / Bedrock 原生服务。上游隐藏指令、人设服从程度、usage 重算、invocationMetrics 拼装、错误包装及 Anthropic 原生验签语义仍不由这次修改解决。签名没有被重写。无参数工具调用和工具后的模型输出格式问题也没有被 ID 映射掩盖为成功。
