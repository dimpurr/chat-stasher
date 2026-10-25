# SG-4 设计节草稿（每小时任务省时）

> **Status: DRAFT** —— W1027 起草，供正手审；本稿**不进 46 正文**，裁定后由正手收编。
> 输入：W1026-OUT.md 前置只读测量（2026-10-25 00:22–00:33，本机）＋代码基线 `1cae3d9d`（下文行号以此为准，函数名先行）；TODO L426 = 台账行。本稿未做任何新测、未跑 push / collect / activity-index / reclaim。
> 引用只含 paths / counts / field 名（无会话正文、无凭据、不指私路径）。
> 数据诚实性：标注「实测」的数字出自 W1026 那一趟 pass 或代码；标注「估算」的数字是 W1026 的推算（假设在条目内列明），**不冒充实测**。

## 0 · 一句话

SG-4 剩下的实事是三件＋一条依赖：把 stage check 里最后一遍重复的 token 复核交出去（≈1 s 量级）；把已 merge 的增量化代码**部署**进定时任务（不部署则观察收益为 0）；对 21,065 个没有 `shard-facts` 的 legacy 分片二选一处置（等 SG-2 真回收／一次性回填摘要）——且 <2 min 的整轮闸**显式依赖 SG-3/ADR-061 把 collect 降下来**，SG-4 单独做不到。

## 1 · 目标复述与口径修正

**目标（46 §3 目标 5 / §4 判据 5 / §6 SG-4 行）**：每小时任务中位耗时 < 2 分钟（m1max），且与暂存区大小基本无关。46 §5.4 当时定的活动索引口径——「先不动，第 1、2 层之后重新量；仍 > 30 s 再立项」——**重新量已由 W1026 完成：221 s ≫ 30 s，立项条件成立**，但量得的现实是：SG-2/SG-3 已 merge 而未部署/未过上线闸，活动索引仍是每小时全量重建。

**两条口径修正（先修再做设计，否则照错靶子发力）**：

1. 46 §2「每小时任务耗时」行里的「推送前扫 22 GB 暂存区 ~90 s」归段错了：它落在 **backup 段**（rustic 走盘＋上传；W1026 实测 39.2 s，其中走盘 ~11 s，其余是开仓/连接固定开销，与暂存区大小基本无关——SG0 §3-E3 同向）。stage check 的真值是：run-once 复用路径 **0.17 s**、独立 `push` 自己数一遍 **≈1.2–1.9 s**。SG-4 不应该对「扫 22 GB」做任何设计。
2. 活动索引基线取 W1026 实测这一趟：**221,219 ms**（前一次 SG0 复核 214 s；46 §2 记 255 s——同一量级，本文一律以 221 s 为基线）。

## 2 · 现状数字（W1026 实测，基线趟）

最近一次定时 pass（run-state `duration_ms=733346`，2026-10-24 23:19:14 结束）分段：

| 段 | 值 | 占 pass | SG-4 可动？ |
|---|---|---|---|
| collect | 469,749 ms（opencode 378,069 ms） | 64% | ✗ SG-3/ADR-061 的靶子（merged `db8a1d55`，未部署） |
| activity_index | 221,219 ms | 30% | ✓ 本节靶子 |
| backup | 39,160 ms | 5% | △ 固定开销为主，SG-4 不动 |
| scan＋stage_audit＋metadata_hash＋push_preflight＋run_state_write | 972＋1,939＋16＋170＋5 ms | 0.4% | ✓ 但只有 ~1 s 可挤 |
| **总计** | **733,346 ms ≈ 12.2 min** | | |

题外一句（口径完整性）：部署的定时二进制是 **0.5.0（mtime 2026-10-13 15:31）**，故这趟活动索引走的是全量重建路径；`run-state` 的新字段（`activity_index_sessions_carried`、sg3 组）不在其中——这正是 §3-W2 的证据。

## 3 · 设计：三件工作

### W1 · stage check 的 token 交接（把已做完的增量收尾）

46 §5.4 的「改成只核对上次推送之后的新分片」已被 W880/W881 做掉了大部分：run-once 的 push preflight 不再重扫 registry、不再自数 stage——`inspect_stage_for_push_with_evidence`（`collect.rs` ≈1502）直接吃本趟 collect 的 scan 证据与 `CountedStageShards`。**剩余的重复只有一处**：preflight 用 `fresh_count` → `StageFreshnessToken::still_matches`（`store.rs` ≈1583）重新枚举＋stat 一遍 sessions 树（12,770 个目录 stat；生产实测 170 ms，探针中位 65 ms，n=3），而审计阶段 `CountedStageShards::count`（`main.rs` ≈9134 → `sealed_shard_count_with_token`，`store.rs` ≈1665）刚为同一棵树数过一遍并拿到了绑定该树形状的 token（25,759 目录遍历＋21,068 文件 stat，生产 1,939 ms）。

设计：**run-once 同进程内，把审计计数连同其 `StageFreshnessToken` 直接交给 preflight 消费，中途不再独立重 stat**。依据：本趟审计与 push 之间没有代码往 sessions/ 写（活动索引只写 `meta/`），外部写者由「单写者」前提排除；该前提若不采信，退让方案是只复核 sessions_root 的 mtime 单点，不做全树 stat（取舍见开放点 5）。standalone `push` 的形状**保持不变**：自己数、自己复核、token 不匹配就 fail-closed 重数（`fresh_count` 的 None 分支保留）。

收益（W1026 估，标估算）：**≈0.9–1.0 s/pass**；其中保底可保证的只是 preflight 的重复 stat（生产 170 ms/pass），更大份额取决于 stage_audit 的全量计数是否也按 46 §5.4 原文收紧成「只数新分片」（开放点 2）。诚实标尺：整个 stage 段合计现 ≈2.1 s，占 pass 0.4%——W1 是「把已经增量的段收干净」，不是省时大盘。

### W2 · 部署前置（不部署则 SG-4 观察到的收益为 0）

事实（W1026 旁证链）：SYNC-D 增量索引（`c3a95cdd`，2026-10-23）与 SG-3/ADR-061（`db8a1d55`，2026-10-23）都已在 main（本稿基线含两者），但定时任务用的 `~/.local/bin/chat-stasher` 是 **0.5.0（mtime 2026-10-13 15:31）**，内含 W880（`ba62d924`）/W881（`8e60b866`）、**不含** SYNC-D 与 SG-3。机电证据：定时 pass 的 run-state phases 里没有 `activity_index_sessions_carried`/sg3 字段；stage 里 `shard-facts` 文件数 = 0。

设计上必须写成**显式前置步骤**：定时任务换上 ≥ `c3a95cdd`＋`db8a1d55` 的二进制（或其后任一含两者的版本），且**部署是否生效用 run-state 判定，不靠人眼**——连续的定时 pass 出现新字段集即生效（详见 §4 验收口径第一行）。在部署之前做的任何 SG-4 验收测量都是全量重建的数字，没有意义。安装/替换二进制的动作本身超出本稿（docs-only），归属见开放点 6。

### W3 · legacy 正文死角处置（二选一，待正手裁）

**死角的事实（W1026 最重要发现）**：`shard-facts` 只在 seal 时由 `shard_writer` 就近写；`carryable_state_entry`（`main.rs` ≈3322）在会话缺 facts 时直接返回 None——该会话每趟走 `derived_row`（`main.rs` ≈3381）整份重读，代码自陈「re-derived every pass until they are re-sealed or restored」（`main.rs` ≈9269）。现存 legacy 正文分片 **21,065 个 / 25.98 GiB**，`shard-facts` 一个都没有（= 0）：**增量索引部署后仍然每趟整份重读这 25.98 GiB，收益为 0**。分摊（正文 bytes）：opencode 16.51 GiB / 8,244 片 · claude-code 5.70 GiB / 7,410 · codex 3.06 / 1,557 · chatgpt 0.31 / 1,611 · grok 0.15 / 886 · hermes 0.12 / 475；有正文会话 8,296（索引 12,765 行 − 4,469 空正文）。

**路线 (i) · 等 SG-2 的真实回收把 legacy 正文清出 stage。** 依据：ADR-060 B3 生效后 R2 属 never-pushed、不参与证明，可只认 storagebox；回收窗口每周日 03:17（W1026 量毕后 ~2.5 h 即下一个窗口；该窗口是否真跑成，W1026 未验证——**不假设**）。风险：`reclaim-stage.log` 自 10-11 起无新行、10-18 那次是 fork 失败/退出码 128（SG0 §3-E2 的静默失败）——**不能假设回收会自己跑成**；真实回收本身还须过 ADR-060 D7 的闸（dry-run 报正手 → 正手看过 → apply → 50 条读回逐字节）；若届时镜像已打开，另有 ADR-062/47 §5.3 的镜像闭包前置。选 (i) ⇒ SG-4 的索引验收在时间上绑在「真回收清完」之后，期间索引保持 ≈221 s。

**路线 (ii) · 一次性回填 `shard-facts`。** 对存量 21,065 片读一遍、按 `shard_facts` 既有格式写一遍摘要文件；成本 ≈86 s 单线程哈希（25.98 GiB ÷ 本机实测 306–315 MiB/s 整文件 sha256 折算，标估算）＋ 21,065 个小文件写入；**需另立实现单**（回填器不属于本设计节，也不属 docs-only）。机制上一致：回填算出的 sha256/bytes 与增量 state 里逐片记下的记录对得上，先全量跑一趟建立增量 state（≈221 s 一次性），其后各趟 `carryable_state_entry` 的逐片校验即可通过、`activity_index_sessions_carried` 开始兑现。缺点：若 SG-2 真回收紧随其后清掉这些正文，回填即白做。

**两案的判据数据**：等待期间每小时多付 ≈212 s（221 − 稳态估算 ≈9 s，标估算）的实耗；(ii) 的固定成本 ≈86 s 一次性＋实现单工时；(i) 的可行性取决于回收静默失败是否已在修（超出本稿验证范围）。

**（iii）· 接受收益推迟**：W1026 曾列为第三个选项，正手指令已收窄为二选一，本稿不再单列，仅存目。

**稳态收益的估算前提（承 W1026，全部标估算非实测）**：(a) W2 部署完成；(b) legacy 处置完成（回收或回填）；(c) 上一趟新增分片代表稳态——若那两个长 opencode 会话在 ADR-061 未生效时继续每小时整份重封，它们是每趟新增字节 ≈39 MiB 的大头，1/660 的字节比只对新（追加型）会话成立；(d) 冷缓存按 310 MiB/s 折算。在此前提下：每趟索引读 ≈40.1 MiB（2 个 opencode 新快照会话 20.74＋18.32 MiB＋17 个追加小片 ≈1 MiB），对全量 25.98 GiB = **1/660**，索引时间 ≈7–9 s（乐观 2–4 s，若 hash-only 携带）。折算依据：全量 221 s 里实测 ≈86 s 是读＋哈希、余下 ≈135 s（约 61%）随字节走；40.1 MiB 的解析折算 ≈5 s ＋ 遍历/携带 ≈2–4 s。

## 4 · 验收口径（全部绑 run-state 已有字段，不新造字段、不只绑总时长）

**前置成立（W2 的机电判定）**：定时 pass 的 run-state phases 出现新字段集——`activity_index_sessions_carried`、`shard_bytes_read_hashed`、摘要行的 `sg3` 组（`full_exports / incr_passes / incr_records / incr_bytes / growth_bytes`；结构体字段对应 `sqlite_full_exports` / `sqlite_increment_passes` / `sqlite_increment_records` / `sqlite_increment_bytes` / `sqlite_growth_bytes`）。字段出现即「部署的定时二进制 ≥ 2026-10-23 两个 merge」的证明；`activity_index_sessions_carried = 0` 在 legacy 未处置期间是诚实预期（只有部署后新 seal 的会话才可能携带——seal 时自带 facts 的会话从第二趟起可携带），不判失败（runstate 的字段文档同款语义：Zero is the honest value）。

**W1 的验收**：run-once 中位上 `push_preflight_ms` 从 170 ms 降到读 `push-state.json`（156 B）的量级——交接后本趟 pass 不再独立 stat sessions 树，这就是机电证据；若正手裁入审计收紧（开放点 2），另加 `stage_audit_ms` 中位降到目标值（数值待裁后定）。等价不倒退：`tests/syncd_incremental_equivalence_test.rs` 维持增量索引与全量重建字节等价；standalone `push` 的 fail-closed 复数与 ADR-022 的空 stage 防线语义不变。

**W3 处置后的索引验收**：`activity_index_ms` 中位 ≤ 30 s（46 §5.4 当定的立项线；稳态估算 ≈7–9 s，闸值挡估算区间还是收紧留余量由正手裁——开放点 3）；伴随 `activity_index_sessions_carried` ≥（有正文会话数 − 尚未处置的遗留数）——口径：carried＋derived = 该趟参与构建的有正文会话。`shard_bytes_read_hashed` 作观察值（混含 collect 的前缀重哈希份额）：处置+部署后的量级应从 ≈25.98 GiB/pass 的索引份额落到新增分片字节量级（≈40 MiB/pass）；索引份额不能从该字段单独拆出，故不作硬闸。

**整轮闸（46 §4.5，逐条写依赖）**：每小时任务中位 < 2 min 的算术成立条件（W1026 §5.6，抄录）——索引 221 → ≈7–9 s、stage 段再收 ~1 s、backup 保持 ≈39 s ⇒ 60–110 s 量级 ⇒ **还要求 collect 从 469.7 s 降进分钟级以内，而 collect 是 SG-3/ADR-061 的效果**（自身的闸是 ADR-061 D6 的写量规则，摘要行 `sg3: incr_bytes` / `growth_bytes` 可观测）。**collect 不降，则 <2 min 达不到**——46 §5.4 的判断成立，本稿把它写成 SG-4 验收的显式外部依赖，而不是隐含假设。对照口径保留：`activity-index --rebuild` 手动全量重建不作废，作为 ADR-020 校验对照与索引坏损的重建出口。

## 5 · 开放点（≤6 条，全部待正手裁，本稿不预设答案）

1. **W3 路线**：(i) 等 SG-2 真回收（含其 D7 闸与静默失败的修复时点）还是 (ii) 一次性回填＋另立实现单？若 (ii)：回填器做成 `activity-index` 的手动子命令还是独立单？是否要求回填单自带等价测试？
2. **W1 范围**：只做 token 交接（保底省 preflight 的重复 stat，生产口径 170 ms/pass），还是同时把 stage_audit 的全量计数按 46 §5.4 原文收紧成「只数新分片」（W1026 估两相合计 ≈0.9–1.0 s/pass）？
3. **`activity_index_ms` 的闸值**：写 46 §5.4 的 30 s（立项线当验收线），还是照稳态估算 7–9 s 收紧留余量（如 ≤10 s）？
4. **整轮 <2 min 闸的口径**：「中位」取多长的窗口（连续 24 个 hourly pass？）？是否把 TODO 的 SG-4 行拆成两条——自身闸（W1–W3 的字段验收）＋整轮闸（显式注 SG-3 依赖），避免 SG-4 单为整轮数背锅？
5. **W1 in-process 交接的边界**：采信单写者前提完全不复核，还是保留 sessions_root mtime 单点复核？（standalone push 不变这点不裁——是设计给定。）
6. **W2 部署归属**：换定时二进制的动作进 SG-4 实现单，还是单立运维单？谁执行、何时（若排在 W3(i) 之前，索引在回收前仍是 221 s——这是已知代价，不是事故）。
