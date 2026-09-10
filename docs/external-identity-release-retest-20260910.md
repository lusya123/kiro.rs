# 外接版身份清洗发布前复测（2026-09-10）

## 代码与测试对象

远程 `AWS杠B可外接版本` 的基线 `221e36567f588e29d52011010c5497ef8c7da07c` 已包含身份清洗。本次移除 thinking signature 入站校验，没有修改 `identity.rs`、`code_identity.rs`、`structured_output.rs`、`stream.rs` 的身份输出处理，也没有新增系统提示词。

在独立 Worktree 编译运行本地服务，通过真实 `/v1/messages` 请求用户授权导入的一条有效凭证。模型为 `claude-opus-5` 与 `claude-opus-4-8`，均覆盖流式、非流式。没有请求线上 Purecall，也没有修改生产实例。

## 结果

| 批次 | 用例结果数 | 完整通过 | 自我身份泄漏 |
| --- | ---: | ---: | ---: |
| 核心语言、JSON、编码、人设、多轮、业务保留 | 148 | 145 | 已返回响应中 0 |
| Java 扩展 | 24 | 24 | 0 |
| C++、SQL、多轮 JSON 各模型/模式重复两次 | 24 | 24 | 0 |
| 合计 | 196 | 193 | 已返回响应中 0 |

196 是最终用例结果数，多轮用例另有首轮请求。195 项收到 HTTP 200，1 项在客户端 120 秒超时，超时不能作为身份清洗成功的证据。各模型分别 98 项。

覆盖 Python、JavaScript、TypeScript、Rust、Go、C、C++、Java、Ruby、Bash、Swift、PHP、C#、Kotlin、SQL。另测 JSON 字符串、转义键、布尔值、字符数组、码点数组、Base64、十六进制、Unicode 转义、字符串拼接、多轮引用前文名称，以及 Bob 人设和日语介绍。

Java 扩展包括 Base64 解码、字符串拼接、字符数组、JSON 中的 `java_code` 源码字段，以及 Java/JSON 内嵌 Java 的 Bob 人设。24 项全部通过。JSON 内嵌源码在 JSON 解析后还检查 Java 语法，并对内部字符串做静态解码扫描。

测试器不执行模型生成代码。Python 使用 AST，其他代码使用 `tree-sitter-language-pack==0.10.0`；另扫描 Unicode、Base64、十六进制、字符拼接等静态候选值中的身份名称。12 项业务字面量对照按请求保留 `Kiro`、`.kiro/specs` 等业务数据，不把这些内容算作自我身份。

## 未消除的格式与稳定性边界

首轮明确保留三项失败，不以重试覆盖原记录：

- Opus 5 / SQL / 流式：完整 200，名称为 Claude，但代码块被单反引号包围，SQL 解析失败。
- Opus 5 / 多轮 JSON / 流式：完整 200，内容为单反引号包围的 `{"name":"Claude"}`，不符合纯 JSON。
- Opus 4.8 / C++ / 非流式：客户端 120 秒超时。

对这三类用例追加的 24 项全部通过，未改运行代码。尚未获取上述两个格式异常的原始上游帧，不能认定它们由上游还是本地转换产生；本次发布没有声称修复这一偶发格式问题。有限测试也不能保证任意提示、任意编码永不出现身份泄漏。

## 复现与证据

`scripts/run-identity-comprehensive.py` 增加六个 Java 扩展用例，并对指定 JSON 源码字段执行嵌套语法及身份检查。使用本地测试配置文件运行（配置含密钥，不提交）：

```sh
python scripts/run-identity-comprehensive.py --config /absolute/private/config.json \
  --out /absolute/private/java-retest --workers 2 \
  --cases java-base64 java-concat java-char-array json-java-code java-bob json-java-bob
```

私有原始请求、响应、逐项结果位于 Worktree 的 `.codex-tmp/live-signature-20260910-115828/identity-release-{core,java-extra,recheck}/`。Java 嵌套源码复核保存在 `identity-release-java-extra/embedded-audit.json`；原始结果未被覆盖。凭证、API Key、服务日志和原始响应不提交。

签名修复另有 264 组本地 HTTP 集成回归、128 次真实成功请求及 32 个应当返回 400 的非法参数对照，见 [签名真实上游报告](external-thinking-signature-live-20260910.md)。发布前完整 Rust 测试为 1028 passed、0 failed、3 ignored。
