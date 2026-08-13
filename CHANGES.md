# 改动说明：system prompt 缓存友好化 + 记忆分层

对应任务：`prompt-optimize.md`（基于 `/Users/jinjingzhou/Documents/prompt-compare/COMPARISON.md` 第 d/e 节）。

## 1. 两项改动

### 改动 1：把日期移出 system prompt（缓存友好化）

**按变动频率对动态区字段重新分组的结论：**

| 字段 | 变动频率 | 处置 |
|---|---|---|
| Model family / Platform | 几乎不变 | 留在 system（`# Environment context`） |
| Working directory | 每个项目一个值，同项目跨会话不变 | 留在 system |
| Is a git repository / instruction 文件数 | 同项目稳定 | 留在 system |
| AGENTS.md 内容（`# Project instructions`） | 同项目稳定（编辑时才变） | 留在 system |
| Runtime config 路径 | 同项目稳定 | 留在 system |
| `# auto memory`（指令+索引+条目） | 指令部分恒定；条目随写入变 | 留在 system（见改动 2） |
| **日期**（原本出现两处：`Date:` 和 `Today's date is`） | **每天变** | **移出 system，改为用户侧注入** |

日期是动态 system block 里唯一逐日变化的字段（实测 49 字符，但任何一个字节变化都会击穿整个动态区的缓存条目）。cwd 虽逐项目不同，但同项目的跨会话缓存复用本来就不受它影响（跨项目复用无论如何做不到——AGENTS.md 等项目数据也在动态区），所以留在 system。移出日期后，**动态 system block 对同一项目做到字节级稳定**：同一项目内今天开的会话可以命中昨天会话写下的缓存前缀（受 TTL 限制），CLI 每回合重建 runtime 跨越午夜时也不再重写 system prompt。

**注入机制**（`runtime/src/conversation.rs`，复用现有的日期回滚 reminder 基础设施）：

- 首个用户回合，在用户内容**之后**追加一个内容块：
  `<system-reminder>Today's date is YYYY-MM-DD.</system-reminder>`
  （追加而非前置，是因为回合标签/file-intent 取的是第一个 Text 块——前置会让标签变成 reminder 文本。）
- 会话中途日期变化时，沿用原有的 rollover reminder（文案微调：原文说 "The system prompt was cached on …"，现在 system prompt 里已没有日期，改为 "The previously announced date was …"）。
- 判断"是否需要注入"靠扫描 session 消息中的 marker（而非一次性标志位）：compaction 把携带日期的首条消息裁掉后，下一回合会自动重新注入（有测试覆盖）。
- 所有 runtime 构造点（CLI REPL / `--print` / ACP / 子 agent / spawn_task / managed agent）都已设置 `with_session_known_date(...)`，逐一核实过，注入在全部入口生效。managed agent 原本 prompt 里是 `Date: unknown`，现在反而第一次拿到了真实日期。

**连带修复**（注入块的显示副作用，属于本改动的直接血区）：

- `cli/format.rs`：`--resume` 回放转录时跳过 `<system-reminder>` 开头的 Text 块（否则日期块会显示在用户输入的回显里）。
- `main.rs`：resume 后给 ↑ 键补历史时同样跳过——否则历史条目变成多行文本，↑ 召回时内嵌换行会把首行直接提交（`pty_cancel::resume_seeds_history_for_up_arrow` 在改动后确实这样失败了，加过滤后恢复绿色）。这两处此前对 rollover reminder 也有同样的潜在问题，只是 rollover 罕见没暴露。

**未做的部分**：cwd/AGENTS.md/MEMORY.md/skills 整体搬到 meta_user 附件（ZCode 全量方案）没有做。理由：scode 没有现成的"首条 user message 附件"组装层，凭空引入一条合成 user message 会改变所有前端（CLI/ACP/子 agent）的消息形状，血区远大于收益；而这些字段本身逐项目稳定，留在 system 不产生逐日缓存击穿。日期走的用户侧注入复用了已有 reminder 通道，是代价最小的等效做法。动态区内部未重排顺序：当前只有静态/动态两个缓存断点，动态区内部顺序对缓存无影响，重排是纯扰动（若将来加第三个断点再排）。

### 改动 2：记忆 prompt 分层

scode **有**"声明记忆权限的子 agent"概念：`custom_agents.rs` 解析 `.md` frontmatter 的 `memory: user|project|local` 字段（此前只解析、无消费方）。因此按 ZCode 方式分层：

- **主循环及内置子 agent**：`# auto memory` 换成新写的**压缩版**（`build_compact_memory_instructions`，2,991 字符，渲染于 `runtime/src/memory/mod.rs`）。保留的操作要点：
  - 写入触发时机（显式要求立即存/忘；学到持久信息时存）
  - 四种 type 的一行区分（user / feedback / project / reference，含"相对日期转绝对日期"）
  - frontmatter 结构（原样保留 `name/description/type` 模板，与 entry 解析器兼容）
  - MEMORY.md 一行索引格式 + 不往索引写正文（原文的"200 行截断"说法经核对代码为失实，已改，见下方格式契约核对）
  - 写前查重、更新而非重复、错了就删
  - 不该存什么（repo 可推导的、git 史、AGENTS.md 已有的、临时状态），含"用户坚持要存时问 non-obvious 的部分"
  - 防陈旧："memories are observations from the past, not guarantees about the present"，用前 read/grep 核实，冲突时信当前状态并修正记忆
  - 砍掉的：12 组 user/assistant 示例、`<types>` XML 教学展开、Why/How-to-apply 的长解释、与 Plan/Task 机制的对比章节
  - 压缩版是**按上面清单重新写的**，措辞沿用 scode 原文的用语（"auto memory" 标题、Write tool、budget 措辞等），未照抄 ZCode。
- **完整版 12.7k 未删**：原函数改名为 `build_full_memory_instructions`，除删去一句失实的"200 行截断"（见格式契约核对）外原文保留，路由给 frontmatter 声明了 `memory:` 的自定义子 agent（`prompt.rs::memory_prompt_variant_for_agent`，经 runtime 自己的 `find_custom_agent` 查找，未触碰 tools/lib.rs）。
- MEMORY.md 索引透传和已加载条目的渲染两个变体完全一致（16k 预算、超限 drop 通知等行为不变）。

### 格式契约核对（压缩版 vs `entry.rs` / `index.rs` / `loader.rs` 解析侧）

分工前提：memory 模块只负责**读**（1,271 行），写入完全靠模型照 prompt 调 Write——所以压缩版必须教出解析器认得的格式。逐条核对结果：

| 解析侧契约（代码） | 压缩版怎么教 | 结果 |
|---|---|---|
| 首行必须是 `---`，且必须有闭合 `---`（`entry.rs:104-120`，缺失分别报 `MissingFrontmatter`/`UnterminatedFrontmatter`） | 模板首行即 `---`，并明文 "The file must begin with the `---` line" | ✓ |
| `name:`、`description:` 顶层必需，值取行内剩余部分（单行）（`entry.rs:140-143,166-167`） | 模板含两字段；明文 "All three fields are required (single line each)" | ✓ |
| type 两种写法：`metadata:` + 缩进 `type:`（规范形）或顶层 `type:` 简写（`entry.rs:144-161`，注释 "for friendliness"） | 教顶层 `type:` 简写（与完整版一致，两者解析器都收） | ✓（教了合法子集） |
| type 合法值恰好四个小写串：`user` / `feedback` / `project` / `reference`（`entry.rs:42-50`，其他值报 `UnknownType`） | 模板枚举四值；明文 "`type` must be exactly one of the four lowercase values" | ✓ |
| 值允许单/双引号包裹（`entry.rs:175-186` unquote） | 未教（教的裸值本来就合法） | ✓（不必教） |
| **解析失败静默跳过**（`loader.rs:214` `if let Ok(entry)`——坏文件不报错、不加载，用户和模型都不知道） | 新增明文警告 "a file that fails to parse is skipped silently, so keep the format exact" | ✓（本次补上；这是风险最高的一条） |
| 条目文件发现规则：`*.md`（扩展名大小写不敏感）、跳过 `.` 开头隐藏文件、跳过 `MEMORY.md`（**文件名大小写不敏感地跳过**）（`loader.rs:196-216`） | 明文 "one `.md` file"；索引 "exact uppercase name" | ✓ |
| 索引行解析：行首（允许缩进）`- ` 或 `* `，含 `[title](file)`，`)` 后尾巴去掉 `—`/`-`/`:` 前缀作 hook；非 bullet 行忽略（`index.rs:53-97`） | 教 `- [Title](file.md) — one-line hook`，与解析器逐字符兼容 | ✓ |
| 渲染上限：`ENTRY_BODY_CHAR_CAP = 2_000`（超长 body 截断）、`RENDERED_CHAR_CAP = 16_000`（超预算条目丢弃）（`mod.rs:31-34`） | 明文 "Bodies render up to 2,000 chars; the whole section caps at 16,000" | ✓（数字与常量一致） |

新增回归测试 `memory/mod.rs::taught_format_round_trips_through_the_parsers`：按 prompt 教的模板逐字构造条目文件（含四个 type 值各一遍）和索引行，断言 `MemoryEntry::parse` / `ParsedIndex::parse` 全部接受——prompt 与解析器将来漂移会在 CI 直接红。

**核对中发现的 prompt-代码不一致（以代码为准处理）：**

1. **"lines after 200 will be truncated"（索引 200 行截断）在代码里不存在**——`index.rs`/`mod.rs` 对索引原文没有任何行数截断（这句是 CC memdir.ts 行为的残留，佐证见 `mod.rs:174` 注释 "matching CC's buildMemoryLines()"）。压缩版**不再写这句**，完整版里的同一句也已删去失实的数字（仅此一处改动，其余原文未动）。
2. 顺带发现的代码侧事实（未改代码，仅记录）：16k 预算**只约束条目**——`render_for_prompt_with` 先无条件拼入索引原文再逐条检查预算，超大 MEMORY.md 仍会整体进 prompt；"budget dropped" 通知只针对条目。压缩版措辞（"the whole section caps at 16,000"）与实际行为的偏差在于超大索引这一角落，属可接受近似。

`scode system-prompt --output-format json` 实测（同一临时 cwd、空记忆目录、macOS debug build）：

**总量与分区：**

| 指标 | 改动前 | 改动后 | 变化 |
|---|---|---|---|
| 总字符数 | 24,227 | 14,635 | **−39.6%** |
| 静态区（7 节，scope:global 缓存） | 10,566（43.6%） | 10,566（72.2%） | 字节不变 |
| 动态区（5 节，ephemeral 缓存） | 13,659（56.4%） | 4,067（27.8%） | **−70.2%** |
| 动态区里"每天变"的内容 | 49 字符 / 2 行（两处日期） | **0** | 消除逐日缓存击穿 |

（说明：49 字符看着小，但动态 system block 只有一个尾部缓存断点，任何一个字节变化都会使整个 ~13.7k 动态前缀 miss。）

**各 section 字符数（按大小排序）：**

| Section | 改动前 | 改动后 |
|---|---|---|
| `# auto memory` | 12,764（52.7%） | **3,223（22.0%）** |
| `# Using your tools` | 3,571 | 3,571 |
| `# Doing tasks` | 2,171 | 2,171 |
| `# Executing actions with care` | 1,556 | 1,556 |
| `# System` | 1,226 | 1,226 |
| intro | 749 | 749 |
| `# Output efficiency` | 730 | 730 |
| `# Tone and style` | 551 | 551 |
| `# Environment context` | 297 | 277（去掉 `Date:`） |
| `# Project context` | 271 | 240（去掉 `Today's date is`） |
| `# Available SudoCode plugins` | 238 | 238 |
| `# Runtime config` | 81 | 81 |

（记忆条目/索引非空时，两侧都会在 16k 预算内额外增加相同的渲染量，对比不受影响。声明 `memory:` 的自定义子 agent 的 prompt 仍含完整版 12.7k 记忆指令。）

## 3. 行为抽查

改动后的 debug 二进制、`scode --print`、模型 `claude-opus-4-8`（经用户配置里唯一可用的 sudorouter proxy）。

**① 日期/cwd 上下文（验证移出 system 后模型仍拿得到日期）**

```
$ scode --print --permission-mode read-only "What is today's date and what is
  your current working directory? Answer in one short line."

⏺ Today is 2026-08-14 and the working directory is /home/user/desktop/config-man
  ager.
[claude-opus-4-8] · turn 1 · 17.6k tokens · $0.27 · 3.2s
```

日期 **2026-08-14 正确** —— 来自首回合注入的 user 侧 announcement（system prompt 里已无日期）。cwd 答错，但追问后模型明确说请求里有**两个** working directory：scode 注入的真实长路径之外，还有一个 `/home/user/desktop/config-manager`。该字符串在 scode 代码库里 grep 不到，来源是 **sudorouter proxy 在请求里加的自有 envelope**，与本改动无关（改动前该污染同样存在）；`scode system-prompt` 输出里的 Working directory 已核实是正确路径。

**② 记忆写入（验证压缩版指令下模型仍知道怎么写、写到哪）**

```
$ SUDOCODE_MEMORY_DIR=<tmp>/mem scode --print --permission-mode danger-full-access \
    "Remember this: I prefer bun over npm for all JavaScript package management.
     Save it to memory now."

🔧 write_file ✓  read_file ✗  write_file ✓ (3 tools, 18.7s)
⏺ Saved. I'll use `bun` instead of npm for all JavaScript package management going forward.

$ cat <tmp>/mem/package-manager-preference.md
---
name: Prefers bun over npm
description: User prefers bun over npm for all JavaScript package management
type: user
---

User prefers **bun** over npm for all JavaScript package management.

**How to apply:** Use `bun` commands (bun install, bun add, bun run, etc.) ...

$ cat <tmp>/mem/memory.md
- [Prefers bun over npm](package-manager-preference.md) — use bun instead of npm for all JS package management
```

frontmatter 结构、type 选择、独立条目文件、索引一行格式全部正确；中间那次 `read_file ✗` 是模型先尝试读索引查重（写前查重行为保留）。一个瑕疵：索引文件名写成了小写 `memory.md`。对照 loader 代码，后果是：macOS（大小写不敏感 FS）能照常当索引加载；Linux 上 `load_index` 找不到 `MEMORY.md`，而 `load_entries` 又**大小写不敏感地把它当索引跳过**（`loader.rs:208`）——即完全不可见但不报错。此瑕疵抽查发生在契约加固**之前**；压缩版现已明文 "(exact uppercase name)"（完整版原文同样只给文件名，未更强调）。

**②b 契约加固后复跑**（压缩版补上"格式精确/静默跳过/大写 MEMORY.md"后，换一条新记忆再跑一次并用加载器闭环验证）：

```
$ SUDOCODE_MEMORY_DIR=<tmp>/mem2 scode --print --permission-mode danger-full-access \
    "Remember this: our CI requires Node 22, never suggest older versions. Save it to memory now."
🔧 write_file ✓  Bash ✓  write_file ✓ (3 tools, 20.5s)

$ ls <tmp>/mem2
ci-node-22.md  MEMORY.md          # ← 这次索引是大写
$ head -5 <tmp>/mem2/ci-node-22.md
---
name: CI requires Node 22
description: CI pipeline requires Node 22; never suggest older Node versions
type: project
---

# 闭环：用真实加载器读回模型写的文件
$ SUDOCODE_MEMORY_DIR=<tmp>/mem2 scode system-prompt | grep -A4 "Loaded memory files"
## Loaded memory files
- name: CI requires Node 22  type: project  description: CI pipeline requires Node 22; ...
  body: CI requires Node 22. Never suggest older Node versions.
```

模型按压缩版写出的文件被 `entry.rs`/`loader.rs` 原样解析并渲染回 prompt——写侧（prompt 教学）与读侧（模块解析）契约闭环成立。

**③ 普通编码任务（验证行为纪律没丢）**

```
$ scode --print --permission-mode read-only "Read fib.py and tell me its time
  complexity and the single most effective fix. Be brief."

🔧 read_file ✗  glob_search ✓  read_file ✓ (3 tools, 12.2s)
⏺ Time complexity: O(2^n) — exponential, due to redundant recursive recomputation.
  Fix: Memoize. Add `from functools import lru_cache` and decorate `fib` with
  `@lru_cache(None)`, which drops it to O(n).
```

用了专用 Read/Glob 工具（没有退化成 Bash cat/grep），回答简洁正确 —— 工具纪律与输出效率纪律未受影响。

## 4. 测试变更

**更新：**

- `conversation.rs::skips_date_change_reminder_when_known_date_unchanged` → 改名为 `first_turn_announces_current_date_after_user_content`：原测试断言"日期一致时首回合无注入"，这正是本次要改变的行为（现在首回合必须宣告日期）。
- rollover 三个既有测试（`injects_date_change_reminder_when_local_date_rolls_over` 等）**未改动断言**，新逻辑下仍通过（rollover 仍是前置、仍含新旧两个日期、仍只发一次）。

**新增：**

- `conversation.rs::date_announcement_is_not_repeated_on_later_turns` — 会话已携带日期时不重复注入。
- `conversation.rs::date_announcement_reinjected_when_session_lost_it` — 模拟 compaction 丢失日期块后自动补注。
- `prompt.rs::rendered_prompt_carries_no_date` — system prompt 任何位置不得出现日期。
- `prompt.rs::memory_variant_full_only_for_custom_agents_declaring_memory` — 只有声明 `memory:` 的自定义 agent 拿完整版；无声明的自定义 agent 与内置 agent（Explore 等）拿压缩版。
- `memory/mod.rs::compact_instructions_stay_small_and_keep_operational_core` — 压缩版 <3k 且逐项包含操作要点、不含 `<types>` 教学块；并断言含"静默跳过"警告、含真实上限 2,000/16,000、不含失实的 "after 200"。
- `memory/mod.rs::full_instructions_render_only_for_full_variant` — 变体路由正确、索引/条目渲染与变体无关、默认路径为压缩版。
- `memory/mod.rs::taught_format_round_trips_through_the_parsers` — prompt 教的条目/索引格式必须被 `entry.rs`/`index.rs` 解析接受（契约回归锁）。

**删除：无。** PTY 记忆测试（`pty_memory.rs`）断言的 `# auto memory` 标题、索引透传、条目渲染、16k 预算文案全部保留，未改一行即通过。

## 5. 风险与不确定性

1. **模型看到日期的位置变了**（system 尾部 → 首条 user message 尾部的 system-reminder）。CC 现行版本就是这么做的（日期走用户侧上下文），模式本身经过验证；但 scode 的提示词整体是"教科书式"旧架构，无法排除个别模型对位置敏感。行为抽查里日期问答正常，但没有大规模回归。
2. **compaction 后的日期恢复**依赖 marker 扫描：如果压缩摘要恰好逐字引用了 `<system-reminder>Today's date is` 这个片段，会误判为"已有日期"而不补注。概率低，后果轻（模型可用 Bash `date` 自救，且日期变更时 rollover reminder 仍会触发）。
3. **记忆压缩的行为保真度**：压缩版保住了全部操作规则，但丢了 12 组示例。对 Claude 系模型影响预计很小（行为抽查里记忆写入格式完全正确）；如果 scode 未来配非 Claude 模型，示例的缺失可能降低遵从率——届时可把完整版按需注入（见下）。
4. **完整版目前实际无人消费**：仓库/用户目录里若没有声明 `memory:` 的自定义 agent，完整版是死代码。建议后续：(a) 在文档里写明自定义 agent 加 `memory: project` 即得完整方法论；(b) 或在模型首次写记忆的回合按需注入完整版（一次性教学）。
5. **`memory:` 的取值（user/project/local）目前只当布尔用**：scode 尚无按 scope 切换记忆目录的实现（现状是按 agent 类型分目录），本次不扩，避免范围蔓延。
6. `session.rs` 的 undo/export 等路径仍会把日期块当普通文本处理（export 属如实导出，未过滤；undo 不受影响）。
7. clippy 门槛说明：`cargo clippy --workspace --all-targets -- -D warnings` 在本分支**改动前就不通过**（telemetry 等 crate 存在大量既有 pedantic warning）。已用 stash 前后对比验证：本改动引入的新 warning 数为 **0**（lib 195→195，all-targets 各 target 计数一致）。

## 6. 降级方案说明

未采用降级方案（重排不移出）。改动 1 实际落地的是"**只把唯一逐日变化的字段（日期）移出 system**，其余按稳定性论证后留在 system"，达成了动态区字节级稳定的目标，同时避免了整体搬迁 meta_user 的大血区。动态区内部重排因当前只有一个尾部缓存断点、纯属无效扰动而未做（理由见改动 1 末尾）。
