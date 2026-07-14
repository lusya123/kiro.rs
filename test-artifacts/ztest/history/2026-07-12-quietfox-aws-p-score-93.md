# 真测 Ztest 检测报告

> **报告 ID:** `01KX98ARYFN9TPNCX17GC98GRC` \
> **状态:** 已完成 \
> **概要:** https://qui***sbs/ · claude-opus-4.8 · 并发模式 parallel \
> **销毁:** 20:54 后销毁 \
> **来源:** https://ztest.ai/report/01KX98ARYFN9TPNCX17GC98GRC \
> **保存时间:** Sun Jul 12 2026 03:00:47 GMT+0800 (中国标准时间)

## 综合评分

- **分数:** 93 / 100
- **结论:** 推荐
- **说明:** 真测·Ztest.ai · 基于多维度探针加权检测

## 反向通道嫌疑度: 0 / 100

> 扫描所有探针的 raw 响应 + 元数据, 提取反向通道 / IDE 包装 / 协议伪造的特征信号。 嫌疑度越低越好 (强证据 +30 / 辅助证据 +15 / 正向信号 −8)。

### 辅助证据 (1) · 嫌疑度 +15 / 项

- **PERFORMANCE_DROP** · 响应明显慢于基线（探针 D8）
  - D8 ratio=2.13x baseline (>=1.5x), 整体延迟偏高

### 正向信号 (4) · 嫌疑度 −8 / 项

- **Author 含真 AI 自身身份** — 代码注释 author 字段填写 Anthropic / Claude 自身身份, 符合真 Claude 在被要求填 author 时的典型行为 [D11]
- **严格服从 user system** — S3 全部 instruction_override + identity_lock 完美服从, user system 优先级正常, 没有渠道层 system 压制 [S3]
- **Canary 完美回响** — 复述 nonce 任务完美执行, 无 prompt 改写迹象 [D5]
- **协议字段完整规范** — Anthropic / OpenAI 标准字段全覆盖, 格式无错, 协议层完全规范 [D2]

## 分类得分

| 分类 | 得分 | 探针 |
|---|---|---|
| 协议合规 | 100% | ✓HB ✓D1 ✓D2 ✓D18 |
| 身份一致 | 96% | ✓D3 ✓D11 ✓D17 |
| 能力验证 | 100% | ✓D7 ✓D10 ✓D13 ✓D16 ✓D19 |
| 内容完整性 | 100% | ✓D5 |
| 安全性 | 95% | ✓S1 ✓S2 ✓S3 ✓S4 |
| 性能 | 80% | ?D8 ✓D9 ✓S5 |

## 探针明细

| 探针 | 名称 | 判定 | 得分 | 延迟 |
|---|---|---|---|---|
| D1 | 协议连通性 | 通过 | 100 | 4.52 s |
| HB | 接口心跳 | 通过 | 100 | 2.47 s |
| D10 | 思维链 | 通过 | 100 | 4.23 s |
| D11 | 隐式身份 | 通过 | 100 | — |
| D13 | 多模态 | 通过 | 100 | — |
| D16 | 能力指纹 | 通过 | 100 | — |
| D17 | 响应签名 | 通过 | 100 | 4.52 s |
| D18 | 缓存字段完备性 | 通过 | 100 | 1.31 s |
| D19 | 文档识别 | 通过 | 100 | 1.94 s |
| D2 | 响应结构 | 通过 | 100 | 4.52 s |
| D5 | 内容 Canary | 通过 | 100 | 1.23 s |
| D7 | 结构化输出 | 通过 | 100 | 3.13 s |
| D8 | 响应时延 | 部分通过 | 55 | 4.70 s |
| D9 | 性能稳定性 | 通过 | 85 | 5.55 s |
| S1 | Token 注入 | 通过 | 80 | — |
| S2 | 提示词提取 | 通过 | 100 | — |
| S3 | 指令覆盖 | 通过 | 100 | — |
| S4 | 错误信息泄露 | 通过 | 100 | — |
| S5 | 流完整性 | 通过 | 100 | 2.42 s |
| D3 | 身份一致性 | 通过 | 88 | 2.02 s |

### 探针诊断详情（异常项 / 可展开项）

#### D13 · 多模态 — 通过（得分 100，延迟 —）

**solid_color**
正确
期望答案
```
red
```

模型回答
```
Red
```

**chessboard**
正确
期望答案
```
4
```

模型回答
```
4
```

**ocr_digits**
正确
期望答案
```
40697
```

模型回答
```
40697
```

**spatial**
正确
期望答案
```
green
```

模型回答
```
Green
```

**text_conflict**
正确
期望答案
```
white
```

模型回答
```
White
```

**原始 JSON:**
```json
{
  "levels": [
    {
      "level": "solid_color",
      "expected": "red",
      "raw_response": "Red",
      "correct": true,
      "skipped": false,
      "http_status": 200,
      "note": ""
    },
    {
      "level": "chessboard",
      "expected": "4",
      "raw_response": "4",
      "correct": true,
      "skipped": false,
      "http_status": 200,
      "note": ""
    },
    {
      "level": "ocr_digits",
      "expected": "40697",
      "raw_response": "40697",
      "correct": true,
      "skipped": false,
      "http_status": 200,
      "note": ""
    },
    {
      "level": "spatial",
      "expected": "green",
      "raw_response": "Green",
      "correct": true,
      "skipped": false,
      "http_status": 200,
      "note": ""
    },
    {
      "level": "text_conflict",
      "expected": "white",
      "raw_response": "White",
      "correct": true,
      "skipped": false,
      "http_status": 200,
      "note": ""
    }
  ],
  "correct_count": 5,
  "total_count": 5,
  "skip_reason": null
}
```

#### D17 · 响应签名 — 通过（得分 100，延迟 4.52 s）

**原始 JSON:**
```json
{
  "channel_type": "anthropic",
  "http_status": 200,
  "requested_model": "claude-opus-4-8",
  "elapsed_ms": 4517,
  "checks": [
    {
      "field": "id",
      "weight": 25,
      "passed": true,
      "observed": "msg_01YAMQ2ElsYGTjwOptFNb5RT",
      "expected": "^msg_[A-Za-z0-9]{18,40}$ (Anthropic 官方 id 不含 '-')"
    },
    {
      "field": "type",
      "weight": 15,
      "passed": true,
      "observed": "message",
      "expected": "\"message\""
    },
    {
      "field": "role",
      "weight": 10,
      "passed": true,
      "observed": "assistant",
      "expected": "\"assistant\""
    },
    {
      "field": "model",
      "weight": 25,
      "passed": true,
      "observed": "claude-opus-4-8",
      "expected": "= claude-opus-4-8 (或带 -YYYYMMDD/版本后缀)"
    },
    {
      "field": "stop_reason",
      "weight": 15,
      "passed": true,
      "observed": "end_turn",
      "expected": "∈ {end_turn, max_tokens, stop_sequence, tool_use}"
    },
    {
      "field": "usage.input/output_tokens",
      "weight": 10,
      "passed": true,
      "observed": "input_tokens=26, output_tokens=4",
      "expected": "均为非负 int"
    }
  ],
  "fail_fields": []
}
```

#### D2 · 响应结构 — 通过（得分 100，延迟 4.52 s）

**原始 JSON:**
```json
{
  "channel_type": "anthropic",
  "matched_fields": [
    "id",
    "type",
    "role",
    "content[0].type",
    "content[0].text",
    "model",
    "stop_reason",
    "usage.input_tokens",
    "usage.output_tokens"
  ],
  "missing_fields": [],
  "type_errors": [],
  "coverage_percent": 100,
  "sample_content_text": "pong"
}
```

#### S1 · Token 注入 — 通过（得分 80，延迟 —）

**原始 JSON:**
```json
{
  "samples": [
    {
      "label": "short",
      "estimated_tokens": 72,
      "reported_tokens": 87,
      "overhead": 15,
      "word_count": 30,
      "error": null
    },
    {
      "label": "long",
      "estimated_tokens": 182,
      "reported_tokens": 199,
      "overhead": 17,
      "word_count": 100,
      "error": null
    }
  ],
  "suspicious_count": 0,
  "compat_mode": "claude_code",
  "note": "slope=1.6, overhead=17, slope on edge of normal BPE (weak evidence); overhead clean",
  "slope": 1.6,
  "max_overhead": 17
}
```

#### D3 · 身份一致性 — 通过（得分 88，延迟 2.02 s）

**显式检查（100分）**
响应 body 中 model 字段
```
claude-opus-4-8
```

判定: exact_match
**隐式检查（80分）**
询问模型自报身份 (JSON 回复)
```
I'm Claude — I was made by Anthropic.
```

声明厂商:
anthropic
声明模型:
判定: vendor_match_family_keyword
S3 交叉信号: 指令遵循率 100%
(身份已被锁定)
**原始 JSON:**
```json
{
  "requested_model": "claude-opus-4-8",
  "requested_family": "claude-opus",
  "explicit_check": {
    "response_model_field": "claude-opus-4-8",
    "score": 100,
    "reason": "exact_match"
  },
  "implicit_check": {
    "raw_response": "I'm Claude — I was made by Anthropic.",
    "parsed": {
      "vendor": "anthropic",
      "model_name": null,
      "model_family": "claude",
      "version": null
    },
    "declared_family": null,
    "score": 80,
    "reason": "vendor_match_family_keyword"
  },
  "probes": [],
  "family_votes": {},
  "suspect_models": [],
  "confidence": 0,
  "final_label": "match",
  "cross_s3_obey_rate": 1,
  "cross_s3_identity_locked": true
}
```

## 报告元数据

| 字段 | 值 |
|---|---|
| 引擎版本 | v1.0.0 |
| 执行节点 | hk3 |
| 报告 ID | 01KX98ARYFN9TPNCX17GC98GRC |
| 检测目标 | https://qui***sbs/ |
| 声明模型 | claude-opus-4-8 |
| 并发模式 | parallel |
| 启动时间 | 2026/7/12 02:51:41 |
| 完成时间 | 2026/7/12 02:52:39 |
| 自动销毁时间 | 2026/7/12 03:21:41 |
