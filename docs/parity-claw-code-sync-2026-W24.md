# claw-code cherry-pick triage

Source: `ultraworkers/claw-code` since `357629dbd9b3` (LAST_PARITY_SYNC_COMMIT).

Total non-merge commits in window: **614**.

## Summary by category

| Tag | Description | Count |
|---|---|---|
| `[SKIP-omx]` | claw-code internal multi-agent orchestration | 112 |
| `[SKIP-g004]` | claw-code internal architecture (G004 / approval token / lane events) | 20 |
| `[SKIP-provider]` | provider-specific (we use sudorouter) | 17 |
| `[SKIP-claw-internal]` | claw-code internal probes / dogfood / branding | 10 |
| `[PICK-hint]` | typed error envelope / hint field — candidate (verify CC alignment) | 80 |
| `[PICK-tool]` | tool surface (bash / file ops / web / agent / etc.) | 4 |
| `[PICK-slash]` | slash command surface | 48 |
| `[PICK-acp]` | ACP transport / SDK surface | 5 |
| `[PICK-permissions]` | permission / sandbox / safety | 9 |
| `[PICK-session]` | session / resume / compact / config | 64 |
| `[PICK-mcp]` | MCP server / plugin / hook lifecycle | 20 |
| `[PICK-tui]` | TUI / rendering / streaming display | 8 |
| `[PICK-cli]` | CLI flags / arg parsing / output format | 21 |
| `[PICK-doctor]` | diagnostics / doctor / status / cost | 8 |
| `[REVIEW]` | needs manual triage | 188 |

## `[SKIP-omx]` - claw-code internal multi-agent orchestration (112)

| Date | SHA | Subject |
|---|---|---|
| 2026-05-15 | `a92e5b2` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `0eddcca` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `ab27f61` | omx(team): auto-checkpoint worker-4 [7] |
| 2026-05-15 | `4cd2bb8` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `9278748` | omx(team): auto-checkpoint worker-4 [7] |
| 2026-05-15 | `02889d7` | omx(team): auto-checkpoint worker-3 [6] |
| 2026-05-15 | `7b63c0a` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `de0f1bb` | omx(team): auto-checkpoint worker-2 [4] |
| 2026-05-15 | `eb7a208` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `11c6a60` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `2221dd4` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `d7f1ad7` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `d04a74c` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `0f87178` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `fb9095c` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `5155225` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `c9b34a2` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `5e0cf62` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `51fa5a7` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `33ac5c3` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `89d1052` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `afd8808` | omx(team): auto-checkpoint worker-2 [5] |
| 2026-05-15 | `fc35dc8` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `c886cbc` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `0940253` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `0bb1451` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `7b21ac1` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-15 | `2db0a5f` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `8c9e41a` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `3767add` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-15 | `b63a1bf` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `ea95bf2` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `dec8efa` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `ce02ace` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `bc32639` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `a212c66` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `1a110bd` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `685f078` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `e4ef0f7` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `76581f7` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-15 | `82ec223` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-15 | `a6ca5c4` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-15 | `3ff8743` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-15 | `29029bf` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `98204a7` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `b655d49` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `ccd99a5` | omx(team): auto-checkpoint worker-2 [2] |
| 2026-05-15 | `0bcab57` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `4a76632` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `9910d58` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `39568fe` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `686cc89` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `d3ae7be` | omx(team): auto-checkpoint worker-2 [2] |
| 2026-05-15 | `2831c45` | omx(team): auto-checkpoint worker-2 [2] |
| 2026-05-15 | `ace2601` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-15 | `983ceb9` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `cac73b4` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-15 | `985c6e9` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `f0e8896` | omx(team): auto-checkpoint worker-2 [2] |
| 2026-05-15 | `2454f01` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `17b4ab4` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `80b8984` | omx(team): auto-checkpoint worker-4 [5] |
| 2026-05-15 | `b01192d` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-15 | `12ca555` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `1a6e475` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `f2ba364` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-15 | `76920c7` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-15 | `0a14f85` | omx(team): auto-checkpoint worker-4 [5] |
| 2026-05-15 | `18805b5` | omx(team): auto-checkpoint worker-2 [2] |
| 2026-05-15 | `6d809cb` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `d3f8ff9` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `5c40d4e` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `5625ba5` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `6a37442` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `0bca524` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `1fbde9f` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `0b0d55d` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `5cebdd9` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `e34209f` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `ff37d39` | Stabilize G004 contract integration after worker merges |
| 2026-05-14 | `f8d744b` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `c8c936e` | omx(team): auto-checkpoint worker-3 [6] |
| 2026-05-14 | `57b3e32` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `06e5453` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `ed3ccae` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-14 | `f4e08d0` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `16d6525` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `aec291c` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-14 | `307b23d` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-14 | `79d3b80` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-14 | `9ec4d83` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-14 | `5f45740` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-14 | `a6ee51b` | omx(team): auto-checkpoint worker-3 [unknown] |
| 2026-05-14 | `6df60a4` | omx(team): auto-checkpoint worker-2 [unknown] |
| 2026-05-14 | `964458a` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `9bc55f9` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `2c48400` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `713ca7a` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `02b591a` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `f789525` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `ad9e023` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `145413d` | omx(team): auto-checkpoint worker-4 [5] |
| 2026-05-14 | `17da296` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `9ab569e` | omx(team): auto-checkpoint worker-2 [3] |
| 2026-05-14 | `4af5664` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `1864ce3` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `74cc590` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `8d0cee4` | omx(team): auto-checkpoint worker-3 [4] |
| 2026-05-14 | `5c77896` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `74bbf4b` | omx(team): auto-checkpoint worker-4 [unknown] |
| 2026-05-14 | `481585f` | omx(team): auto-checkpoint worker-1 [1] |
| 2026-05-14 | `8311655` | omx(team): auto-checkpoint worker-1 [1] |

## `[SKIP-g004]` - claw-code internal architecture (G004 / approval token / lane events) (20)

| Date | SHA | Subject |
|---|---|---|
| 2026-05-15 | `f27bd46` | Update ultragoal ledger for G009 completion |
| 2026-05-15 | `5294648` | Record G009 release readiness quality gate |
| 2026-05-15 | `d6b4349` | Keep G007 mock parity references executable |
| 2026-05-15 | `7ce6b78` | Document G007 mock parity verification boundaries |
| 2026-05-15 | `c522dc9` | Preserve plugin lifecycle JSON in G007 CLI output |
| 2026-05-15 | `0cd1eab` | Keep G007 plugin command integration compiling |
| 2026-05-15 | `65a144c` | Keep G006 packet regressions aligned with shipped schema |
| 2026-05-15 | `f7235ca` | Make G006 task policy state machine executable |
| 2026-05-14 | `8f7eaff` | Close the G005 verification gaps before checkpoint |
| 2026-05-14 | `879962b` | map g004 event report verification lanes |
| 2026-05-14 | `7214573` | Keep approval token contracts in their own runtime module |
| 2026-05-14 | `dcf11f8` | harden report contract projection identity |
| 2026-05-14 | `e1641aa` | Prove G004 contract bundles are machine-checkable |
| 2026-05-14 | `bf533d7` | task: approval token chain |
| 2026-05-14 | `2012718` | Map G003 boot session verification |
| 2026-05-14 | `087e31d` | Keep G003 integrated runtime tests compiling |
| 2026-05-14 | `37b2b75` | Keep G002 path-scope tests aligned with enforced denials |
| 2026-05-14 | `534442b` | Document G002 security verification ownership for integration |
| 2026-05-14 | `45b43b5` | Make the CC2 board schema executable for G001 |
| 2026-05-14 | `424825f` | task: G001 human board and docs rendering |

## `[SKIP-provider]` - provider-specific (we use sudorouter) (17)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-08 | `01f4dd4` | docs: mention local Ollama reasoning setup |
| 2026-06-08 | `222faab` | fix(cli): hint Ollama for Qwen tags |
| 2026-06-08 | `7503c1c` | fix(providers): parse Ollama reasoning fields |
| 2026-06-08 | `c164661` | fix(providers): preserve OpenAI-compatible reasoning history |
| 2026-06-06 | `0755ddf` | fix(providers): strip provider prefix from model names for openai_compat endpoints |
| 2026-06-05 | `eaa2e32` | docs: add ROADMAP #833 — native Ollama provider support via OLLAMA_HOST |
| 2026-06-05 | `be8112f` | feat: add native Ollama provider support via OLLAMA_HOST env var |
| 2026-06-03 | `bcc5bfd` | fix: route local OpenAI-compatible models |
| 2026-06-03 | `54d785d` | fix: preserve DeepSeek V4 thinking history |
| 2026-05-25 | `f003a10` | fix: remove stale retry_after refs from openai_compat.rs |
| 2026-05-25 | `0136944` | chore: sync Cargo.lock and openai_compat.rs to main (stash artifact cleanup) |
| 2026-05-25 | `3364dc4` | chore: fix conflict markers and cargo fmt drift in main (commands, openai_compat, trident, config, tools) |
| 2026-05-25 | `b071fac` | feat: add native Gemini support to openai_compat provider |
| 2026-05-25 | `fdcb05b` | fix: echo reasoning_content back for DeepSeek V4 multi-turn tool calls |
| 2026-05-25 | `f72681f` | fix: recognize OPENAI_API_KEY as valid auth for OpenAI-compatible endpoints |
| 2026-05-15 | `dccb3e7` | Stabilize OpenAI-compatible mock transport verification |
| 2026-05-10 | `8cada12` | Add Qwen model token limits for DashScope compatibility |

## `[SKIP-claw-internal]` - claw-code internal probes / dogfood / branding (10)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `61e8ad9` | docs: add gajae-code to Ecosystem section |
| 2026-06-03 | `78f446f` | test: add argv-safe dogfood probes |
| 2026-05-28 | `0e6d48d` | docs: record argv-safe dogfood probe gap (#3186) |
| 2026-05-28 | `9ac66cb` | docs: quote dogfood build trap cleanup guidance (#3183) |
| 2026-05-28 | `773aa02` | docs: use trap cleanup in dogfood build guidance (#3182) |
| 2026-05-28 | `5c3e1c1` | fix: add dogfood build help handling (#3181) |
| 2026-05-26 | `6e44da1` | Record stale local dogfood probe trap (#3114) |
| 2026-05-15 | `c910063` | Close the ultragoal ledger after final gate |
| 2026-05-15 | `2e93264` | Record G011 ultragoal completion |
| 2026-05-15 | `cf5eb15` | Update ultragoal ledger for G010 completion |

## `[PICK-hint]` - typed error envelope / hint field — candidate (verify CC alignment) (80)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `33f771f` | docs: mark ROADMAP #77 DONE (classify_error_kind implemented) |
| 2026-06-05 | `e4b8f9c` | fix: return typed error for unsupported MCP sub-actions |
| 2026-05-29 | `ac5b19d` | fix: interactive_only hint omits --resume for non-resume-safe commands (#829) |
| 2026-05-29 | `fdfb9f4` | docs: record #829 - interactive_only hint incorrectly suggests --resume for non-resume-safe commands |
| 2026-05-29 | `187aebd` | fix: /approve and /deny outside REPL emit interactive_only error_kind (#828) |
| 2026-05-29 | `9d05573` | fix: unknown slash command emits unknown_slash_command error_kind (#827) |
| 2026-05-29 | `5890291` | docs: record #827 - resume unknown slash command emits opaque error_kind:unknown |
| 2026-05-29 | `b4b1ba1` | fix: route all JSON-mode abort envelopes to stdout (#819 #820 #823) (#3197) |
| 2026-05-29 | `42aff26` | docs: record interactive_only error class JSON stderr routing gap (#820) |
| 2026-05-28 | `c4770e6` | docs(roadmap): add #811 json error envelope nontty hangs (#3171) |
| 2026-05-27 | `fcebf64` | fix(#802): four resume-mode and broad-cwd error envelopes now include hint field |
| 2026-05-27 | `53953a8` | fix(#801): diff non-git-dir error envelope now includes error_kind, hint, and message fields |
| 2026-05-27 | `efb1542` | fix: empty-prompt error now returns non-null hint via newline-delimited usage string |
| 2026-05-27 | `bff3700` | fix: plugins extra-arg errors now return non-null hint via newline-delimited usage string |
| 2026-05-27 | `18b4cee` | fix(#795): skill_not_found and unsupported_skills_action now return non-null hints via fallback table |
| 2026-05-27 | `93a159d` | fix(#791): config extra-arg errors now return non-null hint via \n-delimited usage string |
| 2026-05-27 | `9968a27` | fix(#790): system-prompt unknown-option errors now return typed unknown_option kind + non-null hint |
| 2026-05-27 | `abdbf61` | fix(#788): skills show not-found emitted duplicate JSON error envelope; use exit(1) instead of Err propagation |
| 2026-05-27 | `113145a` | fix(#787): --resume with directory path returns session_path_is_directory kind + hint; wire fallback_hint_for_error_kind into both resume error emission sites |
| 2026-05-27 | `22b423b` | fix(#786): dump-manifests --manifests-dir missing-value errors now return typed missing_flag_value kind + non-null hint |
| 2026-05-27 | `e628b4b` | fix(#784): export --output missing-value and extra-positional errors now return typed error_kind + non-null hint |
| 2026-05-27 | `81fe0cc` | fix(#783): init JSON envelope now includes hint and already_initialized fields for orchestrator parity |
| 2026-05-27 | `32c9276` | fix(#782): acp unsupported invocation now returns non-null hint with newline-delimited remediation text |
| 2026-05-27 | `16c1117` | fix(#781): sub-classify api_auth_error/api_rate_limit_error from api_http_error; add fallback_hint_for_error_kind for hint-less API errors |
| 2026-05-27 | `364e790` | fix(#779): resumed /skills invocation returns interactive_only error_kind + non-null hint |
| 2026-05-27 | `fded4f6` | fix(#778): doctor check JSON objects now include hint field with stable remediation text for warn/fail checks |
| 2026-05-27 | `e020303` | fix(#777): resumed /plugins mutations return interactive_only error_kind + non-null hint instead of unknown+null |
| 2026-05-27 | `2684737` | fix(#776): resume command errors now return typed error_kind + non-null hint (invalid_history_count, session action errors) |
| 2026-05-27 | `028998d` | test(#775): integration tests for #769-#771 interactive-only guards and #774 hint fields; fix stale classifier unit test string |
| 2026-05-27 | `c760a49` | fix(#774): agents/plugins/mcp unknown-subcommand errors now include non-null hint |
| 2026-05-27 | `212f0b2` | fix(#772): slash command aliases now resolve to canonical forms in interactive_only guidance |
| 2026-05-27 | `bf212b9` | fix(#771): init rejects extra args; usage/stats/fork return interactive_only instead of credential check |
| 2026-05-27 | `3a1d883` | fix(#770): cost/clear/memory/ultraplan/model with args now return interactive_only instead of falling to credential check |
| 2026-05-27 | `9e1be05` | fix(#769): claw session <arg> now returns interactive_only instead of falling to credential check |
| 2026-05-27 | `b778d4e` | fix(#768): --resume non-slash trailing arg now has error_kind:invalid_resume_argument + hint |
| 2026-05-27 | `d29a8e2` | fix(#765): login/logout removed_subcommand now has error_kind + non-null hint |
| 2026-05-27 | `4ea255c` | fix(#764): config_parse_error now populates hint field via Display newline delimiter |
| 2026-05-27 | `88ce181` | test(#762): classify_error_kind now covers all 23 classifier arms (was 8 of 23) |
| 2026-05-27 | `d83de56` | fix(#761): mcp server_not_found and skill_not_found envelopes now include hint field |
| 2026-05-26 | `7fa81b5` | fix(#760): agent_not_found and plugin_not_found envelopes now include hint field |
| 2026-05-26 | `02d77ae` | fix(#757): --permission-mode invalid and --allowedTools missing now emit typed error_kind and hint |
| 2026-05-26 | `4df1461` | fix+test(#756): missing/invalid flag-value errors now emit typed error_kind and non-null hint |
| 2026-05-26 | `c70312b` | fix(#754): missing_credentials hint now newline-delimited so JSON hint field is non-null |
| 2026-05-26 | `e932713` | fix+test(#753): claw -p (no arg) parity with #750: error_kind:missing_prompt with non-null hint |
| 2026-05-26 | `cfc2672` | fix(#752): cli_parse unrecognized-arg errors now emit non-null hint for all subcommands |
| 2026-05-26 | `ddc71b5` | test(#751): regression guard for #750 prompt no-arg error_kind and hint contract |
| 2026-05-26 | `ac925ed` | fix(#750): claw prompt (no arg) now emits error_kind:missing_prompt with non-null hint |
| 2026-05-26 | `2dfb7af` | fix+test(#749): compact interactive-only hint now non-null; extend compact JSON test for hint contract |
| 2026-05-26 | `3975f2b` | fix(#748): mcp unknown subcommand now emits error_kind:unknown_mcp_action matching agents/plugins parity |
| 2026-05-26 | `18e7744` | fix(#746): non-TTY interactive-only error populates hint field via newline split |
| 2026-05-26 | `3c5459a` | fix(#745): bare slash command guidance adds newline before hint; claw issue/pr/commit etc now have non-null hint |
| 2026-05-26 | `1d5db5f` | fix(#743): plugins help --output-format json now emits usage envelope matching agents/mcp/skills help shape; resolves #420 |
| 2026-05-26 | `6e78c1f` | fix(#741): config unsupported_config_section error now populates hint field; list/show/help verbs get usage hint |
| 2026-05-26 | `d5f0d6e` | fix(#739): skills unknown-subcommand JSON path no longer emits double error envelope; help action not propagated as Err |
| 2026-05-26 | `4c3cb0f` | fix(#738): interactive-only slash command error adds newline before hint; hint field now non-null with remediation text |
| 2026-05-26 | `b3242e8` | fix(#735): classify_error_kind: /compact and other interactive-only slash commands now emit error_kind:interactive_only not unknown |
| 2026-05-26 | `d4494a8` | fix(#734): agents/plugins show not-found envelopes gain message field; parity with skills show |
| 2026-05-26 | `a0c6c8b` | fix(#726): classify legacy_session_no_workspace_binding error_kind in export path |
| 2026-05-26 | `98f8926` | fix(#716): align 5 resume-path error JSON envelopes from legacy type:error shape to standard kind/action/status/error_kind/exit_code contract |
| 2026-05-26 | `dedad14` | fix(#706): skills show <name> returns error+exit1 when skill not found; classify_error_kind covers skill_not_found from prose message |
| 2026-05-25 | `2f9429c` | fix: slash-command guard errors now emit error_kind:interactive_only instead of unknown; covers memory, permissions, review, and any bare_slash_command_guidance path |
| 2026-05-25 | `b8eca2a` | fix(#349): plugins unknown action emits status:error + error_kind:unknown_plugins_action + exit 1 instead of status:ok with prose |
| 2026-05-25 | `36b3626` | fix(#458): add status:ok to config JSON envelope; unknown section now emits status:error + error_kind:unsupported_config_section |
| 2026-05-25 | `de2e32c` | fix: skills install nonexistent path emits skill_not_found error kind with descriptive message; classify_error_kind adds skill_not_found branch |
| 2026-05-25 | `181b12f` | fix: mcp show <nonexistent> now returns status:error + error_kind:server_not_found + exit 1; extend ok:false gate to also check status:error |
| 2026-05-25 | `f9e98a2` | fix(#700): add status:ok to all help JSON envelopes; rename session_list kind to sessions with action:list |
| 2026-05-25 | `eb7c14c` | fix(#458): add status:ok to bootstrap-plan JSON envelope; all 12 JSON surfaces now have uniform status field |
| 2026-05-25 | `11a6e08` | fix(#458): add status field to export and diff JSON envelopes |
| 2026-05-25 | `16604a1` | fix(#458): add status assertions to skills/agents JSON envelope tests |
| 2026-05-25 | `cc1462a` | fix(#458): add status:ok to skills install JSON envelope (missed in previous sweep) |
| 2026-05-25 | `0581894` | fix(#458): add status:ok to agents and skills list JSON envelopes; all 9 subcommands now pass uniform status check |
| 2026-05-25 | `5b79413` | fix(#458): add status field to version/init/system-prompt JSON envelopes; all 9 subcommands now have uniform status field |
| 2026-05-25 | `85e736c` | fix: add status field to sandbox JSON envelope (ok/warn/error derived from enabled+active+supported) |
| 2026-05-25 | `c345ce6` | fix: mcp/agents/skills help envelopes set ok:false + status:error on unknown subcommand; exit 1 propagates correctly |
| 2026-05-25 | `91a0681` | fix(#697): agents unknown subcommand exits 1 with typed error; plugins remove aliases uninstall and errors on not-found |
| 2026-05-25 | `63a5a87` | fix(#696): exit with typed error when stdin is not a TTY and no prompt piped; fix anthropic/ prefix detection in metadata_for_model |
| 2026-05-25 | `3d02baf` | fix(#683): claw skills remove/add/uninstall/delete emits typed error, exit 1 |
| 2026-05-25 | `1f572ff` | fix: add missing config_load_error_kind to test StatusContext initializers; remove stale retry_after refs again |
| 2026-05-25 | `495e7a0` | fix: remove stale retry_after field, Team variant, config_load_error_kind, denied_tools initializer errors |
| 2026-05-11 | `7244a82` | docs(roadmap): add #447 — JSON error envelopes go to stderr; stdout empty on error |

## `[PICK-tool]` - tool surface (bash / file ops / web / agent / etc.) (4)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `5d85739` | fix: detect skill name/dir mismatch and report metadata drift |
| 2026-06-05 | `8fd11e8` | fix: track skill directory name for name/dir mismatch detection |
| 2026-06-03 | `d07664b` | fix: keep hooks clean and close bash stdin |
| 2026-05-11 | `5a4cc50` | docs(roadmap): add #445 — skill name-vs-dirname mismatch silently accepted; sibling silent drops |

## `[PICK-slash]` - slash command surface (48)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-03 | `d58197c` | fix: update slash command count and add /setup assertion in test |
| 2026-06-04 | `3845040` | feat: wizard entry points -- /setup command, claw setup subcommand, and RuntimeProviderConfig |
| 2026-06-05 | `b8cbb18` | fix: /clear preserves session_id to prevent resume divergence (#114) |
| 2026-06-05 | `cb027ad` | fix: /session switch and /session fork return structured JSON in resume mode (#113) |
| 2026-06-05 | `5bdffbe` | docs: mark ROADMAP #330 DONE (verified /cost and /stats work in resume) |
| 2026-06-05 | `04f1886` | fix: strip macOS /private symlink prefix from JSON cwd (#421) |
| 2026-06-05 | `0cab03d` | fix: /model returns structured JSON in resume mode (#343) |
| 2026-06-05 | `6c3d7be` | fix: /tasks returns structured JSON in resume mode (#341) |
| 2026-06-05 | `e3ffaef` | fix: /config help returns structured section list (#344) |
| 2026-06-05 | `cd18cf5` | fix: /config help returns structured section list (#344) |
| 2026-06-05 | `f1a1639` | fix: plugins enable/disable only reloads when state changes (#411) |
| 2026-06-05 | `8d50276` | docs: mark ROADMAP #339 DONE (/session delete resume-safe) |
| 2026-06-05 | `b04b1d6` | feat: detect git rebase/merge/cherry-pick/bisect states (#89) |
| 2026-06-03 | `0c83a26` | test: cover resumed unknown slash command |
| 2026-05-29 | `e50c46c` | docs: extend #821 - config/providers also leak deprecation warning in JSON mode |
| 2026-05-29 | `69b5907` | docs: record status/sandbox/system-prompt JSON stderr deprecation leak (#821) |
| 2026-05-29 | `37a9a54` | docs: record AGENTS.md and .claude/CLAUDE.md instruction cascade gap (#818) |
| 2026-05-27 | `2c3c0f6` | fix(#804): agents/skills show <name> <extra> in text mode returned wrong error instead of unexpected_extra_args |
| 2026-05-27 | `bad1b97` | fix(#803): agents/skills/plugins list --flag in text mode silently returned empty success |
| 2026-05-27 | `9976585` | fix(#796): agents/skills show <name> <extra> returned wrong not-found instead of unexpected_extra_args |
| 2026-05-27 | `abfa2e4` | fix(#792): agents/skills list --flag silently returned empty success; now returns unknown_option error |
| 2026-05-26 | `04eb661` | test(#747): regression guard for #745 bare slash command hint contract (issue/pr/commit) |
| 2026-05-26 | `ad982d2` | fix(#736): boot_preflight doctor details[] null-value entries: add double-space separator to Required binary, Last failed boot, MCP/Plugin eligible format strings |
| 2026-05-26 | `425d94e` | fix(#730): add path field to plugins list/show JSON; completes path-discoverability trio (agents #728, skills #729, plugins #730) |
| 2026-05-26 | `8f44ad3` | fix(#729): add path field to skills list/show JSON; SkillSummary parity with AgentSummary (#728) |
| 2026-05-26 | `fa29909` | fix(#728): add path field to agents list/show JSON; AgentSummary now stores on-disk .toml path from discovery loop |
| 2026-05-26 | `922c239` | fix(#723): add scripts/roadmap-next-id.sh to prevent concurrent ROADMAP id collision; document optimistic-append pattern |
| 2026-05-26 | `02d1f6a` | fix(#720): claw help <topic> now routes to subsystem help instead of cli_parse error; add Agents/Skills/Plugins/Mcp/Config/Diff help topics |
| 2026-05-26 | `fe2b13a` | fix(#719): plugins list <filter> now applies substring filter on plugin id, matching agents/skills parity |
| 2026-05-26 | `556a598` | fix(#718): implement plugins show/info/describe command with not-found error, parity with agents/skills show |
| 2026-05-26 | `a0b375c` | fix(#717): implement agents show/info/describe and list filter commands, mirror skills handler parity |
| 2026-05-26 | `4b8731b` | fix(#715): add action+status fields to resume-path json responses: compact/clear/cost/stats/history/session_exists/session_delete/memory/restored |
| 2026-05-26 | `fdde5e4` | fix(#712): add missing action fields to doctor/status/bootstrap-plan/dump-manifests json responses |
| 2026-05-26 | `bae0099` | fix(#711): add missing action fields to version/system-prompt/export/init json responses; add contract test assertions |
| 2026-05-26 | `47c0226` | fix(#708): skills show/info/describe responses now emit action:show instead of action:list; remove duplicate status key from render_skills_report_json |
| 2026-05-25 | `1a6f54b` | fix(#703): plugins list JSON now has summary:{total,enabled,disabled,load_failures}; drop reload_runtime/target from list response in both top-level and resume paths |
| 2026-05-25 | `a7a3062` | docs(roadmap): add #703 plugins list JSON missing structured summary; leaks reload_runtime/target |
| 2026-05-25 | `9d1998b` | test(#458/#700/#701/#702): add status:ok assertions for help/bootstrap-plan/export-help contracts; add diff/export JSON shape tests |
| 2026-05-25 | `47521cf` | fix(#701): add detail_entries structured key/value to doctor check JSON; booleans/ints emitted as JSON scalars |
| 2026-05-25 | `9c5f190` | docs(roadmap): add #701 doctor details prose-string gap; details[] should be structured key/value objects |
| 2026-05-25 | `10957f5` | docs(roadmap): add #699 bootstrap-plan/dump-manifests local dispatch gap |
| 2026-04-29 | `8806e62` | docs(roadmap): add #330 — resume mode stats/cost always zero |
| 2026-05-24 | `f1a55a2` | fix: /resume latest searches all workspaces |
| 2026-05-11 | `8f55870` | docs(roadmap): add #448 — sandbox JSON has contradictory enabled/supported/active flags |
| 2026-05-11 | `9e1eafd` | docs(roadmap): add #444 — no broad-cwd guard for --resume; ROOT/HOME silently writable |
| 2026-05-11 | `d3a982d` | docs(roadmap): add #437 — version JSON missing is_dirty/branch/commit_date/rustc; git_sha truncated |
| 2026-05-11 | `7204844` | docs(roadmap): add #427 — subcommand --help requires auth/config; resume hits auth gate |
| 2026-05-11 | `6aa4b85` | docs(roadmap): add #421 — JSON cwd leaks /private symlink canonicalization on macOS |

## `[PICK-acp]` - ACP transport / SDK surface (5)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `78c2a49` | docs: close ROADMAP 443 acp serve evidence |
| 2026-06-05 | `0e54ec4` | fix: exit non-zero for acp serve and remove internal tracking IDs |
| 2026-05-26 | `7d6b204` | fix(#713): add missing action fields to acp and config json responses; acp->status, config bare->list, config section->show |
| 2026-05-11 | `b204885` | docs(roadmap): add #443 — acp serve exits 0 with status:discoverability_only; #413 still unfixed |
| 2026-05-11 | `075c214` | docs(roadmap): add #429 — no global --cwd flag; misleading 'Did you mean --acp' hint |

## `[PICK-permissions]` - permission / sandbox / safety (9)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `aca6584` | fix: normalize permission rule tool names to lowercase (#94) |
| 2026-06-05 | `6fcd0c5` | fix: clarify sandbox requested vs active state in JSON output |
| 2026-06-04 | `94579ea` | fix: default to workspace-write permissions |
| 2026-05-31 | `e8c8ef1` | Harden permission enforcement against sandbox bypasses |
| 2026-05-26 | `29dcd47` | fix(#731): sandbox JSON status:error→warn when filesystem sandbox active but namespace unsupported (macOS degraded state) |
| 2026-05-25 | `f2a9022` | fix: doctor boot preflight detail shows Some(false) for trust_gate_allowed; use Display instead of Debug |
| 2026-05-25 | `ba941f7` | docs(roadmap): add #695 — agent stale-worktree startup burn + sandbox .git writability opacity |
| 2026-05-11 | `8cf628a` | docs(roadmap): add #436 — init template sets permissions.defaultMode:dontAsk + empty .claw/ |
| 2026-05-11 | `ec882f4` | docs(roadmap): add #428 — default permission_mode is danger-full-access |

## `[PICK-session]` - session / resume / compact / config (64)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-08 | `05f0201` | fix: preserve runtime config validation compatibility |
| 2026-06-08 | `27acfe1` | test(runtime): isolate session and git metadata checks |
| 2026-06-06 | `db9ff49` | docs: add interactive session example to quick start |
| 2026-06-05 | `2f0b5b3` | fix: wrap concurrent ENOENT as domain-specific session error (#112) |
| 2026-06-05 | `7c4bcd9` | fix: expand ${VAR} and ~/ in MCP config fields (#92) |
| 2026-06-05 | `f8822aa` | fix: config merge concatenates arrays instead of replacing (#106) |
| 2026-06-05 | `eb86f4d` | fix: reject --compact for non-prompt subcommands (#98) |
| 2026-06-05 | `41b3566` | fix: update resume help test for message field parity (#338) |
| 2026-06-05 | `1b22ed7` | fix: standardize list command count fields and resume help field name |
| 2026-06-05 | `9ef21e2` | fix: expose merged key-value pairs in config JSON |
| 2026-06-05 | `4d4d72c` | fix: add prefix-aware matching to config key suggestion |
| 2026-06-05 | `a671969` | fix: handle --help after --resume flag |
| 2026-06-05 | `6757ebd` | fix: align discovered_config_files count with config check |
| 2026-06-05 | `db56498` | fix: route session list through credentials-free path |
| 2026-06-05 | `9f9b14a` | fix: add broad-cwd guard to resume path |
| 2026-06-04 | `453d894` | fix: validate hook config entries partially |
| 2026-06-04 | `4619375` | fix: load partial MCP configs |
| 2026-06-04 | `5b22bc0` | fix: load Claw and Agents memory files |
| 2026-06-04 | `7dd17c6` | fix: scaffold safe init settings |
| 2026-06-04 | `d8535bf` | fix: keep failed resume side-effect free |
| 2026-06-04 | `7cfd83f` | test: align compact CI contract |
| 2026-06-03 | `41034bb` | fix: address CI test failure and add empty-session error message |
| 2026-06-04 | `2ab2f44` | fix: keep session help local |
| 2026-06-03 | `94be902` | fix: attribute config precedence in JSON |
| 2026-06-03 | `36218ac` | fix: report config file load statuses |
| 2026-06-03 | `6388a2b` | fix: parse object-style hook config |
| 2026-06-02 | `e459a72` | fix: session resume — skip current empty session, unify cross-workspace loading |
| 2026-06-02 | `04bc5f5` | feat: API timeout config, Retry-After header, configurable retry, and 400 transient retry |
| 2026-04-27 | `d8c57ed` | feat: API timeout config, Retry-After header support, and configurable retry |
| 2026-05-29 | `de7edd5` | fix: suppress config deprecation stderr in JSON mode globally (#824) |
| 2026-05-29 | `f0e6671` | docs: record #824 - global settings-load deprecation leaks to stderr in JSON mode |
| 2026-05-29 | `efe59c2` | docs: record export session-not-found JSON stderr routing gap (#819) |
| 2026-05-28 | `9494e3c` | Suppress config warnings on JSON local surfaces (#3192) |
| 2026-05-28 | `89e7f41` | Avoid duplicate config warnings for JSON consumers (#3190) |
| 2026-05-28 | `c3e7b6a` | docs: record config json warning duplication (#3189) |
| 2026-05-27 | `d9844cf` | fix(#780): classifier arm ordering bug — legacy_session_no_workspace_binding and no_managed_sessions shadowed by generic session_load_failed arm |
| 2026-05-27 | `727a1ea` | fix(#773): config --output-format json now surfaces deprecation warnings in warnings[] array instead of only stderr text |
| 2026-05-27 | `89735db` | fix(#766): claw diff extra args now classified as unexpected_extra_args with hint; track #767 session subcommand gap |
| 2026-05-27 | `c86dc73` | fix(#763): config JSON parse errors now classify as config_parse_error |
| 2026-05-26 | `b8b3af6` | fix(#758): --cwd, --date, --session missing-value errors now use missing_flag_value prefix + hint |
| 2026-05-26 | `92e053a` | test(#744): regression guard for #741 config unsupported-section hint contract |
| 2026-05-26 | `d8a6109` | docs(#721/#722): re-add ROADMAP entry for config section expansion after rebase conflict |
| 2026-05-26 | `7037d84` | fix(#714): add action:help to top-level help json, render_export_help_json, render_help_topic_json, and resume repl help json |
| 2026-05-25 | `a30624d` | Expose creation time in session list metadata |
| 2026-05-25 | `1b5a9b0` | test: cover config warning dedup for inventory commands |
| 2026-05-25 | `9e6f753` | Fail closed for compact without an interactive session |
| 2026-05-25 | `c08395c` | docs(roadmap): add #700 help JSON missing status + session_list kind inconsistency |
| 2026-05-25 | `b64df99` | fix(#698): dedup config deprecation warnings per process; add tempfile dev-dep to runtime crate (fixes pre-existing test compile error) |
| 2026-05-25 | `da7924d` | docs(roadmap): add #696 — compact hangs in non-interactive mode with no TTY guard |
| 2026-05-25 | `3489ec5` | fix(#160): add regression test for SessionStore lifecycle (list_sessions, delete_session, session_exists) |
| 2026-05-25 | `0423321` | fix(test): update compact test to reflect flattened previous-context header |
| 2026-05-24 | `b43a6f2` | feat: auto-compact and retry on context window errors |
| 2026-05-24 | `5a9550d` | fix: flatten prior compaction highlights to prevent nesting compounding |
| 2026-05-12 | `a35ee9a` | docs(roadmap): add #449 — session list routes through ResumeSession and hits auth gate despite being a local-only filesystem read |
| 2026-05-15 | `21bbbb7` | Route resumed session commands exhaustively |
| 2026-05-15 | `4ccbd8f` | Keep resumed session handling exhaustive |
| 2026-05-15 | `c5a18e1` | Preserve resumed session command exhaustiveness |
| 2026-05-15 | `e199a39` | Start G010 session hygiene stream |
| 2026-05-15 | `9ae6aa3` | Keep plugin introspection available when MCP config is malformed |
| 2026-05-11 | `5ab969e` | docs(roadmap): add #446 — config loaded 2-3x per invocation; identical deprecation warnings spam |
| 2026-05-11 | `bd12690` | docs(roadmap): add #439 — ancestor CLAUDE.md walk causes silent context bleed |
| 2026-05-11 | `f4a9674` | docs(roadmap): add #438 — memory file discovery only finds CLAUDE.md, ignores AGENTS.md + CLAW.md |
| 2026-05-11 | `b8f989b` | docs(roadmap): add #435 — --resume failure: exit 0 text/1 json + creates partition dir |
| 2026-05-11 | `3730b45` | docs(roadmap): add #425 — config precedence undocumented; deprecation warning 4× |

## `[PICK-mcp]` - MCP server / plugin / hook lifecycle (20)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `c8e9735` | fix: redact MCP server sensitive fields in JSON (#90) |
| 2026-06-03 | `f529fb0` | fix: classify mcp show missing server argument |
| 2026-05-29 | `4d3dc5b` | docs: record #830 - mcp show missing server name emits unknown_mcp_action instead of missing_argument |
| 2026-05-28 | `0800d7a` | Route plugins list JSON parse errors to stdout (#3194) |
| 2026-05-28 | `69b8b36` | docs: record plugins trailing dash json routing (#3193) |
| 2026-05-28 | `85d63b0` | docs(roadmap): add #809 help mcp plugin json hangs (#3168) |
| 2026-05-27 | `87b7e74` | fix(#806): plugins show <not-found> in text mode returned empty success instead of error |
| 2026-05-27 | `1201dc6` | docs(roadmap): add deferred entries #798-#800 (plugins extra-arg, empty-prompt, classifier coverage) |
| 2026-05-27 | `491f179` | fix(#794): plugins install not-found path returns typed plugin_source_not_found instead of unknown+null |
| 2026-05-27 | `57a57ef` | fix(#793): plugins list --flag silent success + uninstall not-found hint:null |
| 2026-05-27 | `e4c3c1a` | fix(#789): agents show and plugins show not-found now exit 1; parity with skills (#788) and mcp (#68) |
| 2026-05-25 | `1003510` | docs(roadmap): add #697 — plugins remove silent ok on missing plugin; agents unknown subcommand exit 0 |
| 2026-05-25 | `96ddeca` | fix: resolve EACCES error from incorrect bundled plugins directory |
| 2026-05-15 | `2202410` | map MCP lifecycle maturity surfaces |
| 2026-05-15 | `7ed1cab` | Prove observable MCP required optional contracts |
| 2026-05-15 | `5de73ec` | Prevent plugin command aliases from becoming prompts |
| 2026-05-15 | `557ab8a` | surface required MCP server semantics |
| 2026-05-15 | `1f00771` | Keep plugin lifecycle JSON complete after team merges |
| 2026-05-15 | `db6f30f` | verify plugin lifecycle JSON contract |
| 2026-05-11 | `8499599` | docs(roadmap): add #441 — hooks schema diverges from Claude Code documented format |

## `[PICK-tui]` - TUI / rendering / streaming display (8)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-04 | `58a30f6` | fix: accept markdown agent definitions with YAML frontmatter |
| 2026-05-28 | `3260258` | fix: make cc2 renderer path errors concise (#3180) |
| 2026-05-26 | `9757fef` | fix(#727): add has_upstream bool to branch_freshness JSON to disambiguate fresh:null-no-upstream from fresh:null-unknown |
| 2026-05-26 | `8f8eb41` | fix(#709): remove duplicate status:ok keys from render_agents_report_json and render_skill_install_report_json; silent overwrite risk in serde_json json! macro |
| 2026-05-25 | `779cf1c` | test(api): fill thinking in stream chunk fixtures |
| 2026-05-24 | `7149bbc` | fix: streaming robustness — OpenAI parsing, error detection, reasoning content |
| 2026-05-15 | `4d78e91` | Start G011 ecosystem ops UX stream |
| 2026-05-11 | `328fd11` | docs(roadmap): add #430 — dump-manifests requires upstream TS source; export PATH dropped |

## `[PICK-cli]` - CLI flags / arg parsing / output format (21)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-08 | `a1da1ca` | test(cli): serialize env-sensitive model alias checks |
| 2026-06-05 | `7305e55` | fix: update skills help test assertions for --project flag (#95 CI fix) |
| 2026-06-05 | `b60cbeb` | feat: skills install --project flag for project-level scope (#95) |
| 2026-06-05 | `934bf28` | fix: validate --base-commit is a hex SHA (#122) |
| 2026-06-05 | `adf5bd1` | fix: validate --cwd and --date for system-prompt (#99) |
| 2026-06-05 | `2f8679b` | fix: track duplicate global flags in status JSON |
| 2026-06-05 | `b3a5a74` | fix: target multi-word guard for CLI subcommands only |
| 2026-06-04 | `b5bead9` | fix: recover CLI parser CI |
| 2026-06-04 | `41678eb` | fix: type output format selection |
| 2026-06-03 | `1bd18be` | feat: add GitShow output formats |
| 2026-05-28 | `b7ea046` | test: cover doctor help JSON flag order (#3185) |
| 2026-05-26 | `0e8a449` | fix+test(#755): -p consumes exactly one token; flags after prompt text now parse normally |
| 2026-05-26 | `c592313` | test(#737): add boot_preflight details non-null-value regression guard to output_format_contract |
| 2026-05-26 | `db80c9b` | fix(#733): diff JSON adds changed_file_count; run git diff --name-only for staged+unstaged and deduplicate into BTreeSet |
| 2026-05-26 | `8d80f2f` | test(#717): add contract tests for agents show not-found and agents list filter in output_format_contract |
| 2026-05-25 | `45dc4f6` | Stabilize JSON action contract for local CLI surfaces |
| 2026-05-26 | `f8a901c` | fix(#710): diff --output-format json adds missing action:diff and working_directory fields to both ok and error branches |
| 2026-05-25 | `a76dda2` | chore: cargo fmt --all on fix-683 branch |
| 2026-05-25 | `1f330c6` | chore: cargo fmt --all on fix-160 branch |
| 2026-05-11 | `0e5f695` | docs(roadmap): add #433 — repeated --output-format silent override + case-sensitive enum |
| 2026-05-11 | `ce39d5c` | docs(roadmap): add #432 — --allowedTools naming inconsistency + missing-value parser bug |

## `[PICK-doctor]` - diagnostics / doctor / status / cost (8)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-05 | `4d41ab3` | fix: expose openai_key_present in doctor auth check |
| 2026-06-05 | `5a76ecb` | fix: add prompt_ready to doctor auth check |
| 2026-05-28 | `73d8d6e` | Keep doctor help machine-discoverable locally (#3184) |
| 2026-05-26 | `cc86f54` | fix(#701): doctor JSON details[] now {key,value} objects; prose preserved as details_prose[]; acceptance check passes |
| 2026-05-25 | `8f809d9` | fix(#704): DiagnosticCheck.json_value now emits stable snake_case id field; doctor checks addressable without scraping name prose |
| 2026-05-25 | `f6cab27` | docs(roadmap): add #704 doctor checks label:null makes check identity unaddressable by machine parsers |
| 2026-05-16 | `f8e1bb7` | docs(roadmap): add #450 — prompt JSON error routed to stderr not stdout; doctor missing prompt_ready field |
| 2026-05-14 | `43b1828` | Lock doctor JSON boot preflight contract |

## `[REVIEW]` - needs manual triage (188)

| Date | SHA | Subject |
|---|---|---|
| 2026-06-08 | `eb21179` | fix: validate attached redirection paths |
| 2026-06-06 | `3acb677` | Update README.md |
| 2026-06-06 | `43eac8f` | Update README.md |
| 2026-06-06 | `c850509` | update readme |
| 2026-06-05 | `503d515` | style: cargo fmt after merged PRs (#3164, #3209, #3214, #3216) |
| 2026-06-05 | `c848eeb` | fix: status JSON reports all workspace panes not just the first (#326) |
| 2026-06-05 | `d4aad71` | fix: add actionable auth hint to 401/403 API errors (#28) |
| 2026-06-05 | `f0e10ff` | docs: mark ROADMAP #342 DONE (covered by #325) |
| 2026-06-05 | `2f3120e` | fix: add structured command list to top-level help JSON (#325) |
| 2026-06-05 | `13992ad` | docs: mark ROADMAP #129 DONE (verified code flow) |
| 2026-06-05 | `b94c49c` | fix: use existing path in system-prompt test (#99 fix regression) |
| 2026-06-05 | `5df485c` | docs: mark ROADMAP #120,121 DONE |
| 2026-06-05 | `8f9315b` | docs: mark ROADMAP #32,43 DONE with runtime evidence |
| 2026-06-05 | `12a091a` | docs: mark ROADMAP #111,115 DONE |
| 2026-06-05 | `9f0cf3b` | docs: mark ROADMAP #101,104 DONE |
| 2026-06-05 | `6e94c1a` | docs: mark ROADMAP #93,109 DONE |
| 2026-06-05 | `38626ca` | docs: mark ROADMAP #85,88,125,126 DONE |
| 2026-06-05 | `e84f7c8` | fix: report 'no_git_repo' instead of 'clean' when not in git (#125) |
| 2026-06-05 | `b8f0663` | fix: structured bootstrap-plan phases JSON (#412) |
| 2026-06-05 | `5adc751` | docs: mark ROADMAP #97 DONE |
| 2026-06-05 | `b220366` | docs: mark ROADMAP 81-83,87,91,102,117,128 DONE with direct evidence |
| 2026-06-05 | `b5d67ef` | docs: mark ROADMAP #105,118 DONE |
| 2026-06-05 | `9a40568` | docs: mark ROADMAP #80,86,100 DONE |
| 2026-06-05 | `b86159c` | docs: mark ROADMAP #124,127 DONE with direct evidence |
| 2026-06-05 | `5e5b996` | docs: mark 8 ROADMAP items DONE (78,84,103,107,108,110,116,119) |
| 2026-06-05 | `3fbfcc4` | docs: mark ROADMAP 327,328,408,6 DONE with fix evidence |
| 2026-06-05 | `c7c5c11` | fix: correct help source lists, add is_clean to status JSON |
| 2026-06-05 | `c0447e2` | docs: mark ROADMAP 338,410 DONE with fix evidence |
| 2026-06-05 | `76f9a13` | docs: mark 16 ROADMAP items as DONE with direct verification evidence |
| 2026-06-05 | `9c11325` | docs: mark additional pre-440 ROADMAP items as DONE |
| 2026-06-05 | `4708ab1` | fix: add structured help JSON and provider BASE_URL validation |
| 2026-06-05 | `311e719` | docs: mark ROADMAP 696-697 DONE |
| 2026-06-05 | `b926a9d` | docs: mark ROADMAP 713-716,723,734-735,737,767,771,774-776,781 as DONE |
| 2026-06-05 | `61d641d` | style: apply cargo fmt |
| 2026-06-05 | `6ac0386` | docs: close ROADMAP 335 evidence |
| 2026-06-05 | `4c939c0` | docs: close ROADMAP 465 evidence |
| 2026-06-05 | `662a50b` | docs: close ROADMAP 347,356,357 evidence |
| 2026-06-05 | `1da4aa4` | docs: close ROADMAP 413,416 evidence |
| 2026-06-05 | `3f50f33` | fix: filter boundary sentinel from system-prompt sections JSON |
| 2026-06-05 | `0ce4168` | docs: close ROADMAP 419-420 evidence |
| 2026-06-05 | `7f1dd0c` | docs: close ROADMAP 700-704 evidence |
| 2026-06-05 | `42f56e7` | docs: close ROADMAP 458-459 evidence |
| 2026-06-05 | `f25fae6` | docs: mark ROADMAP 726-806 as DONE |
| 2026-06-05 | `be66d96` | docs: close ROADMAP 705-722 evidence |
| 2026-06-05 | `b1a40a2` | docs: close ROADMAP 694,698 evidence |
| 2026-06-05 | `726d55d` | docs: close ROADMAP 682,693 evidence |
| 2026-06-05 | `ad76389` | docs: close ROADMAP 464,470 evidence |
| 2026-06-05 | `e68733d` | docs: close ROADMAP 462-463 evidence |
| 2026-06-05 | `9bc2f36` | fix: widen parse_subcommand guard for multi-word commands |
| 2026-06-05 | `f40927b` | docs: close ROADMAP 451-452 models already wired evidence |
| 2026-06-05 | `5bcbc2f` | docs: close ROADMAP 460 alias check evidence |
| 2026-06-05 | `2447273` | fix: add list to KNOWN_SUBCOMMANDS and close ROADMAP 454-455 |
| 2026-06-05 | `de66bfc` | fix: route broad_cwd JSON error to stdout and close ROADMAP 446-447 |
| 2026-06-04 | `346772a` | test: add exclude_id and 0-message filtering coverage; cargo fmt |
| 2026-06-04 | `10fe724` | fix: bound parent memory discovery |
| 2026-06-04 | `ae7da0e` | fix: expose complete version provenance |
| 2026-06-04 | `b45c61e` | fix: recover parser contract CI |
| 2026-06-04 | `ecd3e4c` | fix: type allowed tools validation |
| 2026-06-04 | `22fdaea` | fix: keep skills lifecycle local |
| 2026-06-03 | `7678337` | fix: address CI failures and reviewer feedback on #3214 |
| 2026-06-04 | `4522490` | fix: make dump-manifests self-contained |
| 2026-06-04 | `cd58c05` | fix: add global cwd override |
| 2026-06-04 | `fa35018` | fix: validate env model selection |
| 2026-06-03 | `9522674` | fix: read prompt subcommand input from stdin |
| 2026-06-03 | `c91a306` | fix: normalize Anthropic model routing |
| 2026-06-03 | `9c8375d` | feat: import project instruction rules |
| 2026-06-03 | `0cef539` | fix: resolve clippy pedantic warnings |
| 2026-06-03 | `ce116d9` | fix: expose binary provenance in local JSON |
| 2026-06-03 | `372ec09` | test: cover roadmap helper missing path |
| 2026-06-03 | `55da189` | fix: keep JSON control surfaces local |
| 2026-06-03 | `e752b05` | fix: load common instruction files and typed unknown commands |
| 2026-06-03 | `286638f` | docs: close ROADMAP 828 approval slash evidence |
| 2026-06-03 | `47d6c3d` | docs: close ROADMAP 829 interactive hint evidence |
| 2026-04-27 | `571d3cd` | fix: add "no parseable body" to CONTEXT_WINDOW_ERROR_MARKERS |
| 2026-04-27 | `414a1ac` | fix: retry 400 responses with transient gateway error bodies |
| 2026-05-29 | `d47b015` | fix: unknown single-word subcommand emits command_not_found (#825/#826) |
| 2026-05-29 | `5458d35` | docs: record #826 - multi-word unknown subcommand falls through to missing_credentials |
| 2026-05-29 | `70d64be` | fix: unknown single-word subcommand emits command_not_found instead of missing_credentials (#825) |
| 2026-05-29 | `3dbb35c` | docs: record prompt missing-text JSON stderr routing gap (#823) |
| 2026-05-29 | `3a76c4f` | docs: record unknown subcommand falls through to provider startup (#822) |
| 2026-05-28 | `ed3a616` | docs: record global json warning leak (#3191) |
| 2026-05-28 | `3af2d9f` | docs: verify trailing json inventory gap resolved (#3188) |
| 2026-05-28 | `09ff1ca` | docs: record trailing json inventory timeout (#3187) |
| 2026-05-28 | `a88d52f` | fix: make cc2 validator directory board error concise (#3179) |
| 2026-05-28 | `60f44d3` | fix: avoid cc2 generator dirs on missing source (#3178) |
| 2026-05-28 | `d4e9829` | fix: suppress partial cc2 wrapper validate pass output (#3177) |
| 2026-05-28 | `e17098c` | fix: resolve cc2 wrapper tools from script root (#3176) |
| 2026-05-28 | `e179361` | fix: make cc2 validator board read errors concise (#3175) |
| 2026-05-28 | `760e696` | fix: make cc2 generator missing source error concise (#3174) |
| 2026-05-28 | `193f111` | fix: reject extra roadmap helper paths (#3173) |
| 2026-05-28 | `f11ac23` | fix: add roadmap next-id help handling (#3172) |
| 2026-05-28 | `b0e94c9` | docs(roadmap): add #810 json stdout warning contamination (#3169) |
| 2026-05-28 | `db81598` | docs(roadmap): add #808 control-plane json hangs (#3166) |
| 2026-05-28 | `86f45a1` | docs(roadmap): add #807 model json hang (#3163) |
| 2026-05-27 | `1d516be` | fix: recover from llama.cpp context overflow and reqwest SSE decode failures |
| 2026-05-27 | `ae6a207` | fix(#3129): handle trailing json format for diff errors (#3161) |
| 2026-05-27 | `efd34c1` | fix(#805): skills show <not-found> in text mode silently returned empty success instead of error |
| 2026-05-27 | `23d7761` | docs(roadmap): add #786 installed binary provenance gap (#3126) |
| 2026-05-27 | `6ee67d6` | test: add unit test coverage for invalid_history_count and unknown_option classifier arms |
| 2026-05-27 | `87f4334` | fix(#785): add unknown_subcommand classifier arm for unknown subcommand: prose prefix |
| 2026-05-26 | `ef31328` | fix(#759): validate_model_syntax error strings now use newline separator so hint is non-null |
| 2026-05-26 | `2036f0b` | test(#742): add git-fixture test for diff changed_file_count dedup; fixes unreachable branch in #740 coverage |
| 2026-05-26 | `5d072d2` | test(#740): diff JSON contract test now asserts changed_file_count field behavior per #733 |
| 2026-05-26 | `4c16a42` | fix(#732): status JSON allowed_tools.entries:null→[] when unrestricted; callers can use .entries\|length without null guard |
| 2026-05-26 | `49d5b3f` | Prevent poisoned ROADMAP ids before allocation (#3116) |
| 2026-05-26 | `25ee5f3` | Prevent helper-era ROADMAP id collisions before review (#3115) |
| 2026-05-26 | `92539ca` | Prevent pre-push contract drift (#3113) |
| 2026-05-26 | `8280f66` | Warn before unwritable git metadata blocks worker commits (#3112) |
| 2026-05-25 | `920d5c6` | Catch stale Rust compile drift before push |
| 2026-05-25 | `789ea9a` | Reject drifted claw-analog bootstrap phases |
| 2026-05-26 | `401f6b1` | fix(#707): init test temp_dir combines AtomicU64 counter+nanos to prevent same-process parallel test collisions |
| 2026-05-25 | `f84799c` | fix: auto_compact runs before every iteration break, including terminal no-tool turns; closes #3106 |
| 2026-05-25 | `732007d` | fix(#705): add estimated_cost_usd_num (float) to usage JSON alongside string field; doc entry filed |
| 2026-05-25 | `4daefc7` | Stabilize allowedTools rejection contract in CI |
| 2026-05-25 | `566992c` | Unify inventory provenance for generic parsers |
| 2026-05-25 | `21a9860` | docs(roadmap): add #702 agents source vs skills origin field name inconsistency |
| 2026-05-25 | `9f14a7a` | docs(roadmap): add #700 help JSON prompt fallthrough |
| 2026-05-25 | `c613e8e` | feat: sweep |
| 2026-05-25 | `60108df` | fix(test): update client_integration version string 0.1.0 -> 0.1.3 |
| 2026-05-25 | `bd9102f` | fix(api): skip preflight for unknown model limits |
| 2026-05-25 | `e7d5d08` | fix: ChunkDelta thinking field in test initializers; fix parse_local_help_action ? operator |
| 2026-05-25 | `6f5465a` | fix(test): update client_integration version string 0.1.0 -> 0.1.3 |
| 2026-05-25 | `fdbc789` | fix(api): skip preflight for unknown model limits |
| 2026-05-25 | `06c126a` | fix(claw-analog): reject backslash paths in validate_rel_path (dotdot bypass on Linux) |
| 2026-05-25 | `03bd461` | fix: ChunkDelta thinking field in tests, remove residual retry_after refs, fix parse_local_help_action return type |
| 2026-05-25 | `bf7bae8` | docs(roadmap): add #694 — no pre-push cargo build gate lets broken main accumulate |
| 2026-05-25 | `499125c` | ci: fix rust.yml working-directory — set defaults.run.working-directory to rust/ |
| 2026-05-25 | `c32288b` | docs(roadmap): add #693 — claw-analog bootstrap phase parser silent unknown fallback |
| 2026-05-25 | `c8b4487` | fix: CVE-2021-29937 security vulnerability (#3056) |
| 2026-05-25 | `ae30bf4` | feat(analog): add claw-analog minimal harness |
| 2026-05-25 | `a4efdc4` | feat(rag): add claw-rag-service |
| 2026-05-25 | `52572d5` | docs: personal assistant roadmap |
| 2026-05-24 | `0975252` | feat: git-aware context tools |
| 2026-05-24 | `cef45ef` | feat: interactive provider wizard with fast model selection |
| 2026-05-25 | `bc1b3c8` | build: docker compose + dockerignore |
| 2026-05-25 | `88f79bb` | docs(roadmap): batch merge remaining open ROADMAP doc PRs (#2841-#2876) |
| 2026-05-24 | `aefa5b0` | feat(tools): add LoggingAspect to unified tool dispatch entry point |
| 2026-05-25 | `271283c` | chore: bump rustls-webpki to 0.103.13 |
| 2026-05-25 | `5fb2ed9` | docs: document TweetClaw skill install example |
| 2026-05-24 | `f967df7` | ci: add Rust CI workflow |
| 2026-05-25 | `fc26e16` | fix: resolve model aliases before syntax validation |
| 2026-05-25 | `1c62116` | feat: truncate oversized git diff in system prompt |
| 2026-05-24 | `739488f` | fix: return conservative token limits for unspecified models |
| 2026-05-24 | `a61d023` | fix: unify user_agent to 'clawd-rust-tools/0.1' |
| 2026-05-25 | `c881069` | docs(roadmap): batch merge #451-#470, #681-#691 roadmap entries |
| 2026-05-25 | `5200d1a` | docs(roadmap): add #692 — dump-manifests help json lacks source schema (#3094) |
| 2026-05-15 | `04c2abb` | Stabilize final gate before release checkpoint |
| 2026-05-15 | `33df16b` | Record PR gate evidence to avoid unsafe final merges |
| 2026-05-15 | `17260f6` | Preserve final-gate evidence for release arbitration |
| 2026-05-15 | `6f73103` | Record why issue reconciliation is evidence-gated |
| 2026-05-15 | `1ac8ce8` | Keep G011 artifacts reviewable |
| 2026-05-15 | `5a43d3b` | Keep G011 verification commands runnable |
| 2026-05-15 | `2c601ef` | Gate issue and PR triage on evidence |
| 2026-05-15 | `62bc7b6` | Stabilize G011 integrated evidence |
| 2026-05-15 | `8019999` | Record G010 final verification rerun |
| 2026-05-15 | `a3af013` | Preserve Windows checksum verification after docs merge |
| 2026-05-15 | `1bceda2` | Clarify Windows release onboarding |
| 2026-05-15 | `99efb21` | Ensure release docs are auditable before Windows adoption |
| 2026-05-15 | `7d859ae` | Close Windows release artifact verification gap |
| 2026-05-15 | `8c9a05e` | Restore provider compatibility diagnostics as API types |
| 2026-05-15 | `d5620c0` | Document provider compatibility diagnostics and passthrough |
| 2026-05-15 | `2cac66c` | Stabilize provider compatibility integration verification |
| 2026-05-14 | `7426ede` | map branch recovery verification evidence |
| 2026-05-14 | `d2b5f5d` | require provenance for green contracts |
| 2026-05-14 | `607f071` | harden branch recovery reporting |
| 2026-05-14 | `204af77` | Keep recovery recipe lint green for ledger reporting |
| 2026-05-14 | `4e0211d` | Expose boot preflight evidence in diagnostic JSON |
| 2026-05-14 | `8c11dd1` | task: preserve startup no-evidence timestamp evidence |
| 2026-05-14 | `675d9dd` | Harden workspace path classification |
| 2026-05-14 | `d87c3e6` | Make roadmap PR intake durable for CC2 |
| 2026-05-14 | `3a8ce83` | Deny scoped file reads before tool dispatch |
| 2026-05-14 | `f2dc615` | Prevent workspace escape through tool path resolution |
| 2026-05-14 | `180ebb3` | Reject Windows absolute PowerShell paths from workspace scope |
| 2026-05-14 | `9c2ebb4` | task: prefer tests before fixes |
| 2026-05-14 | `b1d8a66` | Gate CC2 completion on PR and issue resolution |
| 2026-05-14 | `d15268e` | Create a canonical CC2 board so every frozen ROADMAP heading is verifiably mapped |
| 2026-05-14 | `07dad88` | Classify issue and parity intake for CC2 board integration |
| 2026-05-11 | `19aaf9d` | docs(roadmap): add #442 — agents require TOML format, .md files silently dropped |
| 2026-05-11 | `86ff83c` | docs(roadmap): add #440 — one invalid mcpServers entry blocks ALL valid servers |
| 2026-05-11 | `e29010e` | docs(roadmap): add #434 — POSIX -- separator not recognized; shorthand prompts can't start with dash |
| 2026-05-11 | `fad53e2` | docs(roadmap): add #431 — skills uninstall requires creds; install error leaks OS string |
| 2026-05-11 | `1fecdf0` | docs(roadmap): add #426 — ANTHROPIC_MODEL env bypasses invalid_model validator |
| 2026-05-11 | `d7dbe95` | docs(roadmap): add #424 — bare canonical model names rejected; stale 4-6 suggestion |
| 2026-05-11 | `6c0c305` | docs(roadmap): add #423 — claw prompt ignores stdin; kind:unknown for missing arg |
| 2026-05-11 | `3c563fa` | docs(roadmap): add #422 — unknown subcommand silently sent as chat prompt |
| 2026-05-10 | `fa8eeca` | fix |
| 2026-05-10 | `2033c90` | fix log |
| 2026-05-09 | `b98b9a7` | fix(fmt): expand Thinking struct literals to pass cargo fmt |
