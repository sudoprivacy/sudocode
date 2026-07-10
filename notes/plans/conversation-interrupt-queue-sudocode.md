# sudocode CLI · REPL 打断 + 消息队列（cross-product 对齐）

> 上游行为规范：[`sudowork/docs/plans/2026-07-01-conversation-interrupt-and-queue.html`](https://s.shareone.vip/s/sudowork-interrupt-queue) §3.2 矩阵。
> 目的：让 sudocode 交互式 REPL 用户在两个产品间切换体验一致 —— 一轮 running 时可以先输入下一条（甚至连输 N 条），轮结束时按批量合并语义 flush。

## 现状

`rusty-sudocode-cli::main`（`main.rs:1441-1551` 附近）的 REPL 是**同步单线程**：
```rust
loop {
    let outcome = editor.read_line()?;  // rustyline::readline —— 阻塞
    match outcome {
        Submit(line) => {
            // ... slash-command 或 skills 分派 ...
            cli.run_turn(&trimmed)?;    // block_on(runtime.run_turn(...)) —— 阻塞
        }
        Exit => break,
    }
}
```

- `editor.read_line` 阻塞在 stdin，直到用户回车 → 整循环停在这一步
- `cli.run_turn` 通过 `tokio_runtime.block_on` 阻塞到 turn 完成
- 结果：**turn 进行中用户根本敲不进字**（terminal 会照常回显击键，但 rustyline 没在读，回车不会被 accept，Ctrl-C 走的是信号 → `HookAbortSignal.abort()`，那条链路已经工作，见 memory `sudocode as unit in collaboration`）

## 目标行为（对齐 sudowork §3.2 矩阵）

| | 队列 OFF | 队列 ON |
|---|---|---|
| **auto-interrupt OFF** | 当前行为：轮中不接受输入 | 轮中收到的输入进队；轮结束（自然完成 / Ctrl-C）时**合并为 1 条 user message 一次发出** |
| **auto-interrupt ON** | 回车即 Ctrl-C + 新 turn | 回车 = 打断 + 新 solo turn；启动后再来的按左格队列 |

键位：
- Enter（轮中）→ 提交（走矩阵）
- ↑（编辑框空 & 队列非空）→ dequeue 最后一条回填编辑框
- Ctrl-C：手动打断当前 turn（**保持不变**，走 `HookAbortSignal`）

## 关键约束（sudocode 语义硬约束）

- **禁止 alternate-screen TUI / ratatui**（memory `no_alternate_screen_tui_sudocode`）—— 只能 inline ANSI；attach/scrollback/pipe 工作流必须继续能用
- **禁止 unit test**（memory `no_unit_tests_sudocode`）—— 走 PTY 集成门（`tests/pty_*.rs`）
- 不能引入外部 UI 库；rustyline 继续用

## 设计（单进程、双 mpsc、后台输入线程）

主循环拆成 3 个协作角色：

```
┌────────────────┐        ┌─────────────────────┐        ┌───────────────────┐
│ input-thread   │ line   │ coordinator (main)  │  cmd   │ tokio-runtime     │
│ rustyline blk  │──────▶ │ TurnState + queue   │──────▶ │ runtime.run_turn  │
│ 后台线程       │        │ mpsc dispatch       │  done  │ block_on          │
└────────────────┘        └──────┬──────────────┘◀────── └───────────────────┘
                                 │  render tips / prompt
                                 ▼
                              stdout（inline ANSI）
```

- **input-thread**：新建 `std::thread`，跑 rustyline `editor.readline`，把每个 `ReadOutcome::Submit(line)` 通过 `mpsc::Sender<InputEvent>` 送到主线程。↑ 键在 rustyline 里绑定为「emit `InputEvent::DequeueRequest`，返回 `Cmd::Noop` 保持当前 buffer 不变」（用 `ConditionalEventHandler`）。这条线程**不知道**是否有 turn 在跑；语义决策全部在主线程。
- **主线程（coordinator）**：
  - 持一个 `queue: VecDeque<QueuedInput>`（每条 = 用户输入的原文 + slash 前处理后的 prompt 文本）
  - 持一个 `turn_state: enum { Idle, Running { abort: HookAbortSignal } }`
  - 用 `select!` 或 `crossbeam::select!` 在两个 channel 上等：`input_rx` 与 `turn_done_rx`
  - 收到 `InputEvent::Submit(line)`：查设置 `SUDOCODE_INTERRUPT_QUEUE_MODE`（env 或 config）→ 决策：立发 / 入队 / 打断 + 入队 / 阻塞提示
  - 收到 `TurnDone`：如果 queue 非空，**合并**队列所有条目为 1 个 combined prompt，通过 mpsc 送到 tokio-runtime 线程跑 `runtime.run_turn(combined)`
  - 空闲 + queue 有 solo 头（interrupter）→ 单独跑 solo，跑完再看剩下的按合并逻辑
- **tokio-runtime 线程**：一个持续存在的线程，收到 `RunTurn(prompt)` 就 `tokio_runtime.block_on(runtime.run_turn(prompt, ...))`，跑完发 `TurnDone`（含结果 / 错误）。挂在这里的 `HookAbortSignal` 通过之前 `set_abort_signal` 挂钩已经工作。

### 合并语义

同 sudowork：`queue` 里 N 条 `QueuedInput` 时，合并成 1 条 `combined = items.join("\n\n")` 送出。solo 头独占，不合。auto-interrupt 触发时插到队头并打 `solo` 标；跟随非 solo 条目仍会在下一轮 batch 中合并。

### 键位实现细节

- `↑`（rustyline 的 `KeyCode::Up`）：默认是历史向上。要用条件绑定：
  ```rust
  editor.bind_sequence(
      KeyEvent(KeyCode::Up, Modifiers::NONE),
      EventHandler::Conditional(Box::new(DequeueIfEmpty { dequeue_tx: ... })),
  );
  ```
  `DequeueIfEmpty::handle` 只有当 `ctx.line().is_empty()` 时才发 `InputEvent::DequeueRequest`（送到 main → main 回响一个 `Cmd::Insert(dequeued_text)`? 但 rustyline 是同步的，无法从主线程灌 buffer）
  → 更实用的方案：**空 buffer + Up** 直接通过 rustyline 的 `Cmd::Insert(str)` 就地插入（同步：`DequeueIfEmpty` handler 自己去查 queue，不走 channel。queue 用 `Arc<Mutex<VecDeque>>` 共享给 input-thread 和主线程）
  非空 buffer + Up → 默认历史，不干预

### 配置

- `SUDOCODE_INTERRUPT_QUEUE_MODE=off|queue|interrupt|both`（默认 `queue`，与 sudowork 一致的 opt-in 灰度）
- 或复用 sudocode config 文件里的一个新键 `interrupt_queue_mode`

## 测试

**PTY 集成**（`rusty-sudocode-cli/tests/pty_interrupt_queue.rs`）：
- 启动 REPL，喂第一条长 prompt（bash sleep 30 + 一句 echo）
- 通过 PTY 写入 N 条 follow-up + Enter
- 断言：
  - 中间 N-1 条被入队（观察 stdout 上打印的 `[queued: ...]` 状态行）
  - Turn 结束后只有 1 个 combined API turn（通过 telemetry log —— `scode.log` 中 `request_debug` 事件数）
  - `↑`（空 buffer）dequeue 后 rustyline buffer 显示出该条内容
- Windows 上按 memory `sudowork GUI e2e recipe` 的 PTY 配方跑；CI 走 Linux/mac runner（memory `sudocode Rust build on Windows`：PTY 测试在 Windows 上跑不了实际 exec）

**禁写 unit test**（memory `no_unit_tests_sudocode`）——纯逻辑合并函数如果诱人也不写；直接 PTY 观察。

## 落地节奏

1. **plan-doc（本文）+ 起 branch**：`feat/repl-interrupt-queue`
2. **第 1 commit**：把 REPL 主循环拆成「input-thread + main coordinator + turn worker」骨架，行为**保持不变**（不接受轮中输入），跑一遍 PTY 冒烟确认没退化
3. **第 2 commit**：加 queue + turn_state + 合并逻辑；启用 `SUDOCODE_INTERRUPT_QUEUE_MODE=queue`；PTY 断言合并语义
4. **第 3 commit**：加 `↑` 空 buffer dequeue；PTY 断言
5. **第 4 commit**：接 auto-interrupt 分支；PTY 断言 solo 头 + 后续合并
6. **第 5 commit**：ROADMAP 更新 + `sudocode/sudo-code-roadmap.html` 状态同步 → 上传 shareone.app（memory `sudocode ROADMAP shareone share`）

## 已知风险 / 未决

- rustyline 的 `bind_sequence` + `ConditionalEventHandler` 在 buffer 更新流程里能不能干净地插入 dequeued text —— 需要读 rustyline 内部；如果不行退化为「dequeue 只清空当前 buffer + 提示，用户重新粘贴」
- 后台 input-thread + 主线程的 stdout 竞争 —— 都写 stdout 会撕字。方案：主线程把状态行写在 rustyline 提示上方的**保留行**（sudocode 已有这个机制，见 `input.rs:97 write!("\x1b7\x1b[2A\x1b[2K")`），input-thread 只让 rustyline 自己画
- 一轮长 turn（>5 min）中，用户断续输入多条又想 dequeue 又 auto-interrupt —— 状态机要覆盖这些组合，用 memory `feedback_no_fix_without_root_cause`：先复现再改
