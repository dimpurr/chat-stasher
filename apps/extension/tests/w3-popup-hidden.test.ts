// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

/**
 * Seen in a real Chrome (2026-09-14): the popup's buttons stayed visible after
 * the render code set `hidden`, because `#start-backfill` / `#export-file` set
 * `display: block`, and an author `display` beats the browser's own
 * `[hidden] { display: none }`. Unit tests never saw it: they check the view
 * model, not the stylesheet.
 */
const root = resolve(__dirname, '..');
const popupHtml = readFileSync(resolve(root, 'entrypoints/popup/index.html'), 'utf8');

describe('popup: `hidden` really hides', () => {
  it('the stylesheet forces [hidden] to display:none with !important', () => {
    expect(popupHtml).toMatch(/\[hidden\]\s*\{\s*display:\s*none\s*!important;?\s*\}/);
  });

  it('every element that can be hidden and also sets display is covered by that rule', () => {
    const hiddenIds = [...popupHtml.matchAll(/id="([\w-]+)"[^>]*\shidden\b/g)].map((m) => m[1]);
    expect(hiddenIds).toEqual(expect.arrayContaining(['start-backfill', 'export-file']));
    const withDisplay = hiddenIds.filter((id) =>
      new RegExp(`#${id}\\s*\\{[^}]*display\\s*:`).test(popupHtml));
    // These are exactly the ones the [hidden] rule exists for; if the rule is
    // removed, the first test fails.
    expect(withDisplay.length).toBeGreaterThan(0);
  });
});

describe('popup: long notes and coverage text wrap', () => {
  it('keeps a long synthetic scope inside the popup text containers', () => {
    const styles = popupHtml.match(/<style>([\s\S]*?)<\/style>/)?.[1];
    expect(styles).toBeDefined();
    const style = document.createElement('style');
    style.textContent = styles!;
    document.head.appendChild(style);
    document.body.innerHTML = '<div id="dashboard-note"></div><div class="notes" id="notes"></div><div id="coverage"></div><div id="coverage-card"></div>';
    const scope = `synthetic-scope-${'account-segment-'.repeat(24)}`;
    const dashboardNote = document.querySelector<HTMLElement>('#dashboard-note')!;
    dashboardNote.textContent = `Diagnostic for ${scope}`;
    const note = document.createElement('p');
    note.textContent = `Diagnostic for ${scope}`;
    document.querySelector('#notes')!.appendChild(note);

    const coverage = document.querySelector<HTMLElement>('#coverage')!;
    coverage.textContent = `Coverage for ${scope}`;
    const card = document.querySelector<HTMLElement>('#coverage-card')!;
    const line = document.createElement('div');
    line.className = 'line';
    line.textContent = `Coverage note for ${scope}`;
    card.appendChild(line);
    const rowText = document.createElement('div');
    rowText.className = 'cv-text';
    rowText.textContent = `chatgpt (${scope}): archived 1`;
    card.appendChild(rowText);

    for (const element of [dashboardNote, note, coverage, line, rowText]) {
      expect(getComputedStyle(element).overflowWrap).toBe('anywhere');
    }
    expect(dashboardNote.textContent).toContain(scope);
    expect(note.textContent).toContain(scope);
    expect(coverage.textContent).toContain(scope);
    expect(line.textContent).toContain(scope);
    expect(rowText.textContent).toContain(scope);
  });
});

describe('popup copy: no leftover download channel', () => {
  it('neither locale tells the user captures go to the download directory', () => {
    for (const file of ['locales/en.yml', 'locales/zh_CN.yml']) {
      const text = readFileSync(resolve(root, file), 'utf8');
      // The Chinese phrase ("download directory") is escaped: CJK text belongs in zh_CN.yml only.
      expect(text, file).not.toMatch(/download directory|\u4e0b\u8f7d\u76ee\u5f55/);
    }
  });
});
