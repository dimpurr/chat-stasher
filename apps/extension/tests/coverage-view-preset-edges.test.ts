import { beforeEach, describe, expect, it, vi } from 'vitest';
import { buildCoverage, localMonthKey } from '../lib/coverage';
import { coverageView } from '../lib/coverage-view';
import { SPEED_PLANS, SPEED_PRESET_ORDER, type SpeedPreset } from '../lib/backfill/speed';
import { renderMessage, setUiLocale } from '../lib/i18n';
import { CATALOGS, withI18n, type TestLocale } from './i18n-harness';

const NOW = Date.UTC(2026, 8, 24, 12, 0, 0);

/**
 * The wording one preset must show in one locale, read out of the real catalog
 * rather than retyped: the harness compiles the shipped yml through the
 * package's own compiler, so the expectation cannot drift from what the
 * extension says, and this file stays free of locale literals (T5). The `what`
 * template is rendered with the plan's own numbers — the same substitution
 * `presetWhat` performs, so a mismatch in either side names itself.
 */
function expectedWording(locale: TestLocale, preset: SpeedPreset): { label: string; what: string; risk: string | null } {
  const catalog = CATALOGS[locale];
  const plan = SPEED_PLANS[preset];
  return {
    label: catalog[`coverage_preset_${preset}`]!.message,
    what: renderMessage(catalog.coverage_preset_what!.message, [{
      cap: plan.pace.detail.maxPerDay ?? 0,
      tick: plan.tickDetails,
    }]),
    risk: plan.carriesRisk ? catalog.coverage_preset_risky!.message : null,
  };
}

function reportFor(presetRaw: unknown) {
  return buildCoverage({
    scopes: [],
    enabled: true,
    hostPaused: false,
    presetRaw,
    tick: null,
    now: NOW,
    monthKey: localMonthKey,
  });
}

beforeEach(async () => {
  vi.stubGlobal('browser', withI18n({} as never));
  await setUiLocale('en');
});

describe('coverage speed preset wording', () => {
  for (const locale of ['en', 'zh_CN'] as const) {
    it(`${locale} exposes a translated label and explanation for each supported preset`, async () => {
      vi.stubGlobal('browser', withI18n({} as never, locale));
      await setUiLocale(locale);

      const view = coverageView(reportFor('gentle'), NOW);
      expect(view.speed.options.map(({ preset, label, what }) => ({ preset, label, what }))).toEqual(
        SPEED_PRESET_ORDER.map((preset) => ({ preset, ...expectedWording(locale, preset) })).map(({ risk: _risk, ...option }) => option),
      );

      for (const preset of SPEED_PRESET_ORDER) {
        const selected = coverageView(reportFor(preset), NOW).speed;
        expect(selected.current).toBe(preset);
        expect(selected.riskNote).toBe(expectedWording(locale, preset).risk);
      }
    });
  }

  it('an unknown runtime preset stays safe and does not claim a risk level', () => {
    const report = { ...reportFor('gentle'), preset: 'future-preset' } as unknown as ReturnType<typeof reportFor>;
    const view = coverageView(report, NOW);

    expect(view.speed.current).toBe('gentle');
    expect(view.speed.options.map((option) => option.preset)).toEqual(['gentle', 'standard', 'faster']);
    expect(view.speed.riskNote).toBeNull();
  });
});
