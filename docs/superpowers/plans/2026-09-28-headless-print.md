# Sudo Code `-p` 非交互 Agent 模式实施计划

日期：2026-09-28

## 结论与代码基线

可行性高，属于中等规模接口工程。现有单次调用已经复用完整 Agent、工具、技能、MCP 和会话能力；主要工作是建立可靠的非交互输入输出契约，并补齐事件边界、权限与退出行为。

本计划基于从官方仓库独立拉取的 main：`58effa4bcfa0b1aed61ab1b7fc0532ab559dcd1f`（2026-09-28）。不是本机旧工作副本，也不等同于已安装的 v0.2.19。已有本地副本包含未提交修改，本次未改动这些副本。仅静态阅读代码和测试，未编译、未运行模型任务。

源码基址：https://github.com/sudoprivacy/sudocode/tree/58effa4bcfa0b1aed61ab1b7fc0532ab559dcd1f

## 已确认的实现状况

以下路径均相对于源码根目录。

| 位置 | 已确认事实 | 对实现的意义 |
|---|---|---|
| `rust/crates/rusty-sudocode-cli/src/cli/args.rs:102` | 声明了 `print: bool`，没有 `-p` 短参数；转换逻辑没有读取该字段 | 不能只补别名，必须让该标志影响执行模式 |
| 同文件 `:544`、`:734` | 裸提示词与 prompt 子命令都生成 Prompt；无参数时直接读取 stdin，空输入落入 REPL | `-p` 无任务必须明确报错，不能进入 REPL；stdin 读取应移出参数解析 |
| `rust/crates/rusty-sudocode-cli/src/main.rs:913` | Prompt 创建 LiveCli 并执行完整 turn | 复用现有执行链，无需新建 Agent 循环 |
| 同文件 `:927` | 提示词附加管道输入仅在 danger-full-access 下读取 | 应使输入读取与权限策略解耦 |
| 同文件 `:4449` | 默认 Text 走 run_turn_interactive；compact/JSON 走静默收集 | 新模式需要独立输出层，避免工具卡片和状态行进入 stdout |
| 同文件 `:4538` | JSON 模式始终使用交互式权限询问器 | 脚本可能等待输入，且提示污染 JSON |
| `rust/crates/rusty-sudocode-cli/tests/mock_parity_harness.rs:737` | 测试明确锁定旧 JSON 模式的权限提示 | 旧 prompt 行为应单独保留，不能批量删除旧断言 |
| `rust/crates/engine-events/src/lib.rs:142` | 已有 TextDelta、ToolCall、ToolResult、Usage、TurnComplete、Error 等事件 | 可新增事件输出适配器复用引擎 |
| `rust/crates/engine-core/src/session.rs:558` | on_message_stop 当前丢弃消息结束边界 | 若输出 assistant 消息，需要补传边界；不能把整个 turn 当一个消息 |
| `rust/crates/engine-host/src/runtime_build.rs:636` | REPL、print、ACP 都注入技能和 agent 列表 | 非交互模式无需重新实现技能发现 |
| `rust/crates/commands/src/lib.rs:3428` | 扫描 `.nexus/sudocode/skills`、`.agents/skills` 等；该列表不含 `.apeiron/commands` | skill creator 仍需改造测试技能的注入路径 |
| `rust/crates/rusty-sudocode-cli/src/main.rs:2316` | 已有 SIGINT / Windows Ctrl-Break → Cancel | 复用取消通道，补 SIGTERM、结果状态与退出码 |

## 目标与首版接口

首版目标：让外部脚本启动一次真实 Agent 任务，使用现有项目上下文与工具策略，多步执行后返回可解析结果并退出。

```bash
scode -p "分析当前项目"
scode --print "分析当前项目"
cat task.txt | scode -p
cat input.csv | scode -p "汇总供应商报价"
scode -p "检查项目" --output-format json
scode -p "执行任务" --output-format stream-json --verbose
scode -p "执行任务" --output-format stream-json --verbose --include-partial-messages
scode -p "继续检查" --resume <session-id>
```

首版兼容常用调用语义与已声明的事件子集，不宣称全面兼容 Claude Agent SDK。双向 stream-json 输入、SDK control_request 协议、完整子智能体事件协议作为后续范围。

## 阶段一：建立独立的非交互执行路径

1. 在 args.rs 为 print 增加 `short = 'p'`，引入明确的执行模式；建议单独建立 `HeadlessOptions` 与 Headless action，避免继续膨胀所有旧 Prompt 分支。
2. `-p` 与 `--print` 进入同一路径，不论 stdin/stdout 是否为 TTY，都不能启动 REPL、raw mode 或终端问答。裸提示词、prompt 子命令先保留原契约。
3. 固定并测试参数顺序：`scode -p "任务" --model ...` 必须真正解析后置参数，不能被现有 trailing_var_arg 当成提示词。需要以 `--` 明确转义的提示词在帮助中说明。
4. 将 stdin 合并集中到启动阶段，一次读取；有提示词时在明确分隔符后追加 stdin，无提示词时使用 stdin。空输入、非法 UTF-8、读取失败给出明确错误。
5. 保留已有首字节等待保护，覆盖“没有位置参数且管道不关闭”分支。明确有限输入的 EOF 契约、大小上限及超时错误；超时不能悄悄忽略输入后执行不同任务。
6. 将输出实现放入新模块，例如 `cli/headless.rs`、`cli/headless_output.rs`。直接复用 SessionEngine / EngineCommand / EngineEvent，不复制 runtime 工具循环。
7. `--resume` 必须保留 headless 模式、模型、工具策略等选项；禁止当前优先分支把 `-p --resume` 路由回交互逻辑。

完成标准：`-p`、stdin 和 resume 均完成真实工具调用链；全程不读取权限答复、不渲染 UI、不回落到 REPL。

## 阶段二：输出协议与消息边界

### text

stdout 只输出最终助手文本。工具输出保留在会话中；诊断写 stderr，默认关闭 WaitNotice、spinner、状态栏和工具卡片。不要把模型中间解释误作最终结果。

### json

输出单个最终对象，建议字段：`type: result`、`subtype`、`is_error`、`result`、`session_id`、`duration_ms`、`num_turns`、`usage`、`permission_denials`。schema 版本采用扩展字段标识。成本仅在价格与用量可靠时给出估算；未知数据保留缺失，不填零假装测得。

将 model round-trips 与用户 turn 明确定义，不能直接把现有 iterations 不加解释地当作 num_turns。工具参数在新协议中使用 JSON 对象，保留旧 JSON 的字符串字段契约。

### stream-json

按行输出并及时 flush：初始化信息、完整 assistant 消息、工具结果、最终 result。工具调用 id、消息 id、会话 id 稳定可关联。一个运行正常收尾时只产生一个最终 result；错误路径不可再由 main 重复追加另一个不一致的 error 对象。

`--include-partial-messages` 额外提供 stream_event；`--verbose` 控制诊断/附加事件，不得产生非 JSON 的 stdout 文本。为无关子命令拒绝 stream-json，避免修改所有 CliOutputFormat 消费者。

引擎目前只有完整 ToolCall 参数，没有逐片 input_json_delta，而且 on_message_stop 被丢弃。实现时先补 provider 无关的消息/内容块生命周期，从 API/runtime 到 ObserverAdapter 再到 EngineEvent，最终由 CLI 序列化为兼容子集。不要把完整参数伪装成“模型实时生成的参数碎片”。不同供应商缺少原始分块时，可只发完整 assistant/tool_use 事件，并明确能力边界。

为当前 skill creator，完整 `assistant.message.content[].tool_use` 已能走其回退检测；早期分块事件不是第一阶段阻塞项。若尚未支持 partial，必须拒绝对应标志而不是静默忽略。

完成标准：逐行均能独立 JSON 解析；多轮、多工具调用不会串消息；result 与退出码一致；慢消费者与 broken pipe 有明确清理行为。

## 阶段三：权限、错误、资源生命周期

- 沿用现有 PermissionPolicy 与 allow/deny 规则。`-p` 本身不扩大权限，也不把 allowedTools 从工具筛选器暗改成全量授权开关。
- 对仍需人工批准的调用，立即返回结构化拒绝并记录，不读取 stdin。拒绝本身允许 Agent 选择其他合法路径；若最终无法继续，以明确 blocked/error 状态收尾。
- AskUserQuestion 不伪造用户回答；给出不可交互的结果，若任务依赖该答案则结束为需要输入。
- text/json/stream-json 统一错误分类。建议成功 0、运行失败 1、参数错误 2、SIGINT 130、SIGTERM 143；其他业务错误通过 kind 区分。
- 当前文本路径未统一传播 outcome.error，取消也可能返回 Ok；新路径明确检查 cancelled、缺失 TurnComplete、通道关闭及错误。
- 复用 Cancel，补 Unix SIGTERM；验证子进程、MCP、后台 agent 的收尾。同步子任务应结束后再返回；后台任务不能无人监管地泄漏，也不能让命令无限等待。具体关闭策略在实现前固定到协议测试。
- 增加可选总超时/步骤上限时复用引擎能力，确认范围包括启动、工具、清理，且不改变交互模式默认值。

## 阶段四：skill creator 集成

独立提交，依赖非交互模式完成，不与基础 CLI 混在一个补丁里。

1. 将改写描述调用迁移为 scode -p + stdin + text，并使用 scode 可解析的模型标识。
2. 每个评测运行拥有独立目录，在 `.nexus/sudocode/skills/<name>/SKILL.md` 放测试技能，停止使用共享 `.apeiron/commands`。
3. 通过受控配置/技能加载范围排除本机同名技能和祖先目录影响；先复用已有隔离环境机制，不足时增加仅针对技能来源的显式选项，禁止靠全局配置改写实现。
4. 从完整 tool_use 及真实 Read 记录识别目标技能，兼容工具规范名；应区分“尝试读取”和“成功加载”。模型自己声称使用过不算证据。
5. 同时修复现有评测的首个非目标工具即提前失败、异常计为负例通过、进程退出后尾部缓冲未解析等问题。
6. 保留错误/超时为独立运行状态，重试与统计规则显式化。只替换可执行文件名称无法形成可信评测。

## 测试与验收

主要使用仓库已有 mock-anthropic-service、mock_parity_harness 和隔离环境，不依赖真实账户或随机模型判断。

- 参数：-p/--print 等价、前后置参数、冲突参数、纯 stdin、空输入、resume。
- 输入：中文 UTF-8、多行、大输入、慢生产者、空管道、未关闭管道、读取错误。
- Agent：模型→Read/Bash→工具结果→再次模型；技能发现与实际读取；MCP 调用；工具失败后的合法恢复。
- 权限：TTY 和 pipe 下都不等待人工输入；允许/拒绝规则生效；AskUserQuestion 无假回答。
- 输出：text 纯净、JSON 单对象、stream-json 每行有效；最终 result 唯一；工具 input 为对象；消息边界与调用 id 正确。
- 用量：跨多个模型轮次求和，零与未知区分；session 与当前运行分别标注。
- 生命周期：SIGINT/SIGTERM、超时、上游错误、事件通道关闭、broken pipe、子进程清理。
- 回归：旧 prompt JSON 权限断言、compact_output、output_format_contract、stdin_pipe_deadline、resume、REPL 与 ACP 事件处理。
- 跨平台：Linux/macOS/Windows 的 pipe 与 signal 行为；真实账户只做最后少量 smoke，不作为主回归。

新增建议：`tests/headless_mode.rs`、`tests/headless_stream_json.rs`、协议 fixture，以及输出 adapter 的纯函数测试。执行适用的 cargo test、fmt 和 clippy；若补充 EngineEvent 变体，应编译并测试所有匹配该枚举的消费者。

## 交付顺序与工作量

1. PR 1：显式 headless 分流、stdin、权限、text/json、退出行为及回归。
2. PR 2：消息边界、完整 stream-json 事件、partial 兼容范围、flush/取消测试。
3. PR 3：skill creator 调用与评测隔离适配。

代码阅读后的粗估：第一项约 2—3 个工程日，第二项约 2—4 个工程日，第三项约 1—2 个工程日，合计约 5—9 个工程日。假设熟悉 Rust、依赖能正常构建、复用现有 mock 基础设施；跨供应商完整 partial 事件和全量 Claude SDK 兼容另估。这不是实测工时或交付承诺。

优先交付 PR 1 和完整工具事件，即可支撑实际脚本执行与 skill creator 适配；不应为了短参数提前宣称完整 -p 兼容。

## 本分支实施记录（2026-09-28）

分支：`codex/feat-headless-print`，基线仍为 `58effa4b`。

已完成独立 Headless action、后置参数解析、有限 UTF-8 stdin 合并、resume、
text/json/完整消息 stream-json、统一 result 与退出码、权限拒绝记录、
AskUserQuestion 的 needs_input 退出、SIGINT/SIGTERM 取消和引擎关闭等待。
引擎新增 MessageComplete 与 PermissionDenied 事件；旧 Prompt/REPL/ACP 渲染保持原契约。

首版明确采用以下边界：

- partial 事件暂不支持，显式拒绝 `--include-partial-messages`。
- stdin 首字节 3 秒、完整 EOF 30 秒、16 MiB 上限；超时作为输入错误。
- 后台 Bash/PowerShell/agent 调用拒绝执行，要求 `run_in_background: false`。
- stdout 写入等待上限 30 秒；断管后取消并关闭引擎，关闭等待上限 10 秒。
- 用量只报告本次 turn 的已观测值；不生成未知成本。
- 本分支只实现 CLI 能力；阶段四的外部 skill creator 调用迁移与评测隔离不在此补丁内。

macOS 验证：新增 headless/PTY 测试 17 项，旧 compact、JSON 契约、mock parity、
resume、stdin deadline 测试 33 项，runtime conversation 测试 50 项，
CLI 参数单元测试 5 项，共 105 项，全部通过。
新增测试包含真实 Read、多工具消息边界、项目 skill 发现与实际读取、MCP 子进程
调用及回收、两种权限模式、TTY 无问答、stdin 未关闭、大小/编码错误、
provider 失败、SIGINT/SIGTERM 和 broken pipe。模型请求使用隔离 mock，不依赖账户。
`cargo fmt` 和相关 crates 的 `cargo clippy --all-targets` 已执行；仓库既有告警仍存在。
Linux/Windows 执行验证尚未进行。

真实供应商 smoke（2026-09-28）：使用已有 `qwen-plus` / `api-key` 配置验证
stdin 相加得到 `42`、真实 read_file 读取 Cargo.toml 得到 `[workspace]`、
stream-json 五行均可解析且只有一个 result、resume 后纯文本输出一致，退出码均为 0。
另验证了服务端 403 失败路径：输出结构化 runtime_error，并以 1 退出。
测试仅通过命令行选择模型和认证，没有改动用户全局配置。
