# 外接版 thinking signature 真实上游复测（2026-09-10）

## 结论

在本地重新编译并启动修复后的 Worktree，通过本地管理接口真实导入用户提供附件中的第 1 个 social 凭证。正常业务与签名回放共 128 次，全部返回 HTTP 200，无 thinking signature 400，无 SSE error；文本响应均完整结束。管理接口最终记录该凭证成功 128 次、失败 0 次、仍可用。

另外 32 次非法参数对照全部按预期返回 400，均与签名无关。这说明历史 signature 已不再拦截正常请求，不代表任意错误参数都应该返回 200。

## 运行实例与证据

- 修复分支：`codex/aws-b-external-signature-bypass-20260910`，基于外接分支 `221e3656`。
- 本地地址：`http://127.0.0.1:62210`；TLS sidecar 端口 `62211`；缓存隔离为本地模式。
- 凭证来自用户当次提供的 `10个-kiro-accounts-2026-09-09-112.json`，仅导入第 1 个；管理接口 `POST /api/admin/credentials` 返回 200。
- `cargo build --locked --no-default-features` 与 TLS sidecar 的 `go build` 成功。
- 运行二进制 SHA-256：`46eee8689caa13095314af7ca4af4f0d8ce02e7398177af9ac468947efc8e4cd`。
- `handlers.rs` SHA-256：`3ec4537a52e403546289d0e4bdd969079e63e2e09aaded485acafe70973de01a`。
- 测试后重新核对进程、二进制和源码哈希；实例仍在运行。

配置、凭证及原始请求/响应保存在 Worktree 的受保护、Git 忽略目录 `.codex-tmp/live-signature-20260910-115828/`，汇总为 `summary.json`。复测脚本为 `.codex-tmp/live_signature_probe.py`；它从本地配置读取密钥，不在命令行传递密钥。报告不包含令牌或邮箱。

## 结果

| 测试 | 次数 | HTTP 200 | HTTP 400 | Signature 错误 |
|---|---:|---:|---:|---:|
| Opus 5 / 4.8 首轮真实回答、取得签名 | 2 | 2 | 0 | 0 |
| 两个 Messages 入口的签名回放矩阵 | 96 | 96 | 0 | 0 |
| 普通无历史请求 | 8 | 8 | 0 | 0 |
| 真实工具调用及修改签名后的工具结果回传 | 6 | 6 | 0 | 0 |
| 其他 4 个模型的签名补测 | 16 | 16 | 0 | 0 |
| 非法参数对照（预期拒绝） | 32 | 0 | 32 | 0 |

签名主矩阵覆盖 Opus 5、Opus 4.8 × `/v1/messages`、`/cc/v1/messages` × 流式/非流式 × 12 种签名：原样、修改一个字符、截断、跨模型、缺失、null、空字符串、非法 Base64、极短值、数字、对象、数组。首轮真实结果为 423，96 次后续请求均正确得到加 19 后的 442。

工具测试先要求真实上游生成 `add_numbers` 调用，再回传对应工具结果，并带上修改后的真实 thinking signature；两个 Opus 模型的流式和非流式续接均成功。

补测模型为 Opus 4.5、Sonnet 5、Sonnet 4.6、Haiku 4.5，覆盖 `/v1/messages` 的流式/非流式，以及修改签名和对象类型签名。

非法参数对照覆盖两个 Opus 模型、两个 Messages 入口及流式/非流式：temperature 超过 1、非法 role、该模型不支持的 `thinking.type=enabled`、不支持的 `output_config.format`，均在本地按既有参数规则拒绝。

## 适用范围

这次使用真实凭证和真实 Kiro 上游，验证的是本地修复实例。尚未回放 Purecall 那一次原始失败请求，也未在本次测试中推送代码或部署 Purecall 线上实例。真实上游仍可能因其他参数、权限、额度或服务状态拒绝请求；不能将移除本地 signature 校验等同于保证所有请求永远不返回 400。
