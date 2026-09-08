# JSON/代码身份过滤与 fallback 修复

> 后续截图复测发现普通文字人设仍有遗漏，已继续修复；扩展测试中的 JSON 空回复仍未解决，见 [后续复测报告](screenshot-retest-persona-fix-20260908.md)。下文保留本轮历史结果。

2026-09-08，在指定 Worktree 修改、构建并运行，主入口为 `/v1/messages`，目标模型为 Opus 5 和 Opus 4.8。

## 实际复现

使用用户提供的第 1 个凭证，隔离实例只加载这个账号。扩展测试覆盖 22 种请求 × 2 模型 × 普通/流式，共 88 项。修复前结果为 **81/88**，其中：

- Opus 4.8 的 JSON `assistantName` 字段在普通和流式响应中各泄漏一次 Kiro。
- Opus 4.8 的 Python 普通响应返回 `print("Ki" + "ro")`，代码执行后的身份仍是 Kiro。测试只静态解析字符串，没有执行生成代码。
- 两模型的 4 个布尔 JSON 请求被本地快捷回复改成普通自我介绍，没有遵循请求格式。这 4 项是格式错误，不是 Kiro 身份泄漏。

原始响应及最初的失败结果保存在 `identity-expansion-09/before/`，未覆盖。

第一轮修复后的 88 项曾全部通过，但额外复查又发现 1 条遗漏：Python 代码已变成 `print("Bob" + "")`，代码块后仍附加 `My persona name is Kiro, not Bob...`。这条失败保存在 `repeat/`，随后继续修复，未将它隐藏或计为通过。

## 修复

沿用现有响应身份过滤规则，**本次没有新增或修改系统提示词**。

1. JSON 识别驼峰/下划线字段、中文名称字段、根数组和根字符串，以及 `profile` / `assistant` 等身份对象。仅替换身份值，保留原字段名、空白、数字字面量和业务对象。
2. 代码过滤先静态读取字符串字面量，再识别直接 `+` 拼接、Python 相邻字符串、Unicode 码点转义、raw string 和 heredoc。替换后的代码保持语法有效，不解释或执行动态代码。
3. 复用既有自称过滤规则处理 `Hello, I am Kiro` 等带前缀的自我介绍。指定应用人设 Bob 时输出 Bob，默认输出 Claude。
4. JSON 身份请求不会再被不符合 JSON 格式的本地快捷回复截断；由模型生成所需格式后，再经过身份过滤。
5. 普通/流式共用过滤；流式文本重组后处理，逐字符分片测试也通过。签名、工具参数、消息 ID 和其他非文本元数据保持原样。
6. 过滤 JSON/代码块外附加的自我身份说明，即使正文中的名称已经正确也会处理。无关的计算结果等正文保留，不因出现补充文字就放弃 JSON 字段过滤。

另外，删除 Opus 5 的 `fallbacks` 放行例外。因为项目没有实现服务端模型回退，两个目标模型的两个 Messages 入口现在都拒绝非 null 的 `fallbacks`，返回 400 `invalid_request_error`。省略该字段、传 null 或只携带 beta header，仍可正常请求。这里有意区别于此前 POMO 的“接受但忽略”行为，按用户本次要求恢复截图中的拒绝边界。

## 修复后验证

| 检查 | 结果 |
| --- | --- |
| 同一批 88 项 JSON/代码/业务保留测试 | **88/88** |
| 对发生过遗漏的 Bob JSON/拼接代码再复查 | **8/8** |
| 截图第 5–14 项对应的参数校验，普通/流式 | **40/40**，包括 Opus 5 fallback 返回 400 |
| 工具调用、ID、续聊和真实签名篡改 | **20/20**，12 次修改/清空签名全部拒绝 |
| 正常问答、有参数自定义工具、adaptive thinking | **6/6** |
| Rust 测试 | **1014 成功，0 失败，3 忽略** |

88 项由 80 条身份请求和 8 条业务保留对照组成。身份请求全部没有自报 Kiro；业务对照中的 `Kiro`、`.kiro/specs` 等用户原文被保留。JSON 可带 Markdown 围栏，测试解析围栏内的 JSON，并不把这当成裸 JSON 格式保证。

覆盖的形式：普通/驼峰/中文字段、嵌套 JSON、数组、字符串、布尔值、Unicode 转义；Python 赋值、字典、加号与相邻字符串拼接；JavaScript 模板、Unicode 码点转义与拼接；Rust raw string；Bash heredoc。Python 使用 AST、JavaScript 使用 `node --check`、Rust 使用 rustfmt 解析、Bash 使用 `bash -n`；均未执行模型生成代码。

修复前后单元/路由回归均保留证据：fallback 先失败再通过；格式化身份的逐字符流式样本先失败再通过；布尔 JSON 的本地快捷回复先失败再通过。

## 运行和证据

主服务为本 Worktree 的 `target/debug/kiro-rs`，`127.0.0.1:61999`，PID **14319**。最终构建 SHA-256：

`847d728784fb2f70005e6fa373005c3c0e08163532c48aec7874609fa8404bf7`

所有请求证据位于 `.codex-tmp/customer-runtime/`：

- `identity-expansion-09/before/`、`after/`、`final/`：原始、首轮修复、最终修复的相同 88 项请求及未经编辑的响应。
- `identity-expansion-09/repeat/`：额外复查发现代码块外身份说明的失败证据。
- `identity-expansion-09/repeat-final/`：最终额外复查 8/8。
- `identity-expansion-09/tools-signatures-final/`、`identity-positive-final-09/`。
- `fallback-capability-before-09/`：38/40，原有 Opus 5 两条 fallback 返回 200。
- `fallback-capability-final-09/`：40/40，现在全部按负向参数校验标准判断。
- `identity-expansion-09/full-tests-03.log`、`build-final.log`、`final-runtime.json`、`final-verification.json`。

可复现脚本：`scripts/run-identity-variants.py`、`scripts/run-public-capability-check.py`、`scripts/run-v1-tool-signature-check.py`。凭证只保存在 Git 忽略目录中，没有进入测试请求文件。

结论限于已列出的请求、格式和静态字符串形式；不是对任意动态程序、任意编码或截图未知原脚本的保证。修复改变响应中的自称，不改变实际上游服务来源，也不表示已获得原生 Anthropic 验签能力。
