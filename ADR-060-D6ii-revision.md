# ADR-060 · D6 (ii) 原地修订（2026-10-23，随 W1021 落 ADR-062）

依据：正手 2026-10-23 `nm/ADR-062-REVIEW.md`（INBOX〔ADR062-REVIEW〕）——必改 C1 ＋ 小改末条：「14-ADR 里已落盘的 ADR-060 D6 (ii) 也同步改成『S 的闭包在镜像里齐全』，两份 ADR 的措辞要一致」，以及综述「ADR-060 已经在 03:2x 落盘，它 D6 的 (ii) 要在 14-ADR 里原地修订，并留一行修订记录」。同分支的 `ADR-062-section.md` 是对侧文字（其 D4/D6）；本文件的修订后文本与其逐字一致。

**本文件不直接落盘**——由副手照下述定位与替换文，在主仓 `.private/docs/14-ADR.md` 里原地执行，并插入修订记录行。修订前先核对「现行全文」与磁盘一致。

## 1 · 行号定位（14-ADR 现行）

- 文件：主仓 `.private/docs/14-ADR.md`（本地私有树，git 不跟踪；主仓 checkout `main` @ `c3a95cdd`，2026-10-23）。
- 节起始线：**第 3889 行** `## ADR-060 · Destination health and degraded reclaim: …`。
- D6 段＝**整整一物理行：第 3928 行**，以 `**D6 — mirror and Phase 5: reclaim proceeds; the condition cites 47 §5.3.**` 开头，以 `(见 Tickets)。` 结尾（夹在 D5 段之后、`**D7 — report and durable records…` 之前）。
- 本修订不改 D6 之前的任何文本；ADR-062 节段在文件**尾部**追加，不移动本行——落盘次序无论先后，3928 不漂移；若主仓在拉取分支前另有过改动，以「现行全文」整段匹配为准（下节），行号仅作辅助。

## 2 · 现行全文（替换前，须与磁盘逐字一致）

> **D6 — mirror and Phase 5: reclaim proceeds; the condition cites 47 §5.3.** A session may leave the stage when (i) D1/D3's proof holds, **and** (ii) if a mirror of a destination is configured and on, the mirror has already synced to a snapshot containing that session (47 §5.3's own words: no "stage deleted, mirror hasn't pulled it yet" window — any moment the machine can read the session offline). The mirror never counts as a destination and never as proof (47 §5.3: 本地镜像不算异地目的地，不参与远端证明). 46 §8.4 ("switch open ⇒ reclaim disabled") **stands withdrawn, decided** [正手 2026-10-22 裁定（草案开放点 2）] — the ADR encodes 47 §5.3 and does not carry the superseded rule; recorded so no implementation resurrects it, and 46 keeps its 🔁 note. The mirror-side machinery (sync, watermark, status) is ADR-062/LM-2's to define; this ADR owns only the precondition hook at the reclaim gate, config-gated so it is a no-op until a mirror exists (exercised for real at LM-5's offline acceptance). **Shipping gate (依赖注明): SG-2's implementation goes live only after LM-1 (ADR-062 draft + its §5.3 condition revision) has been reviewed — 47 §6's exact ordering rule. Implementation may proceed on the branch while LM-1 is in review; if LM-1's reviewed text differs from §5.3's wording, D6 is re-checked before ship.** [Dim-approved, 47 §7] The review's own ordering note stands: ADR-062's draft review is scheduled ahead of SG-2's ship window so SG-2 does not idle (见 Tickets).

## 3 · 修订后全文（逐字替换 3928 行那一整行）

> **D6 — mirror and Phase 5: reclaim proceeds; the condition cites 47 §5.3.** A session may leave the stage when (i) D1/D3's proof holds, **and** (ii) if a mirror of a destination is configured and on, **the closure of the snapshot S that the proof designated for that session on the mirrored destination is complete in the mirror and hash-verified** — S 的闭包 = 快照文件本身 ＋ 当时的全部 index 文件 ＋ S 的树引用到的全部 pack; the mirror leg is checked only against mirror-state 的「闭包齐全快照 id 集合」（只由一次成功完成的同步写入；版本未知 ⇒ 不可读 ⇒ 未覆盖 ⇒ 暂缓并报警），回收只读这个集合、不自己去扫镜像、也永不触发同步 (ADR-062 必改 C1, 与其 D4/D6 同措辞). No "stage deleted, mirror hasn't pulled it yet" window remains — any moment the machine can read the session offline (47 §5.3). The mirror never counts as a destination and never as proof (47 §5.3: 本地镜像不算异地目的地，不参与远端证明). 46 §8.4 ("switch open ⇒ reclaim disabled") **stands withdrawn, decided** [正手 2026-10-22 裁定（草案开放点 2）] — the ADR encodes 47 §5.3 and does not carry the superseded rule; recorded so no implementation resurrects it, and 46 keeps its 🔁 note. The mirror-side machinery (sync, the snapshot-closure state record, status) is ADR-062/LM-2's to define; this ADR owns only the precondition hook at the reclaim gate, config-gated so it is a no-op until a mirror exists (exercised for real at LM-5's offline acceptance). **Shipping gate (依赖注明): SG-2's implementation goes live only after LM-1 (ADR-062 draft + its §5.3 condition revision) has been reviewed — 47 §6's exact ordering rule. LM-1 completed 2026-10-23: the 正手 approved ADR-062 with 必改 C1–C4 (`ADR-062-REVIEW.md`), and this D6 (ii) has been revised in place to the same closure wording — the pre-ship divergence re-check has therefore run and closed; SG-2 may merge to trunk, its real reclaim still strictly behind ADR-062 D7's legs 3–4 (dry-run 先报正手、`--apply` 之后抽 50 条读回)。Implementation may proceed on the branch while LM-1 is in review; any further divergence from §5.3's wording re-opens D6 before ship.** [Dim-approved, 47 §7; 2026-10-23 按正手 ADR-062 评审修订闭合 — W1021] The review's own ordering note stands: ADR-062's draft review is scheduled ahead of SG-2's ship window so SG-2 does not idle (见 Tickets).

## 4 · 修订记录行（插在替换后的 D6 段之后、`**D7 — report and durable records…` 行之前，独立成行）

> 〔修订记录 2026-10-23（W1021）：D6 (ii) 按正手 ADR-062 评审必改 C1 原地修订——「the mirror has already synced to a snapshot containing that session」⇒「the closure of the snapshot S that the proof designated for that session on the mirrored destination is complete in the mirror and hash-verified」（S 的闭包＝快照文件＋当时全部 index＋S 的树引用到的全部 pack）；机器括号「sync, watermark, status」⇒「sync, the snapshot-closure state record, status」；上线闸补记 LM-1 完成（ADR-062 批准带 C1–C4，2026-10-23）、措辞复核闭合、SG-2 可合入主干（真实回收仍走其 D7 3、4）。与 ADR-062 的 D4/D6 逐字一致（`nm/ADR-062-REVIEW.md` 小改）。〕

## 5 · 改动说明（供副手核对）

| # | 位置 | 改动 | 依据 |
|---|---|---|---|
| 1 | (ii) 子句 | 「the mirror has already synced to a snapshot containing that session（47 §5.3's own words: no … window …）」→「**the closure of the snapshot S … is complete in the mirror and hash-verified**」＋闭包定义＋mirror-state 集合与写规则、不扫镜像、不触发同步；「no-window」句保留但不再冒充 (ii) 的括号注 | 必改 C1 ＋ 小改（措辞与 ADR-062 一致） |
| 2 | 机器括号 | 「(sync, watermark, status)」→「(sync, the snapshot-closure state record, status)」——watermark 已弃，避免读 D6 的人以为水位线仍是机制 | 必改 C1 |
| 3 | Shipping gate | 补记 LM-1 完成、复核已闭合（原文的前置条件已成事实）、SG-2 可合入主干、真实回收仍走 ADR-062 D7 3、4；末句改为「any further divergence … re-opens D6 before ship」（保留常设规则） | 正手综述（批准文首段）＋小改要求两措辞一致 |
| 4 | 新增修订记录行 | 见 §4，独立一行 | 正手要求「留一行修订记录」 |

**不改动（明示）：** D6 标题行、46 §8.4 withdrawn 句（含 [正手 2026-10-22 裁定（草案开放点 2）]）、no-op-until-a-mirror 句、末句 ordering note；ADR-060 节内其余段落（Context 第 6 条第 3900 行引 47 §5.3 原话，属历史引证，不改；Revisit trigger 1 第 3977 行保留常设规则，其 pre-ship 臂已由本次修订兑现，兑现记录在 D6 内）；D7/D8 及其后文本；ADR-061 节。ADR-060 的 Status 行不动——本修订属 2026-10-23 评审动作，记录在 D6 段与修订记录行内即可。
