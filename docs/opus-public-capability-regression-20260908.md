# Opus 5 / Opus 4.8 公共接口回归修复

> 后续已按实时 POMO 响应再次修复，当前行为及验收见 [V1 Messages POMO 对照](pomo-v1-validation-20260908.md)。本文保留历史结果；其中 Opus 5 拒绝 fallbacks、CC 保留这两个 Opus 的 Schema 扩展等描述已被后续实测修正。

2026-09-08。目标按用户最后确认的 `claude-opus-5`、`claude-opus-4-8`。

工作目录：`/Users/xuehongyu/Documents/code/kiro.rs-aws-b/.codex-tmp/aws-b-customer-compat-20260908`。

## 历史验证与根因

实际编译并启动 **2026-09-05 的 `68dd35c1`** 后端，使用独立本地端口 61998，`awsB40Compat=true`、`bedrockMantleEnabled=false`。后端源码未经修改，管理页面复用现有静态构建产物。历史进程已停止。

按截图类别构造请求，每个目标模型分别测试普通和流式：temperature > 1、web_search、异常 role、显式 JSON Schema、非法签名、manual enabled thinking、fallbacks、advisor、code_execution、URL 图片，共 40 条。

| 版本 | 同组检查结果 | 差异 |
| --- | --- | --- |
| 历史 `68dd35c1` | 40/40 | 按历史公共接口契约返回 400 |
| 本 Worktree 修复前 | 36/40 | 两模型的普通/流式 JSON Schema 均返回 200 |
| 本次修复后 | 40/40 | 40 个原始响应体与历史版本逐字节相同 |

三个阶段的 40 个请求体也逐字节相同。非法签名保留历史错误类型 `<nil>`，其余类别为 `invalid_request_error`；没有更改签名错误格式。

历史公共能力校验由 **2026-08-31 的 `926ab941`** 引入，包含拒绝 `output_config.format`。此前本 Worktree 添加 JSON Schema 适配时，删除了该拒绝条件，并在两个入口共用适配器。这是此次实际复现的退化，属于本地未提交改动，与 message ID 或代码身份清洗无关。

截图其他失败项在修复前的本地 `/v1/messages` 已返回预期 400，未复现为退化。另发现历史代码仅在 `bedrock_mantle_provider.is_none()` 时执行公共校验：配置任意原生模型后，其余 Kiro 模型也跳过校验。此问题同时修复；当前本地未启用 Mantle，因此不是本轮四个失败的原因。

## 修改行为

- AWS-B `/v1/messages` 的 Kiro 路由恢复拒绝显式 `output_config.format`，不再进入本地 Schema 生成/验证适配。
- `/cc/v1/messages` 保留 Schema 适配。未携带该字段的普通 JSON、代码和身份清洗继续可用。
- 配置原生模型不再放开其他 Kiro 模型的限制。显式选中的原生模型继续透传，由上游校验。
- Opus 5 / Opus 4.8 的 manual enabled 继续被拒绝，adaptive 继续可用。
- 此次没有新增或修改系统提示词。

修改文件：`src/anthropic/handlers.rs`、`src/anthropic/router.rs`、`README.md`。新增脚本：`scripts/run-public-capability-check.py`。

## 验证结果

- 回归测试先失败后通过；路由覆盖两个目标模型、普通/流式、独立/混合配置、CC 扩展与原生透传。
- Rust 全量测试：**1008 通过，0 失败，3 忽略**；构建及 `git diff --check` 通过。
- 新进程公共接口检查：**40/40 通过**。
- CC 简单/嵌套 Schema 普通/流式：**8/8 通过**，业务代码与路径保持原值。
- Python/JavaScript 代码身份：**6/6 通过**，名称和语法正确；没有执行生成的代码。
- 普通对话、自定义工具参数、adaptive thinking：**6/6 通过**。
- 额外两条无 Schema 的人设 JSON 均返回 Bob，但带 Markdown 围栏，严格纯 JSON 断言失败。原始失败记录保留；该格式问题不属于截图的“显式结构化输出应拒绝”。

截图未提供原测试脚本、模型列、具体请求或完整判定规则。以上是相同可见类别的 HTTP 回归，**不是已重跑并通过截图原测试器的声明**。原生透传用本地模拟上游测试，未使用真实 AWS Bedrock 账号。

## 当前实例与证据

运行本 Worktree 的 `target/debug/kiro-rs`，监听 **127.0.0.1:61999**，PID **88174**。SHA-256：`5ba5d3d276f0cca637746633cf4db2d1e2ac804ad7b89d99050873a6f04599b1`。管理接口确认两份凭证可用。未提交、推送或部署远端。

证据均在本 Worktree 的 `.codex-tmp/customer-runtime/`；请求文件不含鉴权头：

- `public-capability-history-opus5-02/`、`public-capability-before-opus5-02/`、`public-capability-fixed-opus5-03/`：三个阶段的请求、响应及汇总。
- `public-capability-cc-schema-03/`、`public-capability-code-identity-03/`：Schema 与代码身份回归。
- `public-capability-positive-03/`：6 条功能通过和 2 条带围栏 JSON 的严格格式失败。
- `public-capability-red.log`、`public-capability-green.log`、`public-capability-full-tests.log`、`public-capability-build.log`。
- `public-capability-final-verification.json`：字节对比及运行实例记录。

重新检查：

```sh
python3 scripts/run-public-capability-check.py \
  --config .codex-tmp/customer-runtime/config.json \
  --out .codex-tmp/customer-runtime/public-capability-new-run
```
