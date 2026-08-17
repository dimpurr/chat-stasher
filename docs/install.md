# 安装引导

这份文档写给**要用它的人**，不是写给开发者的。你不需要读懂 Rust 或
TypeScript，但你需要能打开一个终端窗口、复制粘贴几条命令。

全文的写法约定：凡是关于「这个软件会怎么做」的说法，后面都跟一个
`文件:行号`，你可以自己去仓库里核对。凡是我们**没有亲手验证过**的，
一律写「未查证」——「未查证」不等于「不存在」，只是我们没测过。

---

## 1. 你要装的是两个东西

它不是一个 App，是**一套两件**，各干各的活：

| 部分 | 它负责什么 | 装在哪 |
| --- | --- | --- |
| **CLI（命令行程序 `chat-stasher`）** | 扫描你本机上各个 AI 编程工具留下的会话记录，收进一个只增不删的加密归档 | 你的电脑，终端里跑 |
| **浏览器扩展（Chat Stasher）** | 把你在**网页版**聊天里的对话，存成文件落到你的下载目录，等 CLI 来收 | 你的浏览器 |

**CLI 这一边**：它的自我描述是 "Append-only archive for every LLM conversation,
across harnesses."（`crates/chat-stasher/src/main.rs:22`）。它读的是本机上已经
存在的会话文件，读的时候是只读的（`crates/chat-stasher/src/main.rs:412-414`）。

**扩展这一边**：它当前支持四个网页平台 —— DeepSeek（`chat.deepseek.com`）、
ChatGPT（`chatgpt.com` / `chat.openai.com`）、Gemini（`gemini.google.com`）、
Claude（`claude.ai`）（`apps/extension/lib/contract.ts:69-70,112-113,128-129,144-145`）。
它申请的权限只有两个：`downloads` 和 `storage`
（`apps/extension/wxt.config.ts:22`）。它把抓到的会话写成 JSON 文件，路径是
下载目录下的 `chat-stasher/inbox/<名字>.json`
（`apps/extension/lib/contract.ts:224`、`apps/extension/lib/download.ts:91`）。

**两边怎么接起来**：扩展只管落盘，CLI 用 `ingest --inbox <你的收件目录>
--stage <你的暂存目录>` 把这些文件收走（`crates/chat-stasher/src/main.rs:397-410`）。

---

## 2. 装 CLI

仓库里没有预编译的安装包，也没有 `brew install` 之类的一键渠道 —— 你需要自己
从源码编译一次。

```sh
git clone <你的仓库地址> <你选的目录>
cd <你选的目录>
cargo build --release
```

- 需要 Rust 工具链（`cargo`）。**仓库没有声明最低 Rust 版本**：`Cargo.toml`
  与 `crates/chat-stasher/Cargo.toml` 里都没有 `rust-version` 字段
  （`Cargo.toml:1-11`、`crates/chat-stasher/Cargo.toml:1-6`）。具体哪个版本能
  编过 —— **未查证**。
- 编译产物在 `target/release/chat-stasher`。

然后写一份配置：

```sh
chat-stasher init
```

`init` 只在配置**不存在**时写入一份带注释的默认配置，它是非破坏性的
（`crates/chat-stasher/src/main.rs:34-35`）。配置文件的位置是
`~/.config/chat-stasher/config.toml`，如果你设了 `XDG_CONFIG_HOME` 就在那底下
（`crates/chat-stasher/src/config.rs:15,153-161`）。

---

## 3. 装浏览器扩展

**它还没有上架任何应用商店**（详见第 6 节）。现在只能手动装：

```sh
cd apps/extension
pnpm install
pnpm build            # Chrome/Edge 等 Chromium 系
pnpm build:firefox    # Firefox
```

（脚本名出自 `apps/extension/package.json:10-11`。需要 Node 和 pnpm；
**具体最低版本仓库没有声明，未查证**。）

构建产物落在 `apps/extension/.output/`（这个目录被 `.gitignore` 排除，
`.gitignore:15`）。之后**怎么把这个目录加载进浏览器** —— 各浏览器的
「加载已解压的扩展」菜单路径，见下一节；我们没有逐一实测，标了「未查证」。

---

## 4. 一次性设置清单

下面这几件事，**装的时候做一次就够了**。

### 4.1 🔴 关掉浏览器的「下载前询问每个文件的保存位置」

**这一条最重要，请不要跳过。**

**为什么要关**：扩展保存会话走的是浏览器的下载通道
（`apps/extension/lib/download.ts:117-121,130-134`）。而如果你开着「下载前
询问保存位置」，浏览器每存一个文件都可能弹一次系统「另存为」对话框。我们的
目标场景是在几天里归档**上千个会话** —— 那种情况下的弹窗数量是你不会想经历的。

**🔴 请照实理解这一条的证据强度**：

- **已实测**：Chrome 确实有这个设置项，它在配置文件里的键名是
  `download.prompt_for_download`。我们在本机的 Chrome `Preferences` 文件里
  直接读到了这个键，当时的值是 `true`（开启）。这是**一手证据**，但它只证明
  「这个设置存在」，不证明它会怎样影响扩展。
- **代码事实**：扩展调用下载时传的是 `saveAs: false`，也就是代码这边**要求**
  不要弹另存为框（`apps/extension/lib/download.ts:121`、`:134`）。
- **🔴 我们不知道的**：**`saveAs: false` 到底会不会被浏览器这个设置强行覆盖，
  我们没有自己实测过。** 外部报告和一条 Chromium issue 都指向「会被覆盖」，
  但那是**二手证据**，我们没有复现。

**所以这里给的是一条操作建议，不是行为保证**：请关掉这个设置，以**尽量避免**归档过程
被弹窗打断。我们不承诺关掉之后一定一个框都不会出现，也不承诺不关就一定会
弹 —— 这两句我们都还没有资格说。

**在哪里点**：

- **Chrome**：设置里的「下载内容」一节有「下载前询问每个文件的保存位置」开关，
  把它关掉。**具体菜单层级与文案 —— 未查证**（我们只核实了配置键
  `download.prompt_for_download` 的存在，没有实际点过 UI，浏览器版本一变
  文案就可能不同）。
- **Edge**：**未查证**。Edge 同为 Chromium 内核，设置项大概率存在且叫法相近，
  但我们没有在 Edge 上核实过任何菜单路径，所以这里不给路径。
- **Firefox**：**未查证**。我们没有在 Firefox 上核实过设置位置或它对扩展下载
  行为的影响。

（我们宁可让你自己在设置里搜一下「下载」两个字，也不想在这里编一个可能是错的
菜单路径给你。）

### 4.2 跑一次 `chat-stasher init`

见第 2 节。做过就不用再做。

### 4.3 决定归档存到哪，并**备份好你的主密钥文件**

归档的目的地由你的配置和命令行参数决定 —— 本地路径，或者你自己配置的后端。
`push` / `read` / `verify` 读的是你在配置或参数里选定的仓库和密钥文件
（`crates/chat-stasher/src/main.rs:97`、`:162`、`:208`）。

🔴 **主密钥文件是唯一的钥匙。丢了，归档就永远读不出来了，没有任何找回手段。**
源码里对此的原话是 "The masterkey is the repository's only key — losing it means
the repo is unreadable forever"（`crates/chat-stasher/src/store.rs:813-815`）。
密钥文件会以「仅属主可读」的权限写入，在能表达该权限的平台上
（`crates/chat-stasher/src/store.rs:824-825`）。

**请现在就把它复制一份到别的地方。** 这件事没人能替你做。

### 4.4 装一个定时器（可选，但这是「装完就不用管」的关键）

`chat-stasher schedule` 会**渲染**一份 launchd plist 或 systemd user
service/timer —— 注意它的原话是 "never installs it"，也就是它只生成文件，
**不替你安装**（`crates/chat-stasher/src/main.rs:74-75`）。生成的模板里包着
一条 `run-once` 命令（`crates/chat-stasher/src/main.rs:75-96`）。

`run-once` 是一次完整的采集+推送，跑完就退出，重复调用是安全的
（`crates/chat-stasher/src/main.rs:36-41`）。

---

## 5. 怎么确认它在工作

跑这一条：

```sh
chat-stasher status
```

`status` 是只读的。源码里对它的输出边界写的是：只有 id、路径、大小、mtime 和
标记会进标准输出，会话内容不会（`crates/chat-stasher/src/main.rs:2771-2772`）。
这是源码的自述，我们没有对每条输出路径做过穷举验证。

它的输出分两段。**第一行**是定时器体检结论，来自上一次 `run-once` 留下的记录
（`crates/chat-stasher/src/main.rs:2724`）。以下是源码里逐字定义的几种结论
（`crates/chat-stasher/src/runstate.rs:184-232`）：

- 还没装定时器 / 从来没跑成功过：
  `[run-once] 还没有任何运行记录：本机从未成功跑完一次 run-once（也可能状态目录被清空）。无法判断定时器是否在工作。`
- 一切正常（`{}` 处会填入真实数字）：
  `[run-once] 正常：上次运行在 N 分钟前，耗时 N ms，入库 N 个分片，已创建快照。`
  （没有新东西时结尾是「无变化故未创建快照」。）
- 定时器可能停了：
  `[run-once] 已经N 天没有运行了（阈值 N 小时）：定时器可能已经停了，上次结果是成功（无变化）。`
- 上次跑挂了：
  `[run-once] 上次运行失败：N 分钟前在 <步骤> 步骤出错，此后没有成功的运行。`

**第二段**是扫描结果。默认是固定几行的汇总，不会刷屏
（`crates/chat-stasher/src/main.rs:2792-2818`）：

- 有会话时：`[scan] N 个会话（N compressed）：<来源> N · <来源> N`
- 一个都没扫到时：`[scan] 本机没有扫描到任何会话。`
- 有来源目录不存在时会多一行：`[scan] 跳过 N 个不存在的来源根目录。`
- 有已识别但不会被归档的会话时：`⚠ N 个 harness 有已识别但 collect 不会归档的会话。`
- 最后固定一行：`明细（每个会话一行）：chat-stasher status --sessions`

想看每个会话一行的明细，就加 `--sessions`；那会是几百行
（`crates/chat-stasher/src/main.rs:154-158`）。

**🔴 一个容易踩的点**：`status` 在判定「不健康」时会**以非零码退出**
（`crates/chat-stasher/src/main.rs:2734-2738`）。所以「命令报错了」不一定是
命令坏了，很可能就是它在告诉你定时器停了。请读第一行的那句话。

还有一条相关的命令：`doctor`。它回答的是另一个问题 —— **有没有哪个工具正在
悄悄删你的历史**。它的报告只含路径、计数、字节数和时间戳
（`crates/chat-stasher/src/main.rs:200-202`）。

---

## 6. 🔴 现在还没有的东西

这一节是**诚实清单**。以下都是我们去代码里确认过的现状，不是暂时的免责声明。

- **没有 restore（整体恢复）命令。第一阶段不做。** 子命令表里没有 `restore`
  这一项（`crates/chat-stasher/src/main.rs:33-473`）。你能做的是 `read`，
  一次把**一个**会话打到标准输出（`crates/chat-stasher/src/main.rs:159-199`）。
  批量恢复 = 目前得你自己写脚本循环。

- **🔴 主密钥丢了，没有任何找回手段。** 没有找回流程、没有恢复码、没有客服。
  源码原话见 4.3 节（`crates/chat-stasher/src/store.rs:813-815`）。

- **回溯历史要跑好几天，不是几分钟。** 回溯这条腿对「取正文」的限速是
  **每天最多 200 条**，两次请求之间至少隔 20 秒
  （`apps/extension/lib/backfill/pace.ts:42`，注释见 `:15`）。按这个上限，
  一千个会话至少要 5 天。这是刻意的慢，不是 bug。

- **回溯功能默认是关的，而且目前没有开关界面。** 默认值是关
  （`apps/extension/lib/backfill/schedule.ts:32`），源码把打开的理由写得很清楚：
  回溯要拿你的登录态把整个账号翻一遍、往下载目录写成百上千个文件，所以必须先
  有一次明确的「开」。但打开它的接口**目前只是一个函数，没有做 UI**
  （`apps/extension/lib/backfill/schedule.ts:41`）。也就是说：**普通用户现在
  没有现成的按钮可以打开回溯。**

- **扩展还没上架，要手动装。** 仓库里没有任何商店上架物料或商店扩展 ID；
  `package.json` 里标着 `"private": true`（`apps/extension/package.json:4`），
  构建脚本产出的是本地目录和 zip（`apps/extension/package.json:10-13`）。
  安装方式见第 3 节。

- **抓下来的会话在被 `ingest` 收走之前，是明文躺在你的下载目录里的。**
  （`apps/extension/lib/download.ts:91`；`README.md` 的「Security and privacy」
  一节也这么说。）同一台机器上的其他程序能读到它们。

- **Zed 和 Cursor 的会话枚举没有实现**（见 `README.md` 的
  "What this does not do / current limits" 一节及其引用的
  `data/harness-registry-v1.json`）。

- **`schedule` 不会替你安装定时器**，只生成模板文件
  （`crates/chat-stasher/src/main.rs:74-75`）。真正的安装步骤要你自己做，
  **具体安装命令本文未给出 —— 未查证**（我们没有在本机走通一次完整的
  launchd/systemd 安装流程）。

---

## 7. 一次性的，还是要反复做的？

这是这个产品的核心承诺，所以说清楚：

**装的时候你要动几次手。之后就不用再管了。**

**只做一次**（第 4 节那些）：

- 关掉浏览器的「下载前询问保存位置」
- `chat-stasher init`
- 决定归档目的地
- 🔴 备份主密钥文件
- 装定时器

**之后自动跑**：定时器每到点跑一次 `run-once`，采集、推送、退出
（`crates/chat-stasher/src/main.rs:36-41`）。它不需要你确认任何东西。

**你偶尔该做的**（不是必须，但建议）：

- 隔一阵子跑一次 `chat-stasher status`，看第一行那句话。定时器坏掉的典型症状
  **不是报错，是沉默** —— `run-once` 在后台跑，没人看它的输出，所以它每次都
  留一条记录，就是为了让 `status` 能替你把这句话说出来
  （`crates/chat-stasher/src/runstate.rs:1-11`）。这也是为什么「从来没跑过」
  被判为**不健康**而不是「还行」：没有记录是**证据的缺席**，不是**健康的证据**
  （`crates/chat-stasher/src/runstate.rs:186-192`）。
- 偶尔跑一次 `doctor`，看有没有哪个工具开始删你的历史。

**这不是「零配置」。** 上面那五件事是真的要你做的，其中备份密钥那件事没人能
替你做。但它确实是**一次性**的 —— 做完就不用再想它。

---

## 8. 本文的「未查证」清单

集中列一遍，方便你知道哪些地方该自己再确认一次：

| 事项 | 状态 |
| --- | --- |
| `saveAs: false` 会不会被浏览器「下载前询问保存位置」覆盖 | **未查证**（我们没实测；外部报告与一条 Chromium issue 指向「会」，属**二手证据**） |
| Chrome 关闭该设置的**菜单路径与文案** | **未查证**（只核实了配置键 `download.prompt_for_download` 存在于本机 Chrome 的 `Preferences`，值为 `true`） |
| Edge 的该设置位置 | **未查证** |
| Firefox 的该设置位置及其对扩展下载的影响 | **未查证** |
| 各浏览器「加载已解压的扩展」的菜单路径 | **未查证** |
| 编译 CLI 所需的最低 Rust 版本 | **未查证**（仓库未声明 `rust-version`） |
| 构建扩展所需的最低 Node / pnpm 版本 | **未查证**（仓库未声明） |
| launchd / systemd 定时器的实际安装步骤 | **未查证**（`schedule` 只渲染模板，不安装） |

「未查证」= 我们没测过，不代表它不存在，也不代表它不工作。
上面第 6 节里那些**没有**的东西，是我们查过代码确认**确实不存在**的 —— 两者
请分开看。
