# 外接版 thinking signature 与 Purecall 400 排查

## 基线与定位

- 基线：远程 `AWS杠B可外接版本` 的 `221e3656`。
- 修复分支：`codex/aws-b-external-signature-bypass-20260910`。
- Purecall 提供的症状为 `status_code=400, Invalid request. Check the request parameters.`，尚无失败请求体、模型、时间或对应上游日志，不能据此认定线上错误一定由签名触发。
- 旧版 `/v1/messages` 和 `/cc/v1/messages` 在 provider 选择前调用签名校验：未知签名只在特定条件下导入；空值、格式异常及登记值的近似修改仍被拒绝。这是真实存在的本地 400 来源。
- Kiro 转换器并不传递这些历史签名；其 `ContentBlock.signature` 的字符串类型还会使非字符串签名导致整块历史 thinking 被跳过。
- 项目中没有上述完整通用文案。参数校验、上游拒绝、网站错误包装仍可能产生其他 400；签名校验移除不会把这些错误伪装成成功。

## 修复

两个 Messages 入口共享的 Kiro 适配路径先丢弃顶层 thinking 内容块中的 `signature` 字段。该字段不再经过格式、Base64、长度、登记、导入或篡改校验，也不作为拒绝请求的条件。删除了旧入口校验函数及要求入口拒绝签名的旧测试。

只忽略签名元数据：历史 thinking 文本、工具调用 ID、工具参数中的业务 `signature` 都保留。响应签名生成、原生签名返回、普通参数和 API Key 认证逻辑不修改；没有添加系统提示词。显式原生 Bedrock 路径在本适配之前返回，继续保持请求字节透传，因此真实上游自身的签名校验不受此本地修复控制。

## 验证

HTTP 回归运行真实路由、认证、请求适配、Kiro provider 和响应转换，仅以本地 EventStream 服务替代收费模型上游。覆盖：

- 6 个模型：Opus 5、Opus 4.8、Opus 4.5、Sonnet 5、Sonnet 4.6、Haiku 4.5。
- `/v1/messages`、`/cc/v1/messages`，各自流式和非流式。
- 11 类签名：缺失、null、空字符串、非法 Base64、极短值、截断值、数字、对象、数组、已登记签名、已登记签名的修改值；包含跨模型回放。
- 共 264 组 HTTP 请求，同时检查返回 200、正常结束、历史 thinking 文本、工具 ID 与业务签名字段保留，每个成功请求仅调用一次上游。

结果：旧版 240 组被签名校验拒绝、24 组成功；修复后 264/264 成功。完整 `cargo test --locked --no-default-features`：1028 passed、0 failed、3 ignored。编译 `cargo build --locked --no-default-features` 成功。

复现命令：

```sh
cargo test --locked --no-default-features external_signature_replay_preserves_history_through_http_and_upstream -- --nocapture
cargo test --locked --no-default-features
```

原始本地验证日志位于 Worktree 的 `.codex-tmp/signature-bypass-validation/`。以上是模拟上游阶段的本地集成结果。后续已使用用户新提供的真实凭证完成 128 次成功请求，详见 [真实上游复测报告](external-thinking-signature-live-20260910.md)。Purecall 那一次报错的归因仍需脱敏请求或对应服务日志。
