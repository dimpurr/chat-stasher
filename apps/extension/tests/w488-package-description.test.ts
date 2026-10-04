import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = new URL('..', import.meta.url).pathname;

describe('extension package description', () => {
  it('describes supported chat capture and local host delivery without promising completeness', () => {
    const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8')) as {
      description?: unknown;
    };

    expect(pkg.description).toBe(
      'Capture chats from supported web apps and deliver them to the local Chat Stasher host.',
    );
  });
});
