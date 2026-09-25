/**
 * ADR-032 · **The coverage page's wiring: paint the view, and nothing else.**
 *
 * What is shown is decided in `lib/coverage.ts` (the facts) and `lib/coverage-view.ts` (the wording and
 * order); this file walks that structured view into elements. That split is the same one the popup uses,
 * and for the same reason — a page that builds its own sentences or its own numbers can say something no
 * test can pin.
 *
 * W149 · The walkthrough is card-shaped: an overview header (three sums, one health line, dismissible
 * global alerts, the last wake demoted to a meta row), one card per (platform × scope) with a composition
 * bar, a percent ring only where the model allowed a percentage, a quota meter and an SVG monthly
 * distribution, and a native-radio segmented control for the speed preset. The honesty rules are not
 * enforced here — they live in the model (what counts exist), in the view (which sentences are shown),
 * and in `lib/coverage-charts.ts` (what geometry a chart may draw) — and this file only draws what those
 * three decided. In particular:
 *
 *   · an alert segment with no count is never drawn (`barGeometry` drops it), so an unknown is not
 *     painted as a zero-width measurement;
 *   · the ring is built only when `card.percent !== null`, and a bare track never appears without one;
 *   · the quota meter's fill is `bodies/cap` exactly, and a stale counter draws no fill at all.
 *
 * 🔴 **This page issues no request to any chat platform, and never will.** It reads this browser's own
 *    `storage.local` and the backfill IndexedDB, and it writes exactly one thing: the speed preset, when
 *    the user picks one of the three radios. Everything that could reach a platform lives behind the
 *    content scripts and the alarm, and none of it is imported here — `lib/coverage-read.ts` is the only
 *    module that touches storage at all, and the model it feeds has no network code in it.
 *
 * 🔴 **It is not a native-messaging client either.** The archive's own view is `chat-stasher ui`
 *    (ADR-028 §1, ADR-032 §2). Asking the host for those numbers would put a second copy of them in front
 *    of the user and would need a protocol change this ticket is not authorised to make.
 *
 * 🔴 **The dismissed-alert set is memory only.** A dismissed callout stays hidden across the repaint a
 *    preset change causes, and dies with the page. This page is read-only apart from the preset, and a
 *    "don't show me this again" that wrote to storage would make that false.
 */

import { browserLocalStore } from '../../lib/backfill/store';
import { writeSpeedPreset, type SpeedPreset } from '../../lib/backfill/speed';
import { buildCoverage } from '../../lib/coverage';
import { readCoverageInputs } from '../../lib/coverage-read';
import { coverageView, type CoverageAlert, type CoverageCardView } from '../../lib/coverage-view';
import { barGeometry, monthLabel, monthsGeometry, ringDash, unknownTimeTotal } from '../../lib/coverage-charts';
import { initUiLocale, t } from '../../lib/i18n';

const SVG_NS = 'http://www.w3.org/2000/svg';
/** The bar's proportional coordinate space; widths divide out to exact percentages in CSS. */
const BAR_WIDTH = 1000;
/** The months chart's own units. The chart keeps its aspect ratio, so labels never skew. */
const MONTHS_W = 320;
const MONTHS_H = 52;
const MONTHS_GAP = 6;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function svg<K extends keyof SVGElementTagNameMap>(tag: K): SVGElementTagNameMap[K] {
  return document.createElementNS(SVG_NS, tag) as unknown as SVGElementTagNameMap[K];
}

// ---------------------------------------------------------------------------
// Alerts (dismissible, session only)
// ---------------------------------------------------------------------------

/** Alerts the reader has dismissed. Memory only — see the file header for why it is not persisted. */
const dismissed = new Set<string>();

function isDismissed(alert: CoverageAlert): boolean {
  return dismissed.has(alert.id);
}

function paintAlert(alert: CoverageAlert): HTMLElement {
  const box = el('div', `alert alert-${alert.tone}`);
  box.dataset.alertId = alert.id;
  const text = el('span', undefined, alert.text);
  text.className = 'alert-text';
  const dismiss = el('button', 'dismiss', '×');
  dismiss.type = 'button';
  dismiss.setAttribute('aria-label', t('coverage.dismiss'));
  dismiss.addEventListener('click', () => {
    dismissed.add(alert.id);
    box.remove();
  });
  box.append(text, dismiss);
  return box;
}

// ---------------------------------------------------------------------------
// One card
// ---------------------------------------------------------------------------

/** The percent ring. Only ever built for a non-null percent; the geometry comes from `ringDash`. */
function paintRing(percent: number, title: string): SVGSVGElement {
  const size = 44;
  const radius = 18;
  const node = svg('svg');
  node.setAttribute('class', 'ring');
  node.setAttribute('viewBox', `0 0 ${size} ${size}`);
  node.setAttribute('role', 'img');
  node.setAttribute('aria-label', title);
  const track = svg('circle');
  track.setAttribute('class', 'ring-track');
  track.setAttribute('cx', '22');
  track.setAttribute('cy', '22');
  track.setAttribute('r', String(radius));
  const dash = ringDash(percent, radius);
  if (dash) {
    const fillCircle = svg('circle');
    fillCircle.setAttribute('class', 'ring-fill');
    fillCircle.setAttribute('cx', '22');
    fillCircle.setAttribute('cy', '22');
    fillCircle.setAttribute('r', String(radius));
    fillCircle.setAttribute('stroke-dasharray', `${dash.dash} ${dash.gap}`);
    fillCircle.setAttribute('transform', 'rotate(-90 22 22)');
    node.append(track, fillCircle);
  } else {
    node.append(track);
  }
  const label = svg('text');
  label.setAttribute('class', 'ring-num');
  label.setAttribute('x', '22');
  label.setAttribute('y', '25');
  label.setAttribute('text-anchor', 'middle');
  label.textContent = `${percent}%`;
  node.append(label);
  return node;
}

/** The composition bar. `barGeometry` decides what exists; the remainder is left as bare track. */
function paintBar(card: CoverageCardView): HTMLElement | null {
  const segments = barGeometry({ ...card.bar }, BAR_WIDTH);
  const total = card.bar.archived + card.bar.owed + card.bar.failed + card.bar.remainder;
  if (total === 0) return null;
  const wrap = el('div', 'bar-wrap');
  const hasDrawn = segments.some((segment) => segment.tone !== 'remainder');
  if (hasDrawn) {
    const bar = el('div', 'bar');
    bar.setAttribute('role', 'img');
    bar.setAttribute('aria-label', card.legend.map((entry) => `${entry.label} ${entry.count}`).join(', '));
    for (const segment of segments) {
      if (segment.tone === 'remainder') continue;
      const div = el('i', `seg-${segment.tone === 'archived' ? 'ok' : segment.tone === 'owed' ? 'hatch' : 'bad'}`);
      div.style.width = `${(segment.w / BAR_WIDTH) * 100}%`;
      bar.append(div);
    }
    wrap.append(bar);
  }
  const legend = el('ul', 'legend');
  for (const entry of card.legend) {
    const item = el('li');
    const swClass = entry.tone === 'ok' ? 'ok' : entry.tone === 'hatch' ? 'hatch' : entry.tone === 'bad' ? 'bad' : 'track';
    const swatch = el('span', `sw sw-${swClass}`);
    swatch.setAttribute('aria-hidden', 'true');
    item.append(swatch, el('span', undefined, `${entry.label} `), el('span', 'n', String(entry.count)));
    legend.append(item);
  }
  wrap.append(legend);
  return wrap;
}

function paintQuota(card: CoverageCardView): HTMLElement {
  const box = el('div', 'quota');
  const meter = el('div', 'meter');
  const fill = el('i');
  const cap = card.quota.dayCap;
  if (cap !== null) {
    const ratio = Math.max(0, Math.min(1, card.quota.counterStale ? 0 : card.quota.bodiesToday / cap));
    fill.style.width = `${ratio * 100}%`;
  }
  meter.append(fill);
  box.append(meter, el('p', 'ql', card.quota.line));
  return box;
}

let patternCounter = 0;

/**
 * The monthly distribution. Geometry from `monthsGeometry` (the time-unknown bucket is its own trailing
 * column, never a month), months labelled `YY/MM`, every column titled with its exact counts for the
 * accessible name, and nothing drawn at all when there is nothing to draw (`months.none` sentence).
 */
function paintMonths(card: CoverageCardView): HTMLElement {
  const box = el('div', 'months');
  if (card.months.title === null) {
    box.append(el('p', 'line mini', card.months.noneNote ?? ''));
    return box;
  }
  const geometry = monthsGeometry(card.months.months, card.months.unknownTime, {
    width: MONTHS_W,
    height: MONTHS_H,
    gap: MONTHS_GAP,
  });
  if (geometry === null) {
    box.append(el('p', 'line mini', card.months.noneNote ?? ''));
    return box;
  }
  // Exact counts for the accessible names come from the model's own numbers, never back out of the
  // drawn heights: geometry is for the eye, the title is for the reader.
  const monthsByKey = new Map(card.months.months.map((month) => [month.month, month]));
  patternCounter += 1;
  const patternId = `cs-hatch-${patternCounter}`;
  const labelH = 12;
  const node = svg('svg');
  node.setAttribute('viewBox', `0 0 ${MONTHS_W} ${MONTHS_H + labelH}`);
  node.setAttribute('role', 'img');
  node.setAttribute('aria-label', card.months.title as string);

  const defs = svg('defs');
  const pattern = svg('pattern');
  pattern.setAttribute('id', patternId);
  pattern.setAttribute('patternUnits', 'userSpaceOnUse');
  pattern.setAttribute('width', '5');
  pattern.setAttribute('height', '5');
  pattern.setAttribute('patternTransform', 'rotate(45)');
  const line = svg('line');
  line.setAttribute('class', 'm-hatchline');
  line.setAttribute('x1', '0');
  line.setAttribute('y1', '0');
  line.setAttribute('x2', '0');
  line.setAttribute('y2', '5');
  pattern.append(line);
  defs.append(pattern);
  node.append(defs);

  for (let index = 0; index < geometry.columns.length; index += 1) {
    const column = geometry.columns[index]!;
    const group = svg('g');
    const title = svg('title');
    if (column.kind === 'month') {
      const data = monthsByKey.get(column.key);
      title.textContent = `${monthLabel(column.key)}: ${card.months.archivedLabel} ${data?.archived ?? 0}, ${card.months.pendingLabel} ${data?.pending ?? 0}`;
    } else {
      title.textContent = `${card.months.unknownLabel}: ${column.total}`;
    }
    group.append(title);
    if (column.kind === 'month') {
      if (column.hArchived > 0) {
        const archived = svg('rect');
        archived.setAttribute('class', 'col-ok');
        archived.setAttribute('x', String(column.x));
        archived.setAttribute('y', String(MONTHS_H - column.hArchived));
        archived.setAttribute('width', String(column.w));
        archived.setAttribute('height', String(column.hArchived));
        group.append(archived);
      }
      if (column.hPending > 0) {
        // Stacked above the archived part, so no gap can read as an empty month.
        const pending = svg('rect');
        pending.setAttribute('fill', `url(#${patternId})`);
        pending.setAttribute('x', String(column.x));
        pending.setAttribute('y', String(MONTHS_H - column.hArchived - column.hPending));
        pending.setAttribute('width', String(column.w));
        pending.setAttribute('height', String(column.hPending));
        group.append(pending);
      }
    } else if (column.hUnknown > 0) {
      const unknown = svg('rect');
      unknown.setAttribute('fill', `url(#${patternId})`);
      unknown.setAttribute('x', String(column.x));
      unknown.setAttribute('y', String(MONTHS_H - column.hUnknown));
      unknown.setAttribute('width', String(column.w));
      unknown.setAttribute('height', String(column.hUnknown));
      group.append(unknown);
    }
    // The stride decides which months get a visible label, so labels never collide at small widths.
    // The time-unknown column always keeps its name: the name is the one thing that says it is not a
    // month, and a missing name would let it pass for one. Its name sits right-anchored on the last
    // edge, so the month before it drops *its* visible label when the unknown column exists — the
    // chart's titles and the details table still name every month.
    const hasUnknown = geometry.columns.some((entry) => entry.kind === 'unknown');
    const lastMonthIndex = geometry.columns.length - (hasUnknown ? 2 : 0);
    const showLabel = column.kind === 'unknown'
      || (index % geometry.stride === 0 && !(hasUnknown && index === geometry.columns.length - 2));
    if (showLabel) {
      const axisLabel = svg('text');
      axisLabel.setAttribute('class', 'm-label');
      axisLabel.setAttribute('y', String(MONTHS_H + 10));
      if (column.kind === 'unknown') {
        // The unknown column is the last one: its name is anchored to the right edge so it can never
        // be clipped by the viewBox (a half-visible name would read as a truncated month).
        axisLabel.setAttribute('text-anchor', 'end');
        axisLabel.setAttribute('x', String(MONTHS_W));
      } else {
        // Centred on the column, but clamped in from both edges: a centred label on the first or last
        // column would otherwise reach past the chart and be cut off.
        axisLabel.setAttribute('text-anchor', 'middle');
        const desired = column.x + column.w / 2;
        axisLabel.setAttribute('x', String(Math.max(16, Math.min(desired, MONTHS_W - 16))));
      }
      axisLabel.textContent = column.kind === 'month' ? monthLabel(column.key) : card.months.unknownLabel;
      group.append(axisLabel);
    }
    node.append(group);
  }
  const axis = svg('line');
  axis.setAttribute('class', 'axis');
  axis.setAttribute('x1', '0');
  axis.setAttribute('y1', String(MONTHS_H + 0.5));
  axis.setAttribute('x2', String(MONTHS_W));
  axis.setAttribute('y2', String(MONTHS_H + 0.5));
  node.append(axis);

  box.append(node);
  return box;
}

/** The details disclosure: every remaining ADR-032 fact, including the months as exact numbers. */
function paintDetails(card: CoverageCardView): HTMLElement {
  const details = el('details', 'details');
  const summary = el('summary', undefined, card.detailsSummary);
  details.append(summary);
  for (const row of card.detailRows) {
    const line = el('div', 'dr');
    line.append(el('span', 'k', row.label), el('span', 'v', row.value));
    details.append(line);
  }
  // The months table: the same numbers the chart drew, one row per month, the time-unknown bucket last.
  const hasMonths = card.months.months.length > 0 || unknownTimeTotal(card.months.unknownTime) > 0;
  if (hasMonths) {
    const table = el('table');
    const head = el('tr');
    head.append(
      el('th'),
      el('th', undefined, card.months.archivedLabel),
      el('th', undefined, card.months.pendingLabel),
    );
    table.append(head);
    for (const month of card.months.months) {
      const tr = el('tr');
      tr.append(el('td', undefined, monthLabel(month.month)), el('td', undefined, String(month.archived)), el('td', undefined, String(month.pending)));
      table.append(tr);
    }
    if (unknownTimeTotal(card.months.unknownTime) > 0) {
      const tr = el('tr');
      tr.append(
        el('td', undefined, card.months.unknownLabel),
        el('td', undefined, String(card.months.unknownTime.archived)),
        el('td', undefined, String(card.months.unknownTime.pending)),
      );
      table.append(tr);
    }
    details.append(table);
  }
  return details;
}

function paintCard(card: CoverageCardView): HTMLElement {
  const section = el('section', 'card');
  section.dataset.hue = card.hueKey;

  const head = el('div', 'card-head');
  const mono = el('span', 'mono', card.monogram);
  mono.setAttribute('aria-hidden', 'true');
  const id = el('div', 'card-id');
  const h2 = el('h2', undefined, card.platform);
  const scope = el('p', 'scope', card.scope);
  id.append(h2, scope);
  const chip = el('span', `chip chip-${card.chip.tone}`, card.chip.word);
  head.append(mono, id, chip);
  if (card.percent !== null && card.percentTitle !== null) {
    head.append(paintRing(card.percent, card.percentTitle));
  }
  section.append(head);

  const bar = paintBar(card);
  if (bar) section.append(bar);

  section.append(el('p', 'line listed', card.listed));
  if (card.percentNote !== null) {
    section.append(el('p', 'line mini', card.percentNote));
  }
  section.append(el('p', 'line eta', card.eta));
  section.append(paintQuota(card));

  if (card.months.title !== null || card.months.noneNote !== null) {
    section.append(paintMonths(card));
  }
  if (card.months.unknownNote !== null) {
    section.append(el('p', 'line mini', card.months.unknownNote));
  }

  const alerts = el('div', 'alerts');
  for (const alert of card.alerts) {
    if (isDismissed(alert)) continue;
    alerts.append(paintAlert(alert));
  }
  if (alerts.childElementCount > 0) section.append(alerts);

  section.append(paintDetails(card));
  return section;
}

// ---------------------------------------------------------------------------
// Page assembly
// ---------------------------------------------------------------------------

/** The elements the page paints into. Read once so a missing one is a named error rather than a null deref mid-render. */
function hosts(): {
  title: HTMLElement;
  subtitle: HTMLElement;
  aboutSummary: HTMLElement;
  aboutNote: HTMLElement;
  overview: HTMLElement;
  health: HTMLElement;
  banners: HTMLElement;
  tickline: HTMLElement;
  sections: HTMLElement;
  speed: HTMLElement;
  empty: HTMLElement;
  error: HTMLElement;
} {
  const get = (id: string): HTMLElement => {
    const node = document.getElementById(id);
    if (!node) throw new Error(`[chat-stasher] coverage page is missing #${id}`);
    return node;
  };
  return {
    title: get('title'),
    subtitle: get('subtitle'),
    aboutSummary: get('about-summary'),
    aboutNote: get('about-note'),
    overview: get('overview'),
    health: get('health'),
    banners: get('banners'),
    tickline: get('tickline'),
    sections: get('sections'),
    speed: get('speed'),
    empty: get('empty'),
    error: get('error'),
  };
}

/** The speed control: three native radios, one line each. */
function paintSpeed(
  box: HTMLElement,
  current: SpeedPreset,
  options: Array<{ preset: SpeedPreset; label: string; what: string }>,
  onPick: (preset: SpeedPreset) => void,
): void {
  const heading = el('h2', undefined, t('coverage.preset.title'));
  const group = el('div', 'speed');
  group.setAttribute('role', 'radiogroup');
  group.setAttribute('aria-labelledby', 'speed-title');
  heading.id = 'speed-title';
  for (const option of options) {
    const input = document.createElement('input');
    input.type = 'radio';
    input.name = 'speed';
    input.value = option.preset;
    input.id = `speed-${option.preset}`;
    // The checked state is the stored preset, not the radio that was last clicked: a write that
    // did not land must not look like one that did.
    input.checked = option.preset === current;
    input.addEventListener('change', () => {
      if (input.checked) onPick(option.preset);
    });
    const label = el('label');
    label.htmlFor = input.id;
    const name = el('span', 'opt-name', option.label);
    const what = el('span', 'opt-what', option.what);
    label.append(name, what);
    group.append(input, label);
  }
  box.replaceChildren(heading, group);
}

async function render(store: ReturnType<typeof browserLocalStore>, now: number): Promise<void> {
  const host = hosts();
  const inputs = await readCoverageInputs(store, now);
  const report = buildCoverage(inputs);
  const view = coverageView(report, now);

  host.title.textContent = view.title;
  host.subtitle.textContent = view.subtitle;
  host.aboutSummary.textContent = view.aboutSummary;
  host.aboutNote.textContent = view.aboutNote;

  const overviewStats = host.overview;
  overviewStats.replaceChildren();
  host.overview.hidden = view.stats === null;
  if (view.stats !== null) {
    const stats: Array<{ id: string; label: string; value: number; cls: string }> = [
      { id: 'stored', label: t('coverage.labels.archived'), value: view.stats.stored, cls: '' },
      { id: 'owed', label: t('coverage.labels.pending'), value: view.stats.owed, cls: '' },
      { id: 'failed', label: t('coverage.labels.failures'), value: view.stats.failed, cls: 'stat-failed' },
    ];
    for (const stat of stats) {
      const box = el('span', `stat stat-${stat.id} ${stat.cls}`);
      const value = el('span', 'v', String(stat.value));
      value.id = `stat-${stat.id}`;
      box.append(value, el('span', 'l', stat.label));
      host.overview.append(box);
    }
  }

  host.health.replaceChildren();
  host.health.hidden = view.health === null;
  if (view.health !== null) {
    host.health.className = `health health-${view.health.tone}`;
    host.health.textContent = view.health.text;
  }

  host.banners.replaceChildren();
  for (const alert of view.alerts) {
    if (isDismissed(alert)) continue;
    host.banners.append(paintAlert(alert));
  }
  host.banners.hidden = view.alerts.length === 0;

  host.tickline.textContent = view.tickNote ?? '';
  host.tickline.hidden = view.tickNote === null;

  host.sections.replaceChildren();
  for (const card of view.cards) {
    host.sections.append(paintCard(card));
  }

  const onPick = (preset: SpeedPreset): void => {
    void applyPreset(store, preset);
  };
  paintSpeed(host.speed, view.speed.current, view.speed.options, onPick);
  if (view.speed.riskNote !== null) {
    host.speed.append(el('p', 'speed-risk', view.speed.riskNote));
  }

  host.empty.textContent = view.empty ?? '';
  host.empty.hidden = view.empty === null;
}

async function applyPreset(store: ReturnType<typeof browserLocalStore>, preset: SpeedPreset): Promise<void> {
  const host = hosts();
  try {
    // Writes one string. The next tick reads it through `presetTickOptions` in the background worker —
    // this page does not wake anything, and it does not touch a platform.
    await writeSpeedPreset(store, preset);
    host.error.hidden = true;
  } catch (err) {
    // 🔴 A failed write is shown, not swallowed: a control that says "faster" while nothing is running
    //    faster is the one thing this page may not do.
    host.error.textContent = t('coverage.preset.failed', { detail: (err as Error).message });
    host.error.hidden = false;
  }
  await render(store, Date.now());
}

void initUiLocale()
  .catch((err) => {
    console.warn('[chat-stasher] coverage locale init failed', (err as Error).message);
  })
  .then(() => render(browserLocalStore(), Date.now()))
  .catch((err) => {
    const host = hosts();
    host.error.textContent = `${(err as Error).message}`;
    host.error.hidden = false;
  });
