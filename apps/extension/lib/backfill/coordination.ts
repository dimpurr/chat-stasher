import {
  BACKFILL_PLANS,
  backfillPlanFor,
  detailPathMatches,
  formSegmentFor,
  type BackfillEnumPlan,
} from './enumerate';

/** Every plan, in the same order as the production registry, for coverage checks. */
export function backfillPlansForCoordination(): readonly BackfillEnumPlan[] {
  return BACKFILL_PLANS;
}

function templatePathMatches(template: string, pathname: string): boolean {
  const expected = template.split('/');
  const actual = pathname.split('/');
  return expected.length === actual.length && expected.every((part, index) =>
    (part.startsWith('{') && part.endsWith('}'))
      ? (actual[index]?.length ?? 0) > 0
      : part === actual[index]);
}

/** Classify only requests declared by that platform's plan; unknown routes fail closed. */
export function coordinationSegmentForRequest(platform: string, url: string): 'enumerate' | 'detail' {
  const parsed = new URL(url);
  const plan = backfillPlanFor(platform);
  if (!plan) throw new Error(`no backfill request plan for ${platform}`);
  const viaForm = formSegmentFor(plan, parsed);
  if (viaForm === 'detail') return 'detail';
  if (viaForm === 'list') return 'enumerate';
  if (plan.listPath === plan.detailPath && parsed.pathname === plan.listPath
      && (plan.listTokenForm || plan.detailForm)) {
    throw new Error(`request query is not declared by the ${platform} backfill plan`);
  }
  const scopePaths = plan.scopeInPath;
  if (scopePaths && parsed.pathname === scopePaths.resolvePath) return 'enumerate';
  if (scopePaths && templatePathMatches(scopePaths.listPath, parsed.pathname)) return 'enumerate';
  if (parsed.pathname === plan.listPath) return 'enumerate';
  if (scopePaths && templatePathMatches(scopePaths.detailPath, parsed.pathname)) return 'detail';
  if (plan.detailPath !== null && detailPathMatches(plan.detailPath, parsed.pathname)) return 'detail';
  if (plan.detailStep2 && detailPathMatches(plan.detailStep2.path, parsed.pathname)) return 'detail';
  throw new Error(`request is not declared by the ${platform} backfill plan`);
}
