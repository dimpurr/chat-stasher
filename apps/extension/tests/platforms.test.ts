import { describe, expect, it } from 'vitest';
import {
  CONTENT_MATCHES,
  extractSessionId,
  isCapturedFetchShape,
  matchesResponseShape,
  PLATFORMS,
  platformForTraffic,
} from '../lib/contract';

describe('data-driven platform contracts', () => {
  it('keeps content-script matches closed to explicit origins', () => {
    expect(CONTENT_MATCHES).toEqual([
      'https://chat.deepseek.com/*',
      'https://chatgpt.com/*',
      'https://chat.openai.com/*',
      'https://gemini.google.com/*',
      'https://claude.ai/*',
    ]);
    expect(CONTENT_MATCHES).not.toContain('<all_urls>');
  });

  it('uses the table for URL, method, and response-shape matching', () => {
    expect(platformForTraffic(
      'https://chatgpt.com/backend-api/conversation/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
      'GET',
    )?.id).toBe('chatgpt');
    expect(platformForTraffic(
      'https://chatgpt.com/backend-api/conversation/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
      'POST',
    )).toBeNull();

    const chatGpt = PLATFORMS.find((platform) => platform.id === 'chatgpt')!;
    expect(matchesResponseShape(chatGpt, JSON.stringify({ mapping: {}, current_node: 'root' }))).toBe(true);
    expect(matchesResponseShape(chatGpt, JSON.stringify({ mapping: {} }))).toBe(false);

    const gemini = PLATFORMS.find((platform) => platform.id === 'gemini')!;
    expect(matchesResponseShape(gemini, '[["wrb.fr","hNvQHb","synthetic"]]')).toBe(true);
    expect(matchesResponseShape(gemini, '[["wrb.fr","MaZiqc","synthetic"]]')).toBe(false);
  });

  it('extracts ids from table-declared URL patterns', () => {
    expect(extractSessionId(
      'https://chatgpt.com/backend-api/conversation/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
      JSON.stringify({ mapping: {}, current_node: 'root' }),
    )).toBe('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa');
    expect(extractSessionId(
      'https://gemini.google.com/_/BardChatUi/data/batchexecute',
      '[["wrb.fr","hNvQHb","synthetic"]]',
      'https://gemini.google.com/app/synthetic-session-1234',
    )).toBe('synthetic-session-1234');
  });

  it('validates source-shaped captures without live login', () => {
    expect(isCapturedFetchShape({
      url: 'https://chatgpt.com/backend-api/conversation/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
      method: 'GET',
      status: 200,
      text: JSON.stringify({ mapping: {}, current_node: 'root' }),
      capturedAt: Date.now(),
    })).toBe(true);
  });
});
