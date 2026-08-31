# kiro-rs

一个用 Rust 编写的 Anthropic Claude API 兼容代理服务，将 Anthropic API 请求转换为 Kiro API 请求。

---

<table>
<tr>
<td>
<b>特别感谢</b>：<a href="https://co.yes.vg/register?ref=hank9999">YesCode</a> 为本项目提供了 AI API 额度赞助, YesCode 作为一家低调务实的 AI API 中转服务商 <br>
长期以来提供稳定高可用的服务, 如您有意体验, 请点击链接注册体验 → <a href="https://co.yes.vg/register?ref=hank9999">立即访问</a>
</td>
</tr>
</table>

---

#### [LINUX DO 讨论帖](https://linux.do/t/topic/1571986)

## 免责声明

本项目仅供研究使用, Use at your own risk, 使用本项目所导致的任何后果由使用人承担, 与本项目无关。
本项目与 AWS/KIRO/Anthropic/Claude 等官方无关, 本项目不代表官方立场。

## 注意！

因 TLS 默认从 native-tls 切换至 rustls，你可能需要专门安装证书后才能配置 HTTP 代理。可通过 `config.json` 的 `tlsBackend` 切回 `native-tls`。
如果遇到请求报错, 尤其是无法刷新 token, 或者是直接返回 error request, 请尝试切换 tls 后端为 `native-tls`, 一般即可解决。

**Write Failed/会话卡死**: 如果遇到持续的 Write File / Write Failed 并导致会话不可用，参考 Issue [#22](https://github.com/hank9999/kiro.rs/issues/22) 和 [#49](https://github.com/hank9999/kiro.rs/issues/49) 的说明与临时解决方案（通常与输出过长被截断有关，可尝试调低输出相关 token 上限）

## 功能特性

- **Anthropic API 兼容**: 完整支持 Anthropic Claude API 格式
- **流式响应**: 支持 SSE (Server-Sent Events) 流式输出
- **Token 自动刷新**: 自动管理和刷新 OAuth Token
- **多凭据支持**: 支持配置多个凭据，按优先级自动故障转移
- **负载均衡**: 支持 `priority`（按优先级）和 `balanced`（均衡分配）两种模式
- **智能重试**: 单凭据最多重试 3 次，单请求最多重试 9 次
- **凭据回写**: 多凭据格式下自动回写刷新后的 Token
- **Thinking 模式**: 支持 Claude 的 extended thinking 功能
- **工具调用**: 完整支持 function calling / tool use
- **WebSearch**: 内置 WebSearch 工具转换逻辑
- **多模型支持**: 支持 Sonnet、Opus、Haiku 系列模型
- **Admin 管理**: 可选的 Web 管理界面和 API，支持凭据管理、余额查询等
- **多级 Region 配置**: 支持全局和凭据级别的 Auth Region / API Region 配置
- **凭据级代理**: 支持为每个凭据单独配置 HTTP/SOCKS5 代理，优先级：凭据代理 > 全局代理 > 无代理

---

- [开始](#开始)
  - [1. 编译](#1-编译)
  - [2. 最小配置](#2-最小配置)
  - [3. 启动](#3-启动)
  - [4. 验证](#4-验证)
  - [Docker](#docker)
- [配置详解](#配置详解)
  - [config.json](#configjson)
  - [credentials.json](#credentialsjson)
  - [Region 配置](#region-配置)
  - [代理配置](#代理配置)
  - [认证方式](#认证方式)
  - [环境变量](#环境变量)
- [API 端点](#api-端点)
  - [标准端点 (/v1)](#标准端点-v1)
  - [Claude Code 兼容端点 (/cc/v1)](#claude-code-兼容端点-ccv1)
  - [Thinking 模式](#thinking-模式)
  - [工具调用](#工具调用)
- [模型映射](#模型映射)
- [Admin（可选）](#admin可选)
- [注意事项](#注意事项)
- [项目结构](#项目结构)
- [技术栈](#技术栈)
- [License](#license)
- [致谢](#致谢)

## 开始

### 1. 编译

> PS: 如果不想编辑可以直接前往 Release 下载二进制文件

> **前置步骤**：编译前需要先构建前端 Admin UI（用于嵌入到二进制中）：
> ```bash
> cd admin-ui && pnpm install && pnpm build
> ```

```bash
cargo build --release
```

### 2. 最小配置

创建 `config.json`：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1"
}
```
> PS: 如果你需要 Web 管理面板, 请注意配置 `adminApiKey`

创建 `credentials.json`（从 Kiro IDE 等中获取凭证信息）：
> PS: 可以前往 Web 管理面板配置跳过本步骤
> 如果你对凭据地域有疑惑, 请查看 [Region 配置](#region-配置)

Social 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social"
}
```

IdC 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "idc",
   "clientId": "你的clientId",
   "clientSecret": "你的clientSecret"
}
```

### 3. 启动

```bash
./target/release/kiro-rs
```

或指定配置文件路径：

```bash
./target/release/kiro-rs -c /path/to/config.json --credentials /path/to/credentials.json
```

### 4. 验证

```bash
curl http://127.0.0.1:8990/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-qazWSXedcRFV123456" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      {"role": "user", "content": "Hello, Claude!"}
    ]
  }'
```

### Docker

也可以通过 Docker 启动：

```bash
docker-compose up
```

需要将 `config.json` 和 `credentials.json` 挂载到容器中，具体参见 `docker-compose.yml`。

## 配置详解

### config.json

| 字段 | 类型 | 默认值 | 描述 |
|------|------|--------|------|
| `host` | string | `127.0.0.1` | 服务监听地址 |
| `port` | number | `8080` | 服务监听端口 |
| `apiKey` | string | - | 自定义 API Key（用于客户端认证，必配；须为非空可见 ASCII） |
| `region` | string | `us-east-1` | AWS 区域 |
| `authRegion` | string | - | Auth Region（用于 Token 刷新），未配置时回退到 region |
| `apiRegion` | string | - | API Region（用于 API 请求），未配置时回退到 region |
| `kiroVersion` | string | `0.9.2` | Kiro 版本号 |
| `machineId` | string | - | 自定义机器码（64位十六进制），不定义则自动生成 |
| `systemVersion` | string | 随机 | 系统版本标识 |
| `nodeVersion` | string | `22.21.1` | Node.js 版本标识 |
| `tlsBackend` | string | `rustls` | TLS 后端：`rustls` 或 `native-tls` |
| `countTokensApiUrl` | string | - | 外部 count_tokens API 地址 |
| `countTokensApiKey` | string | - | 外部 count_tokens API 密钥 |
| `countTokensAuthType` | string | `x-api-key` | 外部 API 认证类型：`x-api-key` 或 `bearer` |
| `proxyUrl` | string | - | HTTP/SOCKS5 代理地址 |
| `proxyUsername` | string | - | 代理用户名 |
| `proxyPassword` | string | - | 代理密码 |
| `adminApiKey` | string | - | Admin API 密钥（须为非空可见 ASCII），配置后启用凭据管理 API 和 Web 管理界面 |
| `loadBalancingMode` | string | `priority` | 负载均衡模式：`priority`（按优先级）或 `balanced`（均衡分配） |
| `extractThinking` | boolean | `true` | 非流式响应的 thinking 块提取。启用后 `<thinking>` 标签会被解析为独立的 `thinking` 内容块 |
| `awsB40Compat` | boolean | `true` | 使用 POMO 上游兼容的模型目录、正文、错误、ID、签名和 SSE 外形；不伪造外层 New API/WAF 响应头。设为 `false` 时使用 AWS-P 外观 |
| `defaultEndpoint` | string | `ide` | 默认 Kiro 端点。凭据未显式指定 `endpoint` 时使用。当前支持：`ide` |

完整配置示例：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1",
   "tlsBackend": "rustls",
   "kiroVersion": "0.9.2",
   "machineId": "64位十六进制机器码",
   "systemVersion": "darwin#24.6.0",
   "nodeVersion": "22.21.1",
   "authRegion": "us-east-1",
   "apiRegion": "us-east-1",
   "countTokensApiUrl": "https://api.example.com/v1/messages/count_tokens",
   "countTokensApiKey": "sk-your-count-tokens-api-key",
   "countTokensAuthType": "x-api-key",
   "proxyUrl": "http://127.0.0.1:7890",
   "proxyUsername": "user",
   "proxyPassword": "pass",
   "adminApiKey": "sk-admin-your-secret-key",
   "loadBalancingMode": "priority",
   "extractThinking": true,
   "awsB40Compat": true
}
```

AWS-B 与 AWS-P 共用 tokenizer、计费、缓存、身份清洗、流式处理和工具链。`awsB40Compat`
只控制对外协议外观，不切换上游模型或内部质量引擎。

当 AWS-B 作为 New API 的上游渠道时，`Server`、`X-Oneapi-Request-Id`、
`X-New-Api-Version`、`X-Request-Id` 等站点或网关身份应由外层 New API、反向代理和
WAF 生成。AWS-B 的 POMO 兼容模式会移除这些本地伪造头，避免外层 New API 复制到
最终客户响应后出现两套网关指纹；`Content-Type`、上游 Bedrock 限流信息等业务头不受
影响。

### credentials.json

支持单对象格式（向后兼容）或数组格式（多凭据）。

#### 字段说明

| 字段             | 类型     | 描述                                          |
|----------------|--------|---------------------------------------------|
| `id`           | number | 凭据唯一 ID（可选，仅用于 Admin API 管理；手写文件可不填）        |
| `accessToken`  | string | OAuth 访问令牌（可选，可自动刷新）                        |
| `refreshToken` | string | OAuth 刷新令牌                                  |
| `profileArn`   | string | AWS Profile ARN（可选，登录时返回）                   |
| `expiresAt`    | string | Token 过期时间 (RFC3339)                        |
| `authMethod`   | string | 认证方式：`social` 或 `idc`                       |
| `clientId`     | string | IdC 登录的客户端 ID（IdC 认证必填）                     |
| `clientSecret` | string | IdC 登录的客户端密钥（IdC 认证必填）                      |
| `priority`     | number | 凭据优先级，数字越小越优先，默认为 0                         |
| `region`       | string | 凭据级 Auth Region, 兼容字段                       |
| `authRegion`   | string | 凭据级 Auth Region，用于 Token 刷新, 未配置时回退到 region |
| `apiRegion`    | string | 凭据级 API Region，用于 API 请求                    |
| `machineId`    | string | 凭据级机器码（64位十六进制）                             |
| `email`        | string | 用户邮箱（可选，从 API 获取）                           |
| `proxyUrl`     | string | 凭据级代理 URL（可选，特殊值 `direct` 表示不使用代理）       |
| `proxyUsername`| string | 凭据级代理用户名（可选）                                |
| `proxyPassword`| string | 凭据级代理密码（可选）                                 |
| `endpoint`     | string | 凭据级端点名称（可选，未配置时使用 `config.defaultEndpoint`）|

说明：
- IdC / Builder-ID / IAM 在本项目里属于同一种登录方式，配置时统一使用 `authMethod: "idc"`
- 为兼容旧配置，`builder-id` / `iam` 仍可被识别，但会按 `idc` 处理

#### 单凭据格式（旧格式，向后兼容）

```json
{
   "accessToken": "请求token，一般有效期一小时，可选",
   "refreshToken": "刷新token，一般有效期7-30天不等",
   "profileArn": "arn:aws:codewhisperer:us-east-1:111112222233:profile/QWER1QAZSDFGH",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social",
   "clientId": "IdC 登录需要",
   "clientSecret": "IdC 登录需要"
}
```

#### 多凭据格式（支持故障转移和自动回写）

```json
[
   {
      "refreshToken": "第一个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 0
   },
   {
      "refreshToken": "第二个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "idc",
      "clientId": "xxxxxxxxx",
      "clientSecret": "xxxxxxxxx",
      "region": "us-east-2",
      "priority": 1,
      "proxyUrl": "socks5://proxy.example.com:1080",
      "proxyUsername": "user",
      "proxyPassword": "pass"
   },
   {
      "refreshToken": "第三个凭据（显式不走代理）",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 2,
      "proxyUrl": "direct"
   }
]
```

多凭据特性：
- 按 `priority` 字段排序，数字越小优先级越高（默认为 0）
- 单凭据最多重试 3 次，单请求最多重试 9 次
- 自动故障转移到下一个可用凭据
- 多凭据格式下 Token 刷新后自动回写到源文件

### Region 配置

支持多级 Region 配置，分别控制 Token 刷新和 API 请求使用的区域。

**Auth Region**（Token 刷新）优先级：
`凭据.authRegion` > `凭据.region` > `config.authRegion` > `config.region`

**API Region**（API 请求）优先级：
`凭据.apiRegion` > `config.apiRegion` > `config.region`

### 代理配置

支持全局代理和凭据级代理，凭据级代理会覆盖该凭据产生的所有出站连接（API 请求、Token 刷新、额度查询）。

**代理优先级**：`凭据.proxyUrl` > `config.proxyUrl` > 无代理

| 凭据 `proxyUrl` 值 | 行为 |
|---|---|
| 具体 URL（如 `http://proxy:8080`、`socks5://proxy:1080`） | 使用凭据指定的代理 |
| `direct` | 显式不使用代理（即使全局配置了代理） |
| 未配置（留空） | 回退到全局代理配置 |

凭据级代理示例：

```json
[
   {
      "refreshToken": "凭据A：使用自己的代理",
      "authMethod": "social",
      "proxyUrl": "socks5://proxy-a.example.com:1080",
      "proxyUsername": "user_a",
      "proxyPassword": "pass_a"
   },
   {
      "refreshToken": "凭据B：显式不走代理（直连）",
      "authMethod": "social",
      "proxyUrl": "direct"
   },
   {
      "refreshToken": "凭据C：使用全局代理（或直连，取决于 config.json）",
      "authMethod": "social"
   }
]
```

### 认证方式

客户端请求本服务时，支持两种认证方式：

1. **x-api-key Header**
   ```
   x-api-key: sk-your-api-key
   ```

2. **Authorization Bearer**
   ```
   Authorization: Bearer sk-your-api-key
   ```

### 环境变量

可通过环境变量配置日志级别：

```bash
RUST_LOG=debug ./target/release/kiro-rs
```

集群 prompt cache 默认连接或自举在 `127.0.0.1:46379`。可用
`KIRO_CLUSTER_CACHE_ADDR=host1:46379,host2:46379` 配置共享候选地址，或设为
`off`、`local`、`disabled` 关闭集群共享并退回进程内缓存。

AWS-B 的公共 Anthropic 兼容响应统一使用 `msg_01…` Base62 消息 ID，不暴露
内部使用的 Bedrock 或其他传输方式。该规则只改变公开消息 ID，不改变模型路由、
对话内容、Thinking、Token 统计或工具调用。

## API 端点

### 标准端点 (/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/v1/models` | GET | 获取可用模型列表 |
| `/v1/messages` | POST | 创建消息（对话） |
| `/v1/messages/count_tokens` | POST | 估算 Token 数量 |
| `/v1/chat/completions` | POST | OpenAI Chat Completions 兼容端点，支持流式响应与函数工具调用 |

### Claude Code 兼容端点 (/cc/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/cc/v1/messages` | POST | 创建消息（缓冲模式，输入 usage 与 `/v1` 使用相同的本地确定性口径） |
| `/cc/v1/messages/count_tokens` | POST | 估算 Token 数量（与 `/v1` 相同） |

> **`/cc/v1/messages` 与 `/v1/messages` 的区别**：
> - `/v1/messages`：实时流式返回；`message_start` 与最终 `message_delta` 使用同一份请求侧本地输入 usage
> - `/cc/v1/messages`：缓冲模式；仅延迟发送事件，不会用上游 `metadataEvent`、`meteringEvent` 或 `contextUsageEvent` 改写输入 usage
> - 启用 `awsB40Compat` 且未配置原生 Bedrock Mantle 混合路由时，公共 `/v1/messages` 按 Amazon Bedrock 能力边界校验：拒绝 `fallbacks`、服务端 `web_search` / `advisor` / `code_execution`、`output_config.format`、现代 Claude 的手动 `thinking.type=enabled` 和 URL 图片；普通问答、代码生成、base64 图片及客户端自定义工具保持可用
> - `/cc/v1/messages` 保留上述 Kiro 兼容扩展，供明确需要扩展能力的 Claude Code 客户端使用；两个入口都会拒绝首条 `system` role，以及现代 Claude 的末尾 assistant prefill
> - 输入总量与缓存拆分在请求发往上游前完成；自动续写和本轮输出不会增加客户端请求的 `input_tokens`
> - 普通文本和缓存前缀统一使用本地 Claude BPE 加固定协议框架；不存在 1024 字符分界，也不会叠加字符数比例补偿
> - 等待期间会每 25 秒发送 `ping` 事件保活

### Thinking 模式

支持 Claude 的 extended thinking 功能：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 16000,
  "thinking": {
    "type": "enabled",
    "budget_tokens": 10000
  },
  "messages": [...]
}
```

### 工具调用

完整支持 Anthropic 的 tool use 功能：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 1024,
  "tools": [
    {
      "name": "get_weather",
      "description": "获取指定城市的天气",
      "input_schema": {
        "type": "object",
        "properties": {
          "city": {"type": "string"}
        },
        "required": ["city"]
      }
    }
  ],
  "messages": [...]
}
```

## 模型映射

| 客户端模型 | Kiro 模型 |
|----------------|-----------|
| `gpt-5.6` / `GPT 5.6` | `gpt-5.6-sol` |
| `gpt-5.6-sol` / `GPT 5.6 Sol` | `gpt-5.6-sol` |
| `gpt-5.6-terra` / `GPT 5.6 Terra` | `gpt-5.6-terra` |
| `gpt-5.6-luna` / `GPT 5.6 Luna` | `gpt-5.6-luna` |
| `deepseek-3.2` / `deepseek-v3.2` / `DeepSeek V3.2` | `deepseek-3.2` |
| `*sonnet*`（含 5/5.0） | `claude-sonnet-5` |
| `*sonnet*`（含 4.6/4-6） | `claude-sonnet-4.6` |
| `*sonnet*`（其他） | `claude-sonnet-4.5` |
| `*opus*`（含 5/5.0） | `claude-opus-5` |
| `*opus*`（含 4.5/4-5） | `claude-opus-4.5` |
| `*opus*`（含 4.7/4-7） | `claude-opus-4.7` |
| `*opus*`（含 4.8/4-8） | `claude-opus-4.8` |
| `*opus*`（其他） | `claude-opus-4.6` |
| `*haiku*` | `claude-haiku-4.5` |

GPT 5.6 仅接受上表中的精确名称或别名；裸 `gpt-5.6` 会规范化为 Sol，
其余三档原样透传到上游。它们不会回退到 Claude
或其他 GPT 模型，也不会使用本地兼容回复。`GET /v1/models` 会同时列出裸别名及
Sol/Terra/Luna 三档，便于客户端发现。身份探测使用独立的 GPT 策略：
公开助手为 ChatGPT，精确模型为请求中的 Sol/Terra/Luna，开发者与模型提供方为
OpenAI，私有宿主/运行时返回 `unknown`。该策略复用跨分片检测和结构化防泄漏框架，
但不注入 Claude/Anthropic 身份；普通第三方讨论、引用、代码、字符串字面量和业务
工具参数保持原样。

### GPT 5.6 推理强度

GPT 5.6 使用原生 `reasoning`，不要使用 Claude 的 `thinking` 或
`output_config.effort`。Sol、Terra、Luna 均支持六档：
`none`、`low`、`medium`、`high`、`xhigh`、`max`。GPT 5.6 官方默认值是
`medium`；`reasoning.mode` 另支持 `standard`（默认）和 `pro`。
仅传 `mode` 时，本服务在三个入口都使用 `medium`；省略整个 `reasoning`
时不向上游伪造配置。如需可审计的跨入口行为，请显式传入档位。

Anthropic Messages 兼容端点：

```json
{
  "model": "gpt-5.6-sol",
  "max_tokens": 1024,
  "reasoning": {
    "effort": "xhigh",
    "mode": "standard"
  },
  "messages": [
    {"role": "user", "content": "分析这个并发问题"}
  ]
}
```

OpenAI Chat Completions 兼容端点可使用顶层
`"reasoning_effort": "xhigh"`，并可选传入
`"reasoning_mode": "standard"`；也接受上面相同的嵌套 `reasoning` 对象。
不支持的档位会在本地返回 400，不会静默降级到默认值。
其 `parallel_tool_calls: false` 也会加入串行工具提示，但受下述相同的
best-effort 限制。

OpenAI Responses 兼容端点使用标准嵌套字段：

```json
{
  "model": "gpt-5.6-sol",
  "input": "分析这个并发问题",
  "reasoning": {
    "effort": "xhigh",
    "mode": "standard"
  }
}
```

该入口位于 `/v1/responses`，支持 GPT 5.6 的文本、图片、base64 文档、
函数工具、非流式响应和 canonical Responses SSE。
`store` 省略时按 Responses 契约默认为 `true`；服务会在单进程内以内存保存
最多 30 分钟的有限可见对话状态，供 `previous_response_id` 续接。状态池最多
256 项、总计 32 MiB、单项 4 MiB。省略 `store` 的请求若超过单项容量，会尽可能
降为 `store: false` 并明确反映在响应中；显式 `store: true` 超限会返回容量错误。
流式响应若直到输出结束后才越过容量，仍可能以 `response_store_error` 终止。
Codex 显式发送的 `store: false` 始终保持无状态，不受此限制。

`parallel_tool_calls: false` 会向上游加入“本轮最多调用一个工具”的串行约束。
当前上游没有原生的硬开关；若它违反约束，服务会记录 warning 并完整返回所有调用，
避免静默丢失工具请求。因此该字段在此兼容层中属于 best-effort，而非硬保证。

AWS-B/Kiro 当前不提供 OpenAI 的不透明加密推理项。为兼容 Codex，请求可以携带
`include: ["reasoning.encrypted_content"]` 及 `reasoning.context/summary`，但服务不会
伪造 `encrypted_content`，也无法在 `store: false` 的后续轮次回放隐藏推理状态；
可见消息和工具结果仍可正常多轮续接。`prompt_cache_key`、`client_metadata`、
`service_tier`、`stream_options` 与 `defer_loading` 同样属于请求形状兼容字段，
不代表上游缓存、服务等级或延迟加载已生效。

Codex 0.146 默认还会声明 `{"type":"web_search","external_web_access":false}`；该值
表示 cached hosted search，而不是完全禁用搜索。为使普通 Codex 编程请求可用，这个
默认声明会原样回显但不会下发给 Kiro，也不会伪造搜索结果；Codex 的可选
`search_content_types` 元数据只严格接受 `text`/`image`。启用外网或带筛选、位置、
上下文等实际搜索参数的 hosted Web Search 会明确报错，因为当前 GPT Responses 路径
没有可验证的搜索结果与引用事件。若不需要搜索，可在 Codex 配置中设
`web_search = "disabled"`；若任务必须搜索，请使用独立搜索工具或支持 hosted search
的上游。这与 Anthropic Messages 的内置 WebSearch 转换逻辑不是同一个协议。

Codex 建议直接配置官方目录中的 `gpt-5.6-sol`、`gpt-5.6-terra` 或
`gpt-5.6-luna`。裸 `gpt-5.6` 仍可调用并会映射到 Sol，但 Codex 客户端会因其内置
目录没有该别名而显示 fallback metadata 警告。带 `client_version` 的模型目录请求会
返回合法的 Codex 空覆盖目录，使客户端继续使用与自身版本匹配的内置模型元数据；
不带该参数的 `/v1/models` 仍返回标准 OpenAI 模型列表。

## Admin（可选）

当 `config.json` 配置了非空 `adminApiKey` 时，会启用：

- **Admin API（认证同 API Key）**
  - `GET /api/admin/credentials` - 获取所有凭据状态
  - `POST /api/admin/credentials` - 添加新凭据
  - `DELETE /api/admin/credentials/:id` - 删除凭据
  - `POST /api/admin/credentials/:id/disabled` - 设置凭据禁用状态
  - `POST /api/admin/credentials/:id/priority` - 设置凭据优先级
  - `POST /api/admin/credentials/:id/reset` - 重置失败计数
  - `GET /api/admin/credentials/:id/balance` - 获取凭据余额（默认服务端缓存 5 分钟）
  - `GET /api/admin/credentials/:id/balance?fresh=true` - 绕过本地缓存查询上游，适合调用前后用量差值实验；上游账本仍可能延迟更新

- **Admin UI**
  - `GET /admin` - 访问管理页面（需要在编译前构建 `admin-ui/dist`）

## 注意事项

1. **凭证安全**: 请妥善保管 `credentials.json` 文件，不要提交到版本控制
2. **Token 刷新**: 服务会自动刷新过期的 Token，无需手动干预
3. **Anthropic WebSearch 工具**: AWS-B 公共 `/v1/messages` 会按 Bedrock 能力边界拒绝服务端 WebSearch；需要 Kiro WebSearch 兼容扩展时使用 `/cc/v1/messages`。OpenAI Responses 的 hosted `web_search` 目前仅接受 `external_web_access: false` 的禁用态声明

## 项目结构

```
kiro-rs/
├── src/
│   ├── main.rs                 # 程序入口
│   ├── http_client.rs          # HTTP 客户端构建
│   ├── token.rs                # Token 计算模块
│   ├── debug.rs                # 调试工具
│   ├── test.rs                 # 测试
│   ├── model/                  # 配置和参数模型
│   │   ├── config.rs           # 应用配置
│   │   └── arg.rs              # 命令行参数
│   ├── anthropic/              # Anthropic API 兼容层
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── middleware.rs       # 认证中间件
│   │   ├── types.rs            # 类型定义
│   │   ├── converter.rs        # 协议转换器
│   │   ├── stream.rs           # 流式响应处理
│   │   └── websearch.rs        # WebSearch 工具处理
│   ├── kiro/                   # Kiro API 客户端
│   │   ├── provider.rs         # API 提供者
│   │   ├── token_manager.rs    # Token 管理
│   │   ├── machine_id.rs       # 设备指纹生成
│   │   ├── model/              # 数据模型
│   │   │   ├── credentials.rs  # OAuth 凭证
│   │   │   ├── events/         # 响应事件类型
│   │   │   ├── requests/       # 请求类型
│   │   │   ├── common/         # 共享类型
│   │   │   ├── token_refresh.rs # Token 刷新模型
│   │   │   └── usage_limits.rs # 使用额度模型
│   │   └── parser/             # AWS Event Stream 解析器
│   │       ├── decoder.rs      # 流式解码器
│   │       ├── frame.rs        # 帧解析
│   │       ├── header.rs       # 头部解析
│   │       ├── error.rs        # 错误类型
│   │       └── crc.rs          # CRC 校验
│   ├── admin/                  # Admin API 模块
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── service.rs          # 业务逻辑服务
│   │   ├── types.rs            # 类型定义
│   │   ├── middleware.rs       # 认证中间件
│   │   └── error.rs            # 错误处理
│   ├── admin_ui/               # Admin UI 静态文件嵌入
│   │   └── router.rs           # 静态文件路由
│   └── common/                 # 公共模块
│       └── auth.rs             # 认证工具函数
├── admin-ui/                   # Admin UI 前端工程（构建产物会嵌入二进制）
├── tools/                      # 辅助工具
├── Cargo.toml                  # 项目配置
├── config.example.json         # 配置示例
├── docker-compose.yml          # Docker Compose 配置
└── Dockerfile                  # Docker 构建文件
```

## 技术栈

- **Web 框架**: [Axum](https://github.com/tokio-rs/axum) 0.8
- **异步运行时**: [Tokio](https://tokio.rs/)
- **HTTP 客户端**: [Reqwest](https://github.com/seanmonstar/reqwest)
- **序列化**: [Serde](https://serde.rs/)
- **日志**: [tracing](https://github.com/tokio-rs/tracing)
- **命令行**: [Clap](https://github.com/clap-rs/clap)

## License

MIT

## 致谢

本项目的实现离不开前辈的努力:  
 - [kiro2api](https://github.com/caidaoli/kiro2api)
 - [proxycast](https://github.com/aiclientproxy/proxycast)

本项目部分逻辑参考了以上的项目, 再次由衷的感谢!
