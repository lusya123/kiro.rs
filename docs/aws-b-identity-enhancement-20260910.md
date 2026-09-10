# AWSB 身份增强同步（2026-09-10）

基线为远程普通 `aws-b` 的 `24e8dd9e`，身份增强来源为 `AWS杠B可外接版本` 的 `61bd4b20`。在独立 Worktree `aws-b-identity-sync-20260910` 中选择性迁移源码，没有整体合并外接分支，也没有改写原有历史。

## 同步内容

- 代码身份请求增加 `program`、`script` 的识别，保持既有多语言字符串、JSON 身份字段和业务字面量保护。
- 对明确要求的身份格式及已识别的 Base64、十六进制姓名编码进行校验。
- 仅当上游完整输出了两段符合条件的等价答案，且中间有已识别的纠正说明时，选用其已提供的正确编码答案；不猜补截断内容，不改写冲突业务数据。
- 同步 Opus 响应完成证据检查；错误事件、截断或缺少完成证据不再当成正常结束。
- 身份格式适配器在尚未向客户端输出时，对格式错误或不完整响应最多重试一次，保持模型、输入和 token 预算不变；带工具、工具选择或工具历史的请求不重试。
- 同步 Java 编码、字符数组、JSON 内嵌源码等检测脚本。

上述处理不新增系统提示词，不执行模型生成的程序。有限的静态格式识别不保证任意代码语义正确；无法恢复的格式错误仍返回错误。

## 保留普通 AWSB 策略

`signature.rs`、`cache.rs`、`billing.rs`、`router.rs`、`middleware.rs`、`native_bedrock.rs`、`compat.rs` 和核心 `identity.rs` 与普通 AWSB 基线字节一致。两个入站签名校验函数及 `/v1/messages`、`/cc/v1/messages` 中的调用也保持原样。

没有引入外接版的 `ignore_history_thinking_signatures`。普通 AWSB 仍校验历史 thinking signature；签名生成、原生 Bedrock 路由、账号渠道选择和计费算法不变。新的完成证据检查会阻止不完整响应走正常完成及对应缓存提交路径。

## 验证

- 基线完整 Rust 回归：1025 passed、0 failed、3 ignored。
- 同步后的完整 Rust 回归（含新增签名边界测试）：**1034 passed、0 failed、3 ignored**。
- 新增 56 组真实本地 HTTP 入口断言：Opus 5 / 4.8 × 两个 Messages 入口 × 普通/SSE × 七种签名状态。已登记签名通过签名关卡并到达 provider 选择（未配置 provider，返回 503）；缺失、null、空值、数字、非法 Base64 和修改后的签名仍返回 400。此测试不调用真实模型。
- 已迁移的模拟上游测试覆盖编码纠错、业务字段冲突、JSON/源码格式、重试上限、工具请求不重试、截断/错误帧处理及请求参数保留。

本地日志位于本 Worktree 的 `.codex-tmp/identity-sync-validation/`。本记录的结论是代码和回归验证，不等同于生产实例已更新，也不以之前外接版的真实调用结果冒充本轮 AWSB 实测。
