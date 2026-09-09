# 代码格式的身份清洗修复

工作目录：`/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908`。

## 问题和修复

2026-09-08 在当前本地实例实测六个模型，要求用 Python / JavaScript 输出自己的身份。18 个请求全部返回 200，但其中 7 个仍生成 `assistant_name = "Kiro"` 或 `const assistantName = "Kiro"`，全部出现在带 Bob 应用人设的请求中。

原因：客户人设请求绕过旧身份清洗；后续新增的完整输出清洗只处理 JSON。代码回答没有进入该路径。

现在将该路径扩展为格式化身份输出处理：

- 明确询问当前助手自身身份并要求代码时，应用人设不再导致清洗缺失。客户提供 Bob 等有效应用名时使用该名字，没有应用人设时沿用 Claude。
- 在源代码字符串中处理完整身份标签，复用既有自我身份声明清洗规则处理相关字符串和注释。支持普通引号、三引号、模板字符串、常见 Unicode / 十六进制转义和大小写变体。应用名里的引号、反斜线和模板插值标记会转义。
- 不对整个代码文件全局替换。路径、代码示例及明确的产品/业务字面量保留；普通代码任务和第三方产品资料不进入身份专用路径。
- 清理代码后附带的“我是 Kiro、不接受 Bob 人设”说明。
- 普通响应及 SSE 采用相同规则。身份代码的 SSE 会缓冲后处理，避免身份字符串或转义跨分片泄漏；`max_tokens` 截断仍保留其终止语义，并处理已返回的身份字符串。
- 只改响应 text 字段，不改变 thinking、signature、tool 参数、message ID、usage 或原生 Bedrock 透传响应。

改动涉及 `code_identity.rs`、`identity.rs`、`handlers.rs`、`structured_output.rs` 及模块注册。原有 JSON 身份清洗及 Schema 验证保留。

## 验证结果

| 检查 | 结果 |
| --- | --- |
| 复现原问题的回归测试 | 修改前 2 项失败，修复后通过 |
| 代码转义、普通业务字面量、注释、逐字符 SSE、截断和非文本元数据 | 6 项定向回归通过 |
| 完整 Rust 测试 | 1007 通过、0 失败、3 跳过 |
| 本地构建 | 成功；仍有既有 80 项 unused / dead-code 等警告 |
| 原 18 个真实请求重放 | **18/18 通过，0 个返回文本包含 Kiro** |
| Python 源码解析 / JavaScript `node --check` | 全部通过；未执行模型生成的代码 |

六模型为 Sonnet 4.6、Sonnet 5、Opus 5、Opus 4.7、Opus 4.8、Haiku 4.5。每个模型覆盖无客户人设的 Python、Bob 人设的 Python，以及 Bob 人设的中文 JavaScript 流式请求。

复测后典型输出：

```python
assistant_name = "Bob"
print(assistant_name)
```

```javascript
const assistantName = "Bob";
console.log(assistantName);
```

证据保存在 Git 忽略目录 `.codex-tmp/customer-runtime/`：

- `code-identity-audit-01/`：修改前原始请求/响应，7/18 出现 Kiro。
- `code-identity-fixed-02/`：同一批原始请求重放，18/18 通过。
- `code-identity-red.log`、`code-identity-green.log`、`code-identity-edge-tests.log`。
- `code-identity-full-tests.log`、`code-identity-build.log`。

复测脚本：

```sh
python3 scripts/run-code-identity-check.py \
  --config .codex-tmp/customer-runtime/config.json \
  --requests-dir .codex-tmp/customer-runtime/code-identity-audit-01 \
  --out .codex-tmp/customer-runtime/<new-output-directory>
```

配置与凭证不提交。当前本地实例 PID `78387`，监听 `127.0.0.1:61999`，二进制为本 Worktree 的 `target/debug/kiro-rs`。没有修改远端服务。

## 范围

本次解决代码自报身份路径中已经复现的问题；不是删除所有正常业务数据中的 `Kiro`，也不改变真实上游、usage 或签名语义。代码身份输出路径会增加缓冲，沿用 32 MiB / 360 秒上限。上述测试覆盖所列代码形式，不代表对任意程序、任意身份探测问法进行了穷尽验证。
