import { describe, it, expect } from 'vitest';
import {
  SCHEMA,
  SCHEMA_V1,
  IDENTITY_LEVEL_ORDER,
  extractIdentity,
  type InboxIdentity,
} from '../lib/contract';

const SESSION_ID = 'a1b2c3d4-0000-0000-0000-000000000000';
const jsonOf = (body: unknown): string => JSON.stringify(body);

describe('schema versioning', () => {
  it('@2 is the producer schema; @1 kept only for CLI recognition', () => {
    expect(SCHEMA).toBe('chat-stasher/inbox@2');
    expect(SCHEMA_V1).toBe('chat-stasher/inbox@1');
  });
});

describe('extractIdentity — ADR-002 account axis fallback chain', () => {
  it('chain order is platform_uid > email > handle > default', () => {
    expect(IDENTITY_LEVEL_ORDER).toEqual(['platform_uid', 'email', 'handle', 'default']);
  });

  it('case A: identity present (numeric user_id on data) -> platform_uid', () => {
    const body = {
      code: 0,
      data: { user_id: 820123456789, session_id: SESSION_ID, title: 'hello' },
    };
    const identity: InboxIdentity = extractIdentity(jsonOf(body), SESSION_ID);
    expect(identity).toEqual({ level: 'platform_uid', value: '820123456789' });
  });

  it('case A2: string userId form -> platform_uid', () => {
    const identity = extractIdentity(jsonOf({ data: { userId: 'us_9100000000000000' } }));
    expect(identity).toEqual({ level: 'platform_uid', value: 'us_9100000000000000' });
  });

  it('email beats handle', () => {
    const identity = extractIdentity(
      jsonOf({ data: { email: 'dim@example.com', username: 'dim' } }),
    );
    expect(identity).toEqual({ level: 'email', value: 'dim@example.com' });
  });

  it('handle is used only when nothing more stable exists', () => {
    const identity = extractIdentity(jsonOf({ data: { username: 'dim p' } }));
    expect(identity).toEqual({ level: 'handle', value: 'dim p' });
  });

  it('case B: no identity, only session fields -> default (value "")', () => {
    const body = { code: 0, data: { session_id: SESSION_ID, title: 'apples' } };
    const identity = extractIdentity(jsonOf(body), SESSION_ID);
    expect(identity).toEqual({ level: 'default', value: '' });
  });

  it('the session id under a user-looking key is never the account id', () => {
    const identity = extractIdentity(jsonOf({ data: { user_id: SESSION_ID } }), SESSION_ID);
    expect(identity.level).toBe('default');
  });

  it('an email value under a uid key is not harvested as platform_uid', () => {
    const identity = extractIdentity(jsonOf({ data: { user_id: 'someone@example.com' } }));
    expect(identity.level).toBe('default');
  });

  it('non-JSON body -> default', () => {
    expect(extractIdentity('[not json at all')).toEqual({ level: 'default', value: '' });
  });

  it('no body object at all -> default', () => {
    expect(extractIdentity('"just a string"')).toEqual({ level: 'default', value: '' });
  });
});