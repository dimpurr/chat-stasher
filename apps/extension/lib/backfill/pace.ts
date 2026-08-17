/**
 * 节流 —— 必须是【可观测的数字】，不是「感觉上慢一点」。
 *
 * 为什么枚举和取正文要分开定速：
 *  · ChatGPT 的会话列表是 GET /backend-api/conversations?offset=&limit=，
 *    响应自带 total，1000 条大约 10 页就枚举完 ⇒ 枚举总共约 10 次请求，很便宜。
 *  · 真正要「温和」的是随后逐条取正文的那 1000 次。
 *  两段用同一个速率，要么枚举被拖到没必要的慢，要么取正文快到不像人。
 *
 * 默认值与理由（可配置，这里只是默认）：
 *  · 枚举 minIntervalMs = 2000（每 2 秒一页）
 *      ⇒ 1000 条 ≈ 10 页 ≈ 20 秒枚举完。比人手滚动侧边栏还慢，且总量只有 10 次。
 *  · 取正文 minIntervalMs = 20000（每 20 秒一条）
 *      ⇒ 比任何真人连续点开对话都慢一个量级，不可能被当成脉冲式抓取。
 *  · 取正文 maxPerDay = 200（每天最多 200 条）
 *      ⇒ 1000 条对话正好摊到 5 天，与产品要的「好几天之后慢慢全部索引进来」一致；
 *        200 × 20s ≈ 67 分钟/天的稀疏活动，落在正常使用者的日活范围内。
 *
 * ⚠️ 这些数字是【按上述算术选的默认值】，**不是**对平台限流阈值的实测结果 ——
 *    我们没有登录态，也不该去试探阈值。真正的阈值只能靠 halt-on-429 兜底。
 */

/** 可注入的时钟：测试用假时钟数 sleep，不真的等。 */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

export const systemClock: Clock = {
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

export interface PacePlan {
  /** 两次请求之间的最小间隔（毫秒）。 */
  minIntervalMs: number;
  /** 每天上限；null = 不限（枚举那段就是 null）。 */
  maxPerDay: number | null;
}

export const DEFAULT_ENUM_PACE: PacePlan = { minIntervalMs: 2_000, maxPerDay: null };
export const DEFAULT_DETAIL_PACE: PacePlan = { minIntervalMs: 20_000, maxPerDay: 200 };

export interface BackfillPace {
  enumerate: PacePlan;
  detail: PacePlan;
}

export const DEFAULT_PACE: BackfillPace = {
  enumerate: DEFAULT_ENUM_PACE,
  detail: DEFAULT_DETAIL_PACE,
};

/**
 * 一段的节流器。第一次请求不等（没有「上一次」可言），之后每次都补足最小间隔。
 * 记录 waits/totalWaitedMs，好让「节流真的生效」是可以被贴出来的数字。
 */
export class Pacer {
  private lastAt: number | null = null;
  readonly waits: number[] = [];

  constructor(
    readonly plan: PacePlan,
    private readonly clock: Clock,
    readonly label: string,
  ) {}

  get totalWaitedMs(): number {
    return this.waits.reduce((a, b) => a + b, 0);
  }

  /** 在每次真实请求之前调用。返回实际等待的毫秒数。 */
  async gate(): Promise<number> {
    const now = this.clock.now();
    if (this.lastAt === null) {
      this.lastAt = now;
      this.waits.push(0);
      return 0;
    }
    const elapsed = now - this.lastAt;
    const wait = Math.max(0, this.plan.minIntervalMs - elapsed);
    if (wait > 0) await this.clock.sleep(wait);
    this.lastAt = this.clock.now();
    this.waits.push(wait);
    return wait;
  }
}
