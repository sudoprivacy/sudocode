# 日志系统优化方案

## 问题分析

### 1. 重复记录问题 (严重)

当前日志存在大量重复记录:

| 事件类型 | 重复情况 | 根因 |
|---------|---------|------|
| `session_started` | 每次会话 2 条 | `record_session_started` 调用了 `sink.record()` 和 `self.record()` |
| `request_started` + `http_request_started` | 每次请求 2 条 | 同上 |
| `request_succeeded` + `http_request_succeeded` | 每次成功 2 条 | 同上 |
| `request_failed` + `http_request_failed` | 每次失败 2 条 | 同上 |

**代码位置**: `telemetry/src/lib.rs` 中 `SessionTracer` 的方法

```rust
// 当前实现 (有重复)
pub fn record_http_request_started(...) {
    self.sink.record(TelemetryEvent::HttpRequestStarted {...});  // 第1条
    self.record("http_request_started", ...);                     // 第2条 (SessionTrace)
}
```

### 2. 缺少 request_id 用于对账

当前 `request_debug` 没有唯一的 request_id，无法与大模型提供商对账。

### 3. 日志级别单一

所有日志都是 `info` 级别，无法过滤:
- `request_debug` → 应该是 `debug` 级别
- `request_failed` → 应该是 `warn` 或 `error` 级别

### 4. 缺少关键指标

- 没有请求耗时 (duration_ms)
- 没有请求/响应大小

---

## 优化方案

### 方案 A: 消除重复记录 (推荐)

**修改 `SessionTracer` 方法，只保留一条记录:**

```rust
// 优化后: 只记录一次
pub fn record_http_request_started(...) {
    // 只调用 sink.record，不再调用 self.record
    self.sink.record(TelemetryEvent::HttpRequestStarted {
        session_id: self.session_id.clone(),
        attempt,
        method,
        path,
        attributes,
    });
}
```

**影响**:
- 日志量减少 ~50%
- 不再有 `http_request_*` 和 `request_*` 两种事件名
- 统一使用 `request_started`, `request_succeeded`, `request_failed`

### 方案 B: 添加 request_id 用于对账

**为每次请求生成唯一 request_id:**

```rust
use uuid::Uuid;

pub fn record_http_request_debug(...) {
    let request_id = Uuid::new_v4().to_string();  // 生成唯一 ID
    self.sink.record(TelemetryEvent::HttpRequestDebug {
        session_id: self.session_id.clone(),
        request_id,  // 新增: 用于对账
        timestamp_ms: current_timestamp_ms(),
        url,
        method,
        headers,
        body,
    });
}
```

**request_id 格式**: `req_` + UUID v4，例如 `req_550e8400-e29b-41d4-a716-446655440000`

**用途**:
1. 用户排查问题时可通过 request_id 定位具体请求
2. 与大模型提供商对账时提供唯一标识
3. 关联同一请求的 started/debug/succeeded/failed 事件

### 方案 C: 添加请求耗时

**在 `HttpRequestStarted` 和 `HttpRequestSucceeded` 中添加时间戳:**

```rust
HttpRequestStarted {
    session_id: String,
    request_id: String,  // 新增: 关联请求
    attempt: u32,
    method: String,
    path: String,
    timestamp_ms: u64,   // 新增: 请求开始时间
    attributes: Map<String, Value>,
}

HttpRequestSucceeded {
    session_id: String,
    request_id: String,  // 新增: 关联请求
    attempt: u32,
    method: String,
    path: String,
    status: u16,
    duration_ms: u64,    // 新增: 请求耗时
    provider_request_id: Option<String>,  // 重命名: 提供商返回的 request-id
    attributes: Map<String, Value>,
}
```

### 方案 D: 添加日志级别

**根据事件类型设置日志级别:**

```rust
fn get_log_level(event: &TelemetryEvent) -> &'static str {
    match event {
        TelemetryEvent::HttpRequestDebug { .. } => "debug",
        TelemetryEvent::HttpRequestFailed { .. } => "warn",
        TelemetryEvent::SessionEnded { .. } => "info",
        _ => "info",
    }
}
```

---

## 推荐实施顺序

1. **方案 A** - 消除重复记录 (最大收益，减少 ~50% 日志量)
2. **方案 B** - 添加 request_id (对账需求)
3. **方案 C** - 添加耗时指标 (增强可观测性)
4. **方案 D** - 添加日志级别过滤 (灵活性)

---

## 实施后的日志格式

### request_started (请求开始)
```json
{
  "timestamp": "2026-05-20T10:00:00.000+08:00",
  "level": "info",
  "session_id": "session-123",
  "component": "scode",
  "event": "request_started",
  "attributes": {
    "request_id": "req_550e8400-e29b-41d4-a716-446655440000",
    "attempt": 1,
    "method": "POST",
    "path": "/v1/chat/completions",
    "timestamp_ms": 1716163200000
  }
}
```

### request_debug (请求详情 - debug 级别)
```json
{
  "timestamp": "2026-05-20T10:00:00.100+08:00",
  "level": "debug",
  "session_id": "session-123",
  "component": "scode",
  "event": "request_debug",
  "attributes": {
    "request_id": "req_550e8400-e29b-41d4-a716-446655440000",
    "url": "https://api.anthropic.com/v1/messages",
    "method": "POST",
    "headers": {"x-api-key": "sk-ant-... (hidden)"},
    "body": {"model": "claude-sonnet-4-6", "messages": [...]}
  }
}
```

### request_succeeded (请求成功)
```json
{
  "timestamp": "2026-05-20T10:00:01.500+08:00",
  "level": "info",
  "session_id": "session-123",
  "component": "scode",
  "event": "request_succeeded",
  "attributes": {
    "request_id": "req_550e8400-e29b-41d4-a716-446655440000",
    "attempt": 1,
    "method": "POST",
    "path": "/v1/chat/completions",
    "status": 200,
    "duration_ms": 1500,
    "provider_request_id": "req_abc123_from_provider"
  }
}
```

### request_failed (请求失败 - warn 级别)
```json
{
  "timestamp": "2026-05-20T10:00:02.000+08:00",
  "level": "warn",
  "session_id": "session-123",
  "component": "scode",
  "event": "request_failed",
  "attributes": {
    "request_id": "req_550e8400-e29b-41d4-a716-446655440000",
    "attempt": 3,
    "method": "POST",
    "path": "/v1/chat/completions",
    "error": "rate limit exceeded",
    "retryable": true
  }
}
```

---

## request_id 设计说明

### 生成规则
- 格式: `req_` + UUID v4
- 示例: `req_550e8400-e29b-41d4-a716-446655440000`
- 在请求开始时生成，贯穿整个请求生命周期

### 与 provider_request_id 的区别

| 字段 | 来源 | 用途 |
|-----|------|------|
| `request_id` | 客户端生成 | 内部追踪、用户排查问题 |
| `provider_request_id` | 提供商返回 (响应头中的 request-id) | 与提供商对账 |

### 关联查询

用户可以通过 request_id 关联一次请求的所有日志:

```bash
# 查询某次请求的所有日志
cat scode.log | jq 'select(.attributes.request_id == "req_550e8400-e29b-41d4-a716-446655440000")'
```
