# W1018 · ADR-060/061 落稿对照清单（review 必改/裁决 → 节段行号）

依据：`ADR-060-061-REVIEW.md`（正手 2026-10-22，INBOX〔ADR-REVIEW〕）。两份节段：

- `ADR-060-section.md`（1–101 行）
- `ADR-061-section.md`（1–107 行）

均随分支 `w1018-adr60-61-land` 提交，可直接追加进 14-ADR.md；体例同 ADR-059 的落盘节段（Status 行 + Context / Decision / Consequences / 🔴 Invalidates / Rejected / Revisit triggers / Tickets，开放点小节已消解并入正文）。行号以本分支提交的文件为准。

落盘提示（照 ADR-059 的做法）：14-ADR.md 里 ADR-059 的落盘形态是「底稿块（`#` 标题＋引证 blockquote，来自 adr-drafts 原文）」在上、落盘节段（`##` 标题）在下相接。两份草稿原文在 adr-drafts 有档，副手落 14-ADR 时可照此把底稿块接在节段上方；本清单只对应节段本身。

## ADR-060 · review 条目 → `ADR-060-section.md`

| Review 条目 | 落点（行号） | 落法 |
|---|---|---|
| **必改 A1** — `[destinations.<name>] degrade_after_days` 覆盖全局 `[reclaim] degrade_after_days`；自身节奏内 healthy 且仍参与证明；超时照样降级、照样报警；不是「排除保护」开关；补夹具：覆盖 35 天、20 天没推 ⇒ healthy、必须证明 | 19（D2 `healthy` 定义改「每目的地有效阈值」，含外接硬盘月推理由与「仍参与证明」）· 34（D5：覆盖键进 config 面）· 51（D8 夹具：`degrade_after_days = 35` / 20 天未推 ⇒ `healthy`、must-prove、被咨询可阻塞、不报警）· 64（Consequences：手动月推目的地活在自己节奏里，报警保持有意义）· 85（Rejected 末条：排除开关与 A1 覆盖的区别写死——覆盖从不卸掉任何目的地的保护）· 97（Tickets SG-2：分类器读入覆盖键） | 判据单位从全局改为每目的地有效阈值；标题 Header 同步写 per-destination（L1） |
| **建议 A2** — 降级报警的修复建议给两条路：修好推送（给命令），或不再需要就从 config 删掉；不许只给一条 | 20（D2 `lagging`：always names both roads）· 29（D3 报警三条：fix path = 两条路）· 46（D7 报告与三面都带两条路）· 50（D8 §4.7 夹具把两条路纳入共享措辞断言）· 97（Tickets SG-2 wiring） | 「never one road only」为硬要求，进 D2/D7/夹具 |
| 裁决 1 — 记录损坏 ⇒ 必须证明（fail-safe），同意草稿 | 22（D2 `unknown` ⇒ must-prove，状态由 [leaning] 转已裁定） | 开放点 1 关闭 |
| 裁决 2 — 按 47 §5.3，不写 46 §8.4 旧规则，同意草稿 | 40（D6：withdrawn **decided**，本 ADR 只留最终文本＋46 保 🔁 注记） | 开放点 2 关闭 |
| 裁决 3 — 键放 `[reclaim]`，同意草稿 | 34（D5 首句：`[reclaim]` + 两旋钮；A1 的覆盖键另在 `[destinations.<name>]`） | 开放点 3 关闭 |
| 裁决 4 — `--json` 用 `never-pushed`，人话「没有推送记录」，同意草稿 | 21（D2 `never-pushed`：slug 与 prose 分述，照草稿） | 开放点 4 关闭 |
| 裁决 5 — 上限只算分片正文字节（stat），不含 `meta/`，同意草稿 | 37（D5 size basis，由 [leaning] 转已裁定） | 开放点 5 关闭 |
| 裁决 6 — 名字照草稿（`degraded-not-consulted`、空 healthy 集措辞、`--json` 健康词表） | 43（D7：三样形状名照草稿定案；45 行空 healthy 集措辞照草稿） | 开放点 6 关闭 |
| 裁决 7 — `reclaim-state.json` 版本策略同 W1012g（未知版本 ⇒ 不可读 ⇒ 报警，绝不当 healthy），同意草稿 | 47（D7 健康快照 shape-versioned，W1012g 政策前向适用，已裁定） | 开放点 7 关闭 |
| （重申·顺序与闸）SG-2 上线等 LM-1；SG-3 不必等；ADR-062 草稿排 SG 实现前面；R2 错误配置保持；SG-5 由正手做；首次真回收 dry-run → 正手看 → `--apply` → 50 条读回逐字节 | 3（Status 的上线闸 = LM-1）· 40（D6 ship gate 与 ADR-062 排序提示）· 57（D8 实机闸：R2 错配保持、50 条抽样逐字节）· 98（Tickets：ADR-062 草稿先审）· 101（Tickets：SG-5 由正手做） | 闸与顺序原样保留 |

## ADR-061 · review 条目 → `ADR-061-section.md`

| Review 条目 | 落点（行号） | 落法 |
|---|---|---|
| **必改 B1** — 写入单位降到 part：message 记录不带 parts、只有 message 行本身；part 单独成记录，以 part id 为键做 id→hash；拼装时 part 按 `message_id, time_created, id` 挂回各自 message；orphan parts 自然同路；46 §5.1 A3 原则不变、键更细 | 1（标题：write unit drops to the part）· 8（Context 2 增补：大头在 parts、一回合一条 message 挂几十上百 parts）· 11（Context 5：原则在更细键上原样成立）· 15（D0：写入粒度 = record）· 17（D1 头：map 按 `message.id` / `part.id` 记）· 18（new message rows：不带 parts）· 19（new parts：各自成记录）· 20（changed：part 改写 part、行改写行；会话级 50–100× 变 part 级最坏情形）· 23（orphan parts = 普通记录，无第三条路）· 31（D3：parts 按 `message_id, time_created, id` 挂回）· 37–38（D4 去重/顺序按 record）· 53（D7 夹具：只改一个 part ⇒ 只重写该 part 记录；新 message 带 N parts ⇒ 1+N 条）· 85（Rejected：审前底稿的 message 粒度连 parts 形状记档） | D1/D3/D4/D7/Rejected/Bad-case 全链改 record 粒度 |
| **必改 B2** — 整份记录 = 一次完整观测：缺失的已知 message/part 视同该次观测被删（隐式墓碑）；增量时代发显式墓碑；默认视图 = 源头最新状态逐字节；被删内容绝不丢，`read`/`export` 开关带标记按原位输出；夹具：整份 v1 → revert 两条 → 再增长 ⇒ 默认视图逐字节等于源头最终导出、带开关两以删除标记出现 | 1（标题：implicit and explicit tombstones）· 30（D3：one whole-file record = one complete observation ⇒ 隐式墓碑；增量时代显式墓碑）· 33（D3 字节契约扩展到会删消息的源头：静止时默认视图 = `read_opencode_session` 最新导出）· 34（D3 末条：删除内容绝不丢、开关按原位带标记、卖点「工具删了自己的历史，我们还留着」）· 25（D2 推论：map 丢失退化路径的那次整份导出同样算完整观测，视图不丢删除）· 39（D4 补漏：只在完整观测里，缺失才算删除；map 缺 ≠ 删）· 57（D7 夹具：map 丢失 ⇒ 一次整份导出后恢复增量，视图仍正确）· 52（D7 **B2 夹具**：v1 → revert 两条 → 再增长 ⇒ 默认视图逐字节等于最终导出；开关开时两条按原位带删除标记）· 66（Consequences：删除史成为产品面）· 86（Rejected：审前底稿「无删除语义合并」记档并写明替换） | 审查指出的「只在源头不删时成立」缺口以隐式/显式墓碑双轨补齐 |
| 裁决 1 — 每条记录一行，外加一行小的会话观测记录；schema `chat-stasher.opencode.session.v2`；墓碑词 `deleted`（message/part 共用，带 kind） | 23（D1 record shapes：one record = one line、v2 schema；信封字段名待 SG 实现定）· 21（D1：tombstone word `deleted` + `kind`） | 开放点 1 关闭 |
| 裁决 2 — map 缓存：每条一个摘要的列表，超过 **5000 条**改用「整体摘要 + 从暂存区重建」 | 25（D2：per-record digest list（W986 同形）→ 5000 条阈值 → 整体摘要 + 暂存区重建） | 开放点 2 关闭（阈值 = 5000） |
| 裁决 3 — 只要求静止时逐字节相等；比较同一序列化器产出的字节，不另造规范格式 | 33（D3 字节契约：same serializer's bytes、quiescence-only）· 51（D7 首夹具：同一序列化器字节、静止相等） | 开放点 3 关闭 |
| 裁决 4 — 会话观测记录只在它本身字节变了时写（实质 = 有变化的轮次） | 22（D1：session-observation record 仅本行字节变化时写） | 开放点 4 关闭 |
| 裁决 5 — 每小时聚合，下限 `max(1.2×增长, 64KiB)`；按天闸门 = 24 个小时值总和对总写入 | 44（D6：hourly 聚合 + `max(1.2 × growth, 64 KiB)` 下限 + 日闸 = 24 小时值之和 vs 日总量）· 59（D7 实测闸书同规则）· 100（Revisit trigger 7 连动：下限再开） | 开放点 5 关闭（floor = 64 KiB） |
| 裁决 6 — orphan parts：是（B1 后自然成立） | 23（D1）· 31（D3：同规则同身份合并） | 开放点 6 关闭 |
| 裁决 7 — 源各自的 v2 戳共享一条装配代码路径 | 42（D5：one assembly code path） | 开放点 7 关闭 |
| 裁决 8 — A2 单独开票，不在 SG-3 首发关键路径上 | 42（D5：freestanding, off the critical path）· 107（Tickets follow-on：A2 单独票） | 开放点 8 关闭 |
| （重申·顺序与闸）SG-3 不依赖 SG-2/LM 可即刻开工；「真实副手会话跑一天」闸报写入/增长比、拼装逐字节、D6 首带实测区间 | 3（Status：开工与闸）· 59（D7 实测闸三联）· 60（D7 末条：与 SG-2/ADR-060、LM/ADR-062 无发版耦合） | 闸与顺序原样保留 |

## 「待 SG 实现定」清单（本节段故意未定的实现细节，均无编造）

- `ADR-060-section.md`：无（7 条裁决已覆盖草稿全部开放点；报警与空证明集的措辞照草稿定案）。
- `ADR-061-section.md`：
  - L23 — v2 记录的信封字段名（schema 串已定：`chat-stasher.opencode.session.v2`）；
  - L34 — `read`/`export` 删除内容开关的确切拼法（工作名 `--include-deleted`）与 UI 的展开形态。
