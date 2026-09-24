/**
 * ADR-032 · **The coverage page's wiring: paint the blocks, and nothing else.**
 *
 * What is shown is decided in `lib/coverage.ts` (the facts) and `lib/coverage-view.ts` (the wording and
 * order); this file walks that array into elements. That split is the same one the popup uses, and for the
 * same reason — a page that builds its own sentences can say something no test can pin.
 *
 * 🔴 **This page issues no request to any chat platform, and never will.** It reads this browser's own
 *    `storage.local` and the backfill IndexedDB, and it writes exactly one thing: the speed preset, when
 *    the user presses one of the three buttons. Everything that could reach a platform lives behind the
 *    content scripts and the alarm, and none of it is imported here — `lib/coverage-read.ts` is the only
 *    module that touches storage at all, and the model it feeds has no network code in it.
 *
 * 🔴 **It is not a native-messaging client either.** The archive's own view is `chat-stasher ui`
 *    (ADR-028 §1, ADR-032 §2). Asking the host for those numbers would put a second copy of them in front
 *    of the user and would need a protocol change this ticket is not authorised to make.
 */

import { browserLocalStore } from '../../lib/backfill/store';
import { writeSpeedPreset, type SpeedPreset } from '../../lib/backfill/speed';
import { buildCoverage } from '../../lib/coverage';
import { readCoverageInputs } from '../../lib/coverage-read';
import { coverageView, presetWhat, speedBlocks, type CoverageBlock } from '../../lib/coverage-view';
import { initUiLocale, t } from '../../lib/i18n';

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

/** Paint one block. Returns `null` for a block kind this build does not know, rather than throwing inside a render. */
function paintBlock(block: CoverageBlock, onPick: (preset: SpeedPreset) => void, current: SpeedPreset): HTMLElement | null {
  switch (block.kind) {
    case 'heading':
      return el(block.level === 2 ? 'h2' : 'h3', undefined, block.text);
    case 'note':
      return el('div', `note ${block.tone}`, block.text);
    case 'facts': {
      const wrap = el('div', 'facts');
      for (const row of block.rows) {
        const line = el('div', 'fact');
        line.append(el('span', 'label', row.label), el('span', 'value', row.value));
        wrap.append(line);
      }
      return wrap;
    }
    case 'table': {
      const table = el('table');
      const head = el('tr');
      for (const header of block.headers) head.append(el('th', undefined, header));
      table.append(head);
      for (const row of block.rows) {
        const tr = el('tr');
        for (const cell of row) tr.append(el('td', undefined, cell));
        table.append(tr);
      }
      return table;
    }
    case 'list': {
      const list = el('ul');
      for (const item of block.items) list.append(el('li', undefined, item));
      return list;
    }
    case 'speed': {
      const wrap = el('div', 'speed');
      for (const option of block.options) {
        const button = el('button', undefined, option.label);
        button.type = 'button';
        // The pressed state is the stored preset, not the button that was last clicked: a write that did
        // not land must not look like one that did.
        button.setAttribute('aria-pressed', String(option.preset === current));
        button.dataset.preset = option.preset;
        button.addEventListener('click', () => onPick(option.preset));
        wrap.append(button);
      }
      wrap.append(el('div', 'what', presetWhat(current)));
      return wrap;
    }
    default:
      return null;
  }
}

/** The elements the page paints into. Read once so a missing one is a named error rather than a null deref mid-render. */
function hosts(): {
  title: HTMLElement;
  subtitle: HTMLElement;
  banners: HTMLElement;
  empty: HTMLElement;
  sections: HTMLElement;
  speed: HTMLElement;
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
    banners: get('banners'),
    empty: get('empty'),
    sections: get('sections'),
    speed: get('speed'),
    error: get('error'),
  };
}

async function render(store: ReturnType<typeof browserLocalStore>, now: number): Promise<void> {
  const host = hosts();
  const inputs = await readCoverageInputs(store, now);
  const report = buildCoverage(inputs);
  const view = coverageView(report, now);

  host.title.textContent = view.title;
  host.subtitle.textContent = view.subtitle;
  host.banners.replaceChildren();
  for (const banner of view.banners) {
    const node = paintBlock(banner, () => {}, report.preset);
    if (node) host.banners.append(node);
  }

  host.empty.textContent = view.legend;
  host.empty.hidden = view.legend === '';

  // Every scope's blocks are painted into one flow. The platform and scope headings inside them are what
  // groups them, so a platform with two accounts reads as two labelled groups rather than one merged one.
  host.sections.replaceChildren();
  const onPick = (preset: SpeedPreset): void => {
    void applyPreset(store, preset);
  };
  for (const section of view.sections) {
    for (const block of section.blocks) {
      const node = paintBlock(block, onPick, report.preset);
      if (node) host.sections.append(node);
    }
  }

  // The control is painted once, under the sections: it governs all of them.
  host.speed.replaceChildren();
  // 🔴 From the view module, not rebuilt here: the risk note that appears with the preset that carries it
  //    is worded once (ADR-032 §3), and a second copy of the control could say something else about it.
  for (const block of speedBlocks(report)) {
    const node = paintBlock(block, onPick, report.preset);
    if (node) host.speed.append(node);
  }
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
