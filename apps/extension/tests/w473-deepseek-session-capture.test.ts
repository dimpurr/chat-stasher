/**
 * W473 · Fixture-backed contract probe for DeepSeek's current conversation
 * response. This is the same-origin GET used for the open session; the separate
 * fetch_page list response is not a conversation capture. Every value below is
 * synthetic and bounded. No network request, account data, or conversation text
 * is used.
 *
 * 🔴 What makes this a pin rather than a mirror: each case fails against the
 *    broad row this probe replaced (POST, a mismatched nested id, the list-route
 *    shape, and an absent nested field all passed it). A row that ever loosens
 *    back to the old hints stops passing these.
 */

import { describe, expect, it } from 'vitest';
import {
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  platformForTraffic,
  PLATFORMS,
} from '../lib/contract';

const ORIGIN = 'https://chat.deepseek.com';
const SESSION_ID = 'synthetic-session-473';
const OTHER_SESSION_ID = 'synthetic-session-474';
const CURRENT_URL = `${ORIGIN}/api/v0/chat/history_messages?chat_session_id=${SESSION_ID}`;
const CAPTURED_AT = 1_798_761_600_000;

function conversationBody(sessionId = SESSION_ID): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_code: 0,
      biz_msg: 'ok',
      biz_data: {
        chat_session: { id: sessionId, current_message_id: 2 },
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER', fragments: [{ content: 'synthetic prompt' }] },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', fragments: [{ content: 'synthetic reply' }] },
        ],
      },
    },
  });
}

function deepseek() {
  return PLATFORMS.find((candidate) => candidate.id === 'deepseek')!;
}

function capture(url: string, text: string, method = 'GET') {
  return { url, method, status: 200, text, capturedAt: CAPTURED_AT };
}

/** The same body with a field removed, so a missing nested value can be probed. */
function bodyWithout(path: string[]): string {
  const body = JSON.parse(conversationBody()) as Record<string, unknown>;
  let current: Record<string, unknown> = body;
  for (const part of path.slice(0, -1)) {
    current = current[part] as Record<string, unknown>;
  }
  delete current[path.at(-1)!];
  return JSON.stringify(body);
}

describe('W473 · DeepSeek current-session capture contract', () => {
  it('accepts the same-origin GET, extracts its session id, and requires the nested body fields', () => {
    const platform = platformForTraffic(CURRENT_URL, 'GET');
    expect(platform?.id).toBe('deepseek');
    expect(platformForTraffic(CURRENT_URL.replace(ORIGIN, 'https://elsewhere.example'), 'GET')).toBeNull();
    expect(platformForTraffic(CURRENT_URL, 'POST')).toBeNull();
    expect(extractSessionId(CURRENT_URL, conversationBody())).toBe(SESSION_ID);

    const body = conversationBody();
    expect(matchesResponseShape(deepseek(), body)).toBe(true);
    expect(isCapturedFetchShape(capture(CURRENT_URL, body))).toBe(true);
  });

  it('refuses a response whose nested session id differs from the requested session', () => {
    // The body is well-formed and names a session — just not the one the request
    // asked for. A capture filed under the request's id would be another
    // conversation's bytes, so the mismatch is not a capture.
    expect(isCapturedFetchShape(capture(CURRENT_URL, conversationBody(OTHER_SESSION_ID)))).toBe(false);
    // The shape gate alone cannot see the binding; that is why the refusal lives
    // in isCapturedFetchShape and this case pins it there.
    expect(matchesResponseShape(deepseek(), conversationBody(OTHER_SESSION_ID))).toBe(true);
  });

  it('refuses a current-session candidate with a missing nested session id', () => {
    const text = bodyWithout(['data', 'biz_data', 'chat_session', 'id']);
    expect(matchesResponseShape(deepseek(), text)).toBe(false);
    expect(isCapturedFetchShape(capture(CURRENT_URL, text))).toBe(false);
  });

  it('refuses a current-session candidate with no nested message collection', () => {
    const missing = bodyWithout(['data', 'biz_data', 'chat_messages']);
    expect(matchesResponseShape(deepseek(), missing)).toBe(false);
    expect(isCapturedFetchShape(capture(CURRENT_URL, missing))).toBe(false);
    // A present-but-not-a-collection value is a changed response, never a capture.
    const body = JSON.parse(conversationBody()) as Record<string, unknown>;
    const bizData = (body.data as Record<string, unknown>).biz_data as Record<string, unknown>;
    bizData.chat_messages = 'not-a-collection';
    const nonArray = JSON.stringify(body);
    expect(matchesResponseShape(deepseek(), nonArray)).toBe(false);
    expect(isCapturedFetchShape(capture(CURRENT_URL, nonArray))).toBe(false);
  });

  it('accepts an empty message collection: a count of zero is a measurement, not a refusal', () => {
    const body = JSON.parse(conversationBody()) as Record<string, unknown>;
    const bizData = (body.data as Record<string, unknown>).biz_data as Record<string, unknown>;
    bizData.chat_messages = [];
    const text = JSON.stringify(body);
    expect(matchesResponseShape(deepseek(), text)).toBe(true);
    expect(isCapturedFetchShape(capture(CURRENT_URL, text))).toBe(true);
  });

  it('does not capture the nearby session-list route as a conversation', () => {
    const listUrl = `${ORIGIN}/api/v0/chat_session/fetch_page?count=20`;
    expect(platformForTraffic(listUrl, 'GET')).toBeNull();
    expect(isCapturedFetchShape(capture(listUrl, conversationBody()))).toBe(false);
  });

  it('does not capture a history_messages request that names no session', () => {
    // Same path, no chat_session_id query: nothing names which conversation this
    // is, so there is no id to bind the body to and no file to file it under.
    const bare = `${ORIGIN}/api/v0/chat/history_messages`;
    expect(platformForTraffic(bare, 'GET')?.id).toBe('deepseek');
    expect(isCapturedFetchShape(capture(bare, conversationBody()))).toBe(false);
  });
});
