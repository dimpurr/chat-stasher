import { GEMINI_RPC_DETAIL } from '../gemini-rpc';

/** Classify a backfill URL for the native host's shared request budget. */
export function coordinationSegmentForRequest(url: string): 'enumerate' | 'detail' {
  const parsed = new URL(url);
  const path = parsed.pathname;
  if (/\/(?:history_messages|fetch_content)(?:\/|$)/i.test(path)) return 'detail';
  if (/\/chat_conversations\/[^/]+\/?$/i.test(path)) return 'detail';
  if (/\/rest\/thread\/[^/]+\/?$/i.test(path)) return 'detail';
  if (/\/rest\/app-chat\/conversations\/[^/]+\/response-node\/?$/i.test(path)) return 'detail';
  if (/\/apiv2\/kimi\.gateway\.chat\.v1\.ChatService\/ListMessages$/i.test(path)) return 'detail';
  if (/\/conversation\/[^/]+\/?$/i.test(path) && !/\/conversations(?:\/|$)/i.test(path)) return 'detail';
  if (parsed.searchParams.get('rpcids') === GEMINI_RPC_DETAIL) return 'detail';
  return 'enumerate';
}
