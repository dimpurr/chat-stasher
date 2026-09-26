/** Classify a backfill URL for the native host's shared request budget. */
export function coordinationSegmentForRequest(url: string): 'enumerate' | 'detail' {
  const path = new URL(url).pathname;
  if (/\/(?:history_messages|fetch_content)(?:\/|$)/i.test(path)) return 'detail';
  if (/\/chat_conversations\/[^/]+\/?$/i.test(path)) return 'detail';
  if (/\/conversation\/[^/]+\/?$/i.test(path) && !/\/conversations(?:\/|$)/i.test(path)) return 'detail';
  return 'enumerate';
}
