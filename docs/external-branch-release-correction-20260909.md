# 外接分支发布纠正（2026-09-09）

本次发布目标为 `AWS杠B可外接版本`，容器别名为 `ghcr.io/lusya123/kiro-rs:aws-b-external`。

在外接分支 `900e0bc4` 上合入客户兼容修复 `7deedb81` 至 `510faa98`。保留既有 Responses/GPT 修复及外接入口的签名导入策略：结构合法但未登记的外部签名继续进入 provider 选择，畸形签名在入口拒绝。`src/anthropic/signature.rs` 未修改。身份、JSON/代码格式及响应完整性修复来自已提交快照；原客户 Worktree 中并行进行的未提交修改不包含在本次发布中。

误推到普通 `aws-b` 的 `510faa98` 使用普通 revert 提交恢复到其父提交 `5b95e8cd` 的完整文件树，不重写远程历史。早期已经发布到 `aws-b` 的修复不在本次撤销范围内。

验证：

- `cargo test --locked --no-default-features`：1032 passed，0 failed，3 ignored。
- 验证外接分支手动/自动发布均使用 `aws-b-external`，普通分支使用 `aws-b`，候选分支不写移动别名。
- 保留 CI 的 Rust 全量测试门禁、amd64/arm64 构建和 revision/version 镜像标签。

本次验证为分支集成回归；没有重新调用真实模型。此前真实模型测试中剩余的 3 次格式失败及账号限制详见 `format-base64-retest-20260909.md`，本次发布不宣称这些问题已经全部解决。
