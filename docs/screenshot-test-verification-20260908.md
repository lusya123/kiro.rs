# 截图第 5–15 项核对

> 后续已修复 Opus 5 的 fallback 放行例外，重新验证为 40/40；扩展 JSON/代码身份过滤验证为 88/88。见 [最新修复报告](identity-and-fallback-fix-20260908.md)。下面保留修复前的实测记录。

2026-09-08，重新请求指定 Worktree 正在运行的 `http://127.0.0.1:61999/v1/messages`，目标为 `claude-opus-5`、`claude-opus-4-8`。PID 98971；二进制 SHA-256 为 `17f1edc8d6a323574d6a36767a3fd273079cc1b4bd0c74326e2a586ae98895df`。

**按截图“不支持”表示应拒绝该参数/能力的解释，当前仍不能全部通过：Opus 5 的第 11 项接受请求并返回 200。** 两个模型的其余 9 类负向能力检查均返回预期的 400。每类分别请求普通和流式接口；不把 200 的 fallback 响应计为截图测试通过。

| 编号 | 截图项目 | 对应请求 | Opus 5 普通/流式 | Opus 4.8 普通/流式 |
| --- | --- | --- | --- | --- |
| 5 | temperature > 1 | `temperature: 1.1` | 400 / 400，通过 | 400 / 400，通过 |
| 6 | web_search | `web_search_20250305` 服务端工具 | 400 / 400，通过 | 400 / 400，通过 |
| 7 | 异常 role | `role: invalid` | 400 / 400，通过 | 400 / 400，通过 |
| 8 | 结构化输出 | `output_config.format.type: json_schema` | 400 / 400，通过 | 400 / 400，通过 |
| 9 | 错误签名 | 历史 thinking 块内的无效 signature | 400 / 400，通过 | 400 / 400，通过 |
| 10 | thinking enabled | `enabled`，budget 1024，max_tokens 2048 | 400 / 400，通过 | 400 / 400，通过 |
| 11 | server-side-fallback | `fallbacks: default` 和对应 beta header | **200 / 200，不通过** | 400 / 400，通过 |
| 12 | advisor | `advisor_20260301` 服务端工具 | 400 / 400，通过 | 400 / 400，通过 |
| 13 | code_execution | `code_execution_20260521` 服务端工具 | 400 / 400，通过 | 400 / 400，通过 |
| 14 | URL 图片 | `image.source.type: url`，有效 PNG URL | 400 / 400，通过 | 400 / 400，通过 |
| 15 | 模型身份指纹 | JSON 默认身份/Bob，Python/JavaScript 身份 | 已测样本通过 | 已测样本通过 |

负向能力检查按上述截图解释为 **38/40**：Opus 5 为 18/20，Opus 4.8 为 20/20。14 项 JSON/代码身份样本均没有自报 Kiro，默认 Claude，应用人设 Bob；代码语法检查通过，未执行生成代码。JSON 可能带 Markdown 围栏，此处只核验身份语义。

此前“84/84”使用的是当前模型专属兼容规则，其中 Opus 5 的 fallbacks 以 POMO 实测的 200 为预期；它不是截图的通过分数。本轮保留原规则结果，另生成 `screenshot-verdict.json`，明确记录这两条失败。为了不擅自反转用户此前要求的 POMO 兼容行为，本轮没有修改业务代码、提示词或运行配置。

截图没有给出原始请求、响应判定代码、三行分别对应的模型名称。这里的“通过”限于表内对应请求；第 15 项只覆盖已列出的身份样本，不能据此认证完整供应商指纹或所有隐藏提示探针。

本地证据：

- `.codex-tmp/customer-runtime/screenshot-verification-08/`：40 组原始请求/响应，及独立的 `screenshot-verdict.json`。
- `.codex-tmp/customer-runtime/screenshot-json-identity-08/`：8 组原始请求/响应，及 `validated-semantics.json`。
- `.codex-tmp/customer-runtime/screenshot-code-identity-08/`：6 组原始请求/响应和语法判定。
- 上一轮新凭证测试中的 12 次真实签名篡改拒绝证据仍保留在 `selected-credential-07/tools-signatures/`。
