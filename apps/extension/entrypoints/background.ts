import {
  extractIdentity,
  extractSessionId,
  SCHEMA,
  pathSafeSessionId,
  findPlatformForUrl,
  getPlatformByOrigin,
  type CapturedFetch,
  type InboxBundle,
} from '../lib/contract';
import { writeCommitted } from '../lib/download';
import { recordCapture, refreshBadge } from '../lib/badge';
import { browserLocalStore } from '../lib/backfill/store';
import {
  BACKFILL_ENABLED_KEY,
  isBackfillEnabled,
  tickBackfill,
  tickBlockReason,
  type TickResult,
} from '../lib/backfill/schedule';
import type { BackfillOptions, HttpPort } from '../lib/backfill/engine';
import {
  BACKFILL_ALARM_NAME,
  loadTargets,
  rememberTarget,
  saveLastTick,
  syncBackfillAlarm,
  type AlarmsApi,
} from '../lib/backfill/alarm';
import {
  BACKFILL_PING_MESSAGE,
  isTabHello,
  pickLiveTab,
  rememberTab,
  tabHttpPort,
  type TabSend,
} from '../lib/backfill/tab-port';
import {
  POPUP_START_BACKFILL_MESSAGE,
  POPUP_STATUS_MESSAGE,
  type BackfillRuntimeStatus,
} from '../lib/popup-view';
import {
  guardBadge,
  isGuardTripped,
  loadGuardState,
  recordDownloadOutcome,
  resumeAfterGuard,
} from '../lib/download-guard';

export interface HandledResult {
  saved: boolean;
  reason?: string;
  finalName?: string;
  bytes?: number;
  /**
   * 🔴 C20：落盘时【实际用来命名】的那个身份。
   * 根因是「同一个身份被表达了两次」—— 欠账键来自列表接口的 items[].id，
   * 文件名来自「用正则从 URL 里再抠一次」，中间没有任何一致性校验。
   * 把这一位如实报出来，回溯腿才有可能当场对一次账（见 engine.ts 的 sinkVerdict）。
   * saved:false 时为 undefined（根本没有命名过）。
   */
  sessionId?: string;
}

/**
 * Build the inbox JSON document.
 * WHY raw-first: the parsed envelope is best-effort; if our field guesses are
 * wrong the CLI can re-derive structure from `raw.text` instead of losing data.
 */
function buildBundle(captured: CapturedFetch): InboxBundle {
  const parsed = { hasJson: false, keys: [] as string[] };
  try {
    const obj = JSON.parse(captured.text);
    if (obj && typeof obj === 'object') {
      parsed.hasJson = true;
      parsed.keys = Object.keys(obj);
    }
  } catch { /* not JSON */ }
  const sessionId = resolveSessionId(captured) ?? 'unknown';
  const platform = findPlatformForUrl(captured.url) ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  return {
    schema: SCHEMA,
    platform: platform?.id ?? 'deepseek',
    sessionId,
    // ADR-002: the dedupe axis is the ACCOUNT. `sessionId` guard keeps a
    // per-session id from ever being mistaken for the stable account id.
    identity: extractIdentity(captured.text, sessionId === 'unknown' ? null : sessionId),
    url: captured.url,
    method: captured.method,
    status: captured.status,
    capturedAt: new Date(captured.capturedAt).toISOString(),
    parsed,
    raw: {
      text: captured.text,
      bytes: new TextEncoder().encode(captured.text).length,
    },
  };
}

/**
 * 🔴 C21 · **这条链路上唯一一处决定「这条会话是谁」的地方。**
 *
 * 顺序不是偏好，是根因治理本身：
 *  1. `captured.sessionId` —— 上游【已经知道】身份时一路带下来的权威值
 *     （回溯腿：欠账键 = 列表接口的 items[].id，见 lib/backfill/engine.ts）。
 *     有它就【绝不再推导】—— 第二次表达就是在这里消失的。
 *  2. 没有它才回落到 extractSessionId（实时腿：身份只存在于 URL 里，别无来源）。
 *
 * 页面来的载荷永远走不到第 1 支：lib/contract.ts 的 isCapturedFetchShape
 * 对带 sessionId 的页面载荷【存在即拒收】。
 */
function resolveSessionId(captured: CapturedFetch): string | null {
  if (captured.sessionId !== undefined) return captured.sessionId;
  return extractSessionId(captured.url, captured.text, captured.pageUrl);
}

export async function handleCaptured(captured: CapturedFetch): Promise<HandledResult> {
  const sessionId = resolveSessionId(captured);
  if (cancelledIdLike(sessionId)) {
    // Per-session naming is the inbox contract; a session-less capture has no
    // stable file name and is dropped rather than polluting the inbox.
    return { saved: false, reason: 'no-session-id (skipped in report only)' };
  }

  const bundle = buildBundle(captured);
  // 🔴 C21 · 命名是【恒等映射】，不是「换掉不安全字符」：
  //    sanitizePathSegment 是多对一的（'a b' 与 'a/b' 同名），而下载是 overwrite ⇒
  //    两条不同的会话曾经能互相抹掉。起不出安全名字就当场收手、留痕，绝不硬塞。
  const named = pathSafeSessionId(bundle.sessionId);
  if (named === null) {
    return { saved: false, reason: 'session id is not usable as a file name (refused, not collapsed)' };
  }
  const slug = `${bundle.platform}-${named}`;
  const { finalName, bytes } = await writeCommitted(slug, JSON.stringify(bundle), {
    // C12：实时腿的每一次写入也喂给守卫（观测是共享的），
    // 但【熔断只暂停回溯腿】—— 这里不做任何阻断，用户当前这条对话照存。
    onOutcome: (result) => paintGuard(result),
  });
  // Badge is never allowed to take the save down: fire-and-forget, log-only.
  void recordCapture().catch((err) => {
    console.warn('[chat-stasher] badge update failed', (err as Error).message);
  });
  // 🔴 sessionId 报的是 bundle 里那个【真的用来拼文件名】的值，不是上面那个局部变量：
  // 中间隔着 buildBundle 的一次重新抽取，报错了那一位才有意义。
  return { saved: true, finalName, bytes, sessionId: bundle.sessionId };
}

/**
 * C12：记一次下载观测，并在熔断时把角标换成告警态。
 * 全程 best-effort —— 守卫是保护层，绝不允许它把落盘路径带下水。
 */
async function paintGuard(result: import('../lib/download-guard').DownloadResult): Promise<void> {
  try {
    const state = await recordDownloadOutcome(browserLocalStore(), result, { now: Date.now() });
    if (!state) return;
    const badge = guardBadge(state);
    if (!badge) return;
    const action = (browser as {
      action?: {
        setBadgeText: (o: { text: string }) => Promise<void>;
        setTitle?: (o: { title: string }) => Promise<void>;
      };
    }).action;
    if (!action) return;
    await action.setBadgeText({ text: badge.text });
    await action.setTitle?.({ title: badge.title });
  } catch (err) {
    console.warn('[chat-stasher] download guard update failed', (err as Error).message);
  }
}

/** 回溯腿的闸门：给 runBackfill 的 downloadGuard 用。 */
export async function isBackfillPausedByDownloadGuard(): Promise<boolean> {
  const store = browserLocalStore();
  if (!store) return false;
  return isGuardTripped(await loadGuardState(store));
}

/** 显式恢复入口（不做 UI，函数即接口）。欠账集合不动 ⇒ 从断点继续。 */
export async function resumeBackfillAfterDownloadGuard(): Promise<boolean> {
  const store = browserLocalStore();
  if (!store) return false;
  await resumeAfterGuard(store);
  const action = (browser as { action?: { setBadgeText: (o: { text: string }) => Promise<void> } }).action;
  await action?.setBadgeText({ text: '' });
  return true;
}

// ---------------------------------------------------------------------------
// C13 → C19 · 回溯腿的接线
//
// C13 的触发点是【实时腿的 onMessage】，理由是 MV3 里 SW 平时是死的、而实时腿
// 那条消息里现成带着 platform/origin/账号 scope。那些理由今天仍然成立，所以
// 那一脚【保留】。但它有一个致命推论：
// 🔴 一个装了之后再也不打开那个网站的用户，永远不会有第二条实时捕获来踢它，
//    历史就永远补不完。
// ⇒ C19 加了第二个心跳：chrome.alarms（见 lib/backfill/alarm.ts，周期与理由都在
//   那里）。两个心跳走的是同一个 tickBackfill，闸门顺序完全一致。
//   闹钟醒来时 SW 是全新的、什么都不知道 —— 所以实时腿踢那一脚时会顺手把目标
//   记进 storage（rememberTarget），闹钟直接读它，一个字都不用猜。
// ---------------------------------------------------------------------------

/**
 * 🔴 显式覆盖用的网络端口。默认 null。
 * C13~C18 期间这是【唯一】的注入口，而生产代码里没人调用它 ⇒ 回溯腿永远
 * 停在 'no-http-port'。C19 之后它退化成一个测试接缝：真正的生产端口由
 * resolveHttpPort() 现场从「一个活着的、已登录的平台标签页」上造出来。
 */
let backfillTransport: HttpPort | null = null;
let lastTick: TickResult | null = null;
let pendingTick: Promise<unknown> = Promise.resolve();
/** 测试接缝：注入假时钟/自定义节奏，免得测试真的睡满 20 秒。生产恒为 null。 */
let backfillPaceOverride: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null = null;

export function configureBackfillTransport(http: HttpPort | null): void {
  backfillTransport = http;
}

export function configureBackfillPace(
  override: { pace?: BackfillOptions['pace']; clock?: BackfillOptions['clock'] } | null,
): void {
  backfillPaceOverride = override;
}

/** 最近一次 tick 的结果（给测试/排查用，也给 C18 的 Popup 用）。 */
export function lastBackfillTick(): TickResult | null {
  return lastTick;
}

function alarmsApi(): AlarmsApi | null {
  return (browser as unknown as { alarms?: AlarmsApi }).alarms ?? null;
}

function tabsApi(): { sendMessage: TabSend } | null {
  const tabs = (browser as unknown as { tabs?: { sendMessage?: TabSend } }).tabs;
  // 🔴 tabs.sendMessage 不需要 'tabs' 权限（'tabs' 只管 url/title 这些敏感字段），
  //    而且我们只往【自己注入的内容脚本】发消息。matches 一个字都不用改。
  return tabs && typeof tabs.sendMessage === 'function'
    ? { sendMessage: (id, msg) => tabs.sendMessage!(id, msg) }
    : null;
}

/**
 * 🔴 **生产环境里 http 端口就是在这里被造出来的。**
 *
 * 顺序：显式覆盖（测试） → 指定的那个标签页 → 登记表里任意一个还活着的同源标签页。
 * 一个都没有 ⇒ 返回 undefined ⇒ tickBackfill 如实答 'no-http-port'。
 * 🔴 绝不退化成"由 SW 自己 fetch" —— 那需要 host 权限，也会把取数挪出用户的
 *    登录上下文。宁可这一次不跑。
 */
async function resolveHttpPort(origin: string, senderTabId?: number): Promise<HttpPort | undefined> {
  if (backfillTransport) return backfillTransport;
  const tabs = tabsApi();
  if (!tabs) return undefined;
  // 实时腿那条路上，发消息的那个标签页【此刻就开着、且刚刚发生过一次真实抓取】
  // ——它是最可靠的取数通道，不必再去查登记表。
  if (senderTabId !== undefined) return tabHttpPort(senderTabId, tabs.sendMessage);
  const live = await pickLiveTab(browserLocalStore(), origin, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  return live ? tabHttpPort(live.tabId, tabs.sendMessage) : undefined;
}

/**
 * C18 · Popup 问 background 要的运行时事实。全部是「我这边真实是什么」，不含推测。
 * 🔴 transportWired 不是一个静态标志，而是【现场 ping 一次】的结果：
 *    有没有一个活着的平台标签页可以替我们取数。没有就是没有，Popup 照实说。
 */
export async function backfillRuntimeStatus(): Promise<BackfillRuntimeStatus> {
  // 🔴 C33：一次 ping 同时回答两个问题（通道通不通 / 是哪个平台）。
  //    分两次问会出现「说有通道、却答不上是哪个平台」这种自相矛盾的回答。
  const live = await liveTransport();
  return {
    transportWired: live.wired,
    lastTickReason: lastTick?.reason ?? null,
    liveTarget: live.target,
  };
}

/**
 * 此刻那条活着的取数通道。
 * `wired` 与 C19 的 hasLiveTransport 逐字同义；`target` 是它顺带带出来的事实：
 * 那个标签页的源，以及源在平台表里对应的平台。
 * 🔴 显式覆盖（测试接缝）下 target 恒为 null —— 那条路上没有任何标签页，
 *    编一个平台出来就是撒谎。
 */
async function liveTransport(): Promise<{
  wired: boolean;
  target: { platform: string; origin: string } | null;
}> {
  if (backfillTransport) return { wired: true, target: null };
  const tabs = tabsApi();
  if (!tabs) return { wired: false, target: null };
  const live = await pickLiveTab(browserLocalStore(), null, (id) =>
    tabs.sendMessage(id, { type: BACKFILL_PING_MESSAGE }));
  if (!live) return { wired: false, target: null };
  const row = getPlatformByOrigin(live.origin);
  // 源不在平台表里 ⇒ 我们答不上"这是哪个平台" ⇒ 照实说 null，绝不猜一个。
  return { wired: true, target: row ? { platform: row.id, origin: live.origin } : null };
}

/**
 * 🔴 C33 · 【显式知情同意】那条登记入口：用户在 Popup 上按了「开始回溯这个平台」。
 *
 * 与 kickBackfill 的关系：**这是多一条登记入口，不是放宽任何既有判定。**
 * kickBackfill / rememberTab / 闹钟里 `targets.length` 的判定一个字都没动 ——
 * 这里做的事和 kickBackfill 里那一行 rememberTarget 完全相同，只是目标的来源
 * 从「一次真实捕获」换成了「用户自己按下的这一次」。两个来源都不是我们编的。
 *
 * 🔴 scope（账号标识）怎么办：**记 'default'**，理由写在这里，不许悄悄编一个。
 *  · 通道那一跳（内容脚本 ping）带回来的只有源，**没有任何账号信息** ——
 *    要拿到真实账号得去读页面里的响应体，那是"猜"，本单不做；
 *  · 'default' 不是新发明：它就是本仓「认不出账号」时既有的写法
 *    （entrypoints/background.ts:303 的 `identity.value || 'default'`、
 *      lib/popup-view.ts 的 emptyStateFor、lib/contract.ts 的 IdentityLevel 'default'），
 *    含义逐字是「身份不可靠」，而不是冒充成某个具体账号；
 *  · 取数用的仍然是用户自己那个页面的登录态，所以**补回来的确实是他自己的历史**；
 *    scope 只是欠账账本的分区键，写 'default' 不会把别人的东西补到这里来。
 *  · 代价（如实写下）：以后真的捕获到一次时会再记一份带真实账号的目标，
 *    于是同一个平台下会有两份欠账集合，同一批会话可能被各自清一遍。
 *    文件名是 platform-sessionId、下载是覆盖写 ⇒ 不会串档、不会互相抹掉，
 *    多出来的只是重复的取数与写入。相比「回溯永远不开始」，这个代价是划算的。
 */
export async function registerBackfillTargetHere(): Promise<
  | { ok: true; target: { platform: string; origin: string; scope: string } }
  | { ok: false; reason: 'no-store' | 'no-live-transport' | 'origin-not-a-platform' }
> {
  const store = browserLocalStore();
  if (!store) return { ok: false, reason: 'no-store' };
  const live = await liveTransport();
  if (!live.wired) return { ok: false, reason: 'no-live-transport' };
  if (!live.target) return { ok: false, reason: 'origin-not-a-platform' };
  // 🔴 scope = 'default'：见上面那段。这是既有约定，不是新发明的值。
  const target = { platform: live.target.platform, origin: live.target.origin, scope: 'default' };
  await rememberTarget(store, { ...target, at: Date.now() });
  return { ok: true, target };
}

/** 等待 fire-and-forget 的那次 tick 结束。回溯腿绝不允许拖慢落盘，所以只能这样等。 */
export function backfillTickSettled(): Promise<unknown> {
  return pendingTick;
}

/** 从一次真实捕获里推出回溯目标。推不出来就返回 null（不猜）。 */
export function backfillTargetFor(
  captured: CapturedFetch,
): { platform: string; origin: string; scope: string } | null {
  const row = findPlatformForUrl(captured.url)
    ?? (captured.pageUrl ? findPlatformForUrl(captured.pageUrl) : null);
  if (!row) return null;
  let origin: string | null = null;
  for (const candidate of [captured.pageUrl, captured.url]) {
    if (!candidate) continue;
    try {
      const o = new URL(candidate).origin;
      if (row.origins.includes(o)) { origin = o; break; }
    } catch { /* 不是合法 URL ⇒ 换下一个候选 */ }
  }
  if (!origin) return null;
  // 归档范围键走 ADR-002 的账号轴；认不出账号就用 'default'（与落盘那边一致）。
  const identity = extractIdentity(captured.text, extractSessionId(captured.url, captured.text, captured.pageUrl));
  return { platform: row.id, origin, scope: identity.value || 'default' };
}

/**
 * 实时腿存完一条之后顺带踢一脚回溯腿。全程 best-effort：
 * 任何异常只进日志，绝不影响用户当前这条对话的落盘。
 */
export async function kickBackfill(
  captured: CapturedFetch,
  senderTabId?: number,
): Promise<TickResult | null> {
  const target = backfillTargetFor(captured);
  if (!target) return null;
  const store = browserLocalStore();
  // 闹钟醒来时用得上：把这个「用户真的用过的」目标记下来，省得以后去猜。
  // 记不下来也照跑这一次 —— 登记表只影响闹钟那条路。
  try {
    await rememberTarget(store, { ...target, at: Date.now() });
  } catch (err) {
    console.warn('[chat-stasher] backfill target registry write failed', (err as Error).message);
  }
  const result = await tickBackfill({
    ...target,
    store,
    http: await resolveHttpPort(target.origin, senderTabId),
    downloadGuard: isBackfillPausedByDownloadGuard,
    // 归档出口 = 实时腿同一个落盘函数，逻辑不分叉。
    // 🔴 C20：**必须 return**。以前这里是 `{ await handleCaptured(c); }` —— 花括号
    //    把 HandledResult 吃掉了，于是 engine 拿不到任何异议，没落盘也照样清账。
    sink: (c) => handleCaptured(c),
    ...(backfillPaceOverride ?? {}),
  });
  lastTick = result;
  return result;
}

/**
 * 🔴 闹钟那一脚。与 kickBackfill 走【同一个 tickBackfill】，闸门顺序一致。
 * 与实时腿的唯一区别：目标从 storage 的登记表里读，而不是从一条消息里现取。
 * 一次闹钟最多清 1 笔账（DEFAULT_TICK_DETAILS），跑成了就收手。
 */
export async function runAlarmTick(): Promise<TickResult> {
  const store = browserLocalStore();
  const targets = await loadTargets(store);

  if (targets.length === 0) {
    // 还没有任何目标 ⇒ 用户从没在受支持平台上被抓到过一条。
    // 仍然把闸门跑一遍，好让 Popup 的 lastTickReason 说得出真话。
    const blocked = await tickBlockReason({
      hasStore: store !== null,
      isEnabled: () => isBackfillEnabled(store),
      isDownloadPaused: isBackfillPausedByDownloadGuard,
      // 🔴 C30：这两个值都是【事实】。以前这里把 hasHttp 写死成 false 再兜底成
      //    'no-http-port'，于是「通道好端端接着、只是没有目标」被报成了端口坏了 ——
      //    排查的人顺着那句话去查端口，而端口根本没坏。
      hasHttp: false,
      hasTargets: false,
    });
    lastTick = { ran: false, reason: blocked ?? 'no-targets', report: null };
    await recordAlarmTick(store, lastTick, 0);
    return lastTick;
  }

  let last: TickResult = { ran: false, reason: 'no-http-port', report: null };
  for (const target of targets) {
    const result = await tickBackfill({
      platform: target.platform,
      origin: target.origin,
      scope: target.scope,
      store,
      http: await resolveHttpPort(target.origin),
      downloadGuard: isBackfillPausedByDownloadGuard,
      // 🔴 C20：闹钟那一脚同样必须 return（两条心跳走同一个出口，不许一条报一条不报）。
      sink: (c) => handleCaptured(c),
      ...(backfillPaceOverride ?? {}),
    });
    last = result;
    lastTick = result;
    // 跑动了就收手；被开关/存储/熔断挡住也没必要再试别的目标（结论一样）。
    if (result.reason !== 'no-http-port') break;
  }
  await recordAlarmTick(store, last, targets.length);
  return last;
}

/**
 * 🔴 C30 · 把闹钟这一跳的结论写进存储。
 *
 * 为什么必须落盘而不是只留在 lastTick 变量里：MV3 的 SW 干完这一跳就会被回收，
 * 用户过一会儿点开 Popup 时内存里什么都不剩 —— 真机上看到的正是这个：
 * 闹钟一直在醒、一直什么都没做，而任何地方都查不到它醒过。
 * 写失败只 warn，绝不让「留痕」反过来变成挡住这条腿的新理由。
 */
async function recordAlarmTick(
  store: ReturnType<typeof browserLocalStore>,
  result: TickResult,
  targets: number,
): Promise<void> {
  await saveLastTick(store, {
    at: Date.now(),
    ran: result.ran,
    reason: result.reason,
    targets,
  });
}

function cancelledIdLike(id: string | null): boolean {
  if (!id) return true;
  return id.length < 8 || id === 'unknown';
}

/** 开关是什么状态，闹钟就该是什么状态。SW 每次醒来都对一次表。 */
export async function syncAlarmWithSwitch(): Promise<string> {
  const enabled = await isBackfillEnabled(browserLocalStore());
  return syncBackfillAlarm(alarmsApi(), enabled);
}

type StorageChange = { newValue?: unknown };

function addBackfillSwitchListener(): void {
  const storage = (browser as unknown as {
    storage?: {
      onChanged?: {
        addListener(fn: (changes: Record<string, StorageChange>, areaName: string) => void): void;
      };
    };
  }).storage;
  storage?.onChanged?.addListener((changes, areaName) => {
    if (areaName !== 'local' || !(BACKFILL_ENABLED_KEY in changes)) return;
    // storage.onChanged fires after the write is committed. Re-read through the
    // same gate as startup so remove/non-boolean values also fail closed.
    void syncAlarmWithSwitch().catch((err) => {
      console.warn('[chat-stasher] backfill alarm switch sync failed', (err as Error).message);
    });
  });
}

export default defineBackground(async () => {
  browser.runtime.onMessage.addListener(
    (message: { type?: string; payload?: CapturedFetch }, sender, sendResponse) => {
      // C18：Popup 打开时问一句「取数通道接上没有」。
      // C19 起这个回答要现场 ping 一个标签页 ⇒ 变成异步，仍然 return true。
      if (message?.type === POPUP_STATUS_MESSAGE) {
        backfillRuntimeStatus()
          .then(sendResponse)
          // 问不出来就按最保守的方向答，绝不让 Popup 显示成"在跑"。
          .catch(() => sendResponse({ transportWired: false, lastTickReason: null }));
        return true;
      }
      // C19：内容脚本报到。tab id 由浏览器填在 sender 上（不需要 'tabs' 权限），
      // 记进 storage 之后，闹钟醒来才知道该找谁取数。
      // 🔴 C33：Popup 上那个「开始回溯这个平台」按钮按下来的那一脚。
      //    登记【写完】才回话 —— 回一个 ok 就该意味着"登记表里真的有这一条了"，
      //    否则 Popup 紧接着的那次重画会读到一张还没落盘的表，看起来像没生效。
      if (message?.type === POPUP_START_BACKFILL_MESSAGE) {
        registerBackfillTargetHere()
          .then(sendResponse)
          .catch((err: Error) => {
            console.warn('[chat-stasher] backfill start-here failed', err.message);
            sendResponse({ ok: false, reason: 'no-store' });
          });
        return true;
      }
      if (isTabHello(message)) {
        const tabId = (sender as { tab?: { id?: number } })?.tab?.id;
        if (typeof tabId !== 'number') {
          sendResponse({ ok: false, error: 'no tab id on sender' });
          return true;
        }
        // 🔴 登记【写完】才回话：回一个 ok 就该意味着"闹钟已经找得到你了"，
        // 否则紧跟着的一次闹钟/状态查询会读到一张还没落盘的表。
        rememberTab(browserLocalStore(), { tabId, origin: message.origin, at: Date.now() })
          .then(() => sendResponse({ ok: true }))
          .catch((err: Error) => {
            console.warn('[chat-stasher] tab registry write failed', err.message);
            sendResponse({ ok: false, error: err.message });
          });
        return true;
      }
      if (message?.type !== 'chat-captured' || !message.payload) return;
      const payload = message.payload;
      const senderTabId = (sender as { tab?: { id?: number } })?.tab?.id;
      handleCaptured(payload)
        .then((result) => {
          // C13：实时腿这一脚顺带唤醒回溯腿。fire-and-forget —— 回溯是慢活，
          // 绝不允许它挡住 sendResponse 或拖慢用户当前这条对话的落盘。
          pendingTick = kickBackfill(payload, senderTabId).catch((err) => {
            console.warn('[chat-stasher] backfill tick failed', (err as Error).message);
            return null;
          });
          if (result.saved) {
            // Privacy rule: never the conversation content — ids/bytes only.
            console.log(
              `[chat-stasher] saved ${result.bytes} bytes -> ${result.finalName}`,
            );
          }
          sendResponse({ ok: result.saved, ...result });
        })
        .catch((err) => {
          console.error('[chat-stasher] handleCaptured failed', (err as Error).message);
          sendResponse({ ok: false, error: (err as Error).message });
        });
      // MV3: async sendResponse requires returning true to keep the channel open.
      return true;
    },
  );

  // 🔴 C19 · 闹钟的监听器必须在 SW 顶层【同步】注册：MV3 里 SW 被回收后是由
  //    事件重新唤醒的，注册晚了就会错过那次唤醒。
  const alarms = (browser as unknown as {
    alarms?: AlarmsApi & { onAlarm?: { addListener(fn: (a: { name?: string }) => void): void } };
  }).alarms;
  alarms?.onAlarm?.addListener((alarm) => {
    if (alarm?.name !== BACKFILL_ALARM_NAME) return;
    pendingTick = runAlarmTick().catch((err) => {
      console.warn('[chat-stasher] backfill alarm tick failed', (err as Error).message);
      return null;
    });
  });

  // Popup changes the persisted switch from another extension context. Keep
  // the alarm lifecycle closed immediately instead of waiting for a later SW
  // wake-up. The listener itself is synchronous, as required by MV3.
  addBackfillSwitchListener();

  // Every SW wake (fresh start AND runtime.onStartup) re-asserts the badge's
  // truth, so a dead-worker leftover badge gets cleared once 5 min pass.
  void refreshBadge();
  // 开关是持久的，闹钟也该是。每次 SW 醒来对一次表：开着就确保有闹钟，
  // 关着就确保没有 —— 🔴 默认（关）下这里只会 clear，绝不会凭空创建。
  await syncAlarmWithSwitch().catch((err) => {
    console.warn('[chat-stasher] backfill alarm sync failed', (err as Error).message);
  });
  browser.runtime.onStartup.addListener(() => {
    void refreshBadge();
    void syncAlarmWithSwitch().catch(() => { /* 日志已在上面那条路径覆盖 */ });
  });

  console.log('[chat-stasher] background ready, captures go to Downloads/ for explicit platform origins');
});
