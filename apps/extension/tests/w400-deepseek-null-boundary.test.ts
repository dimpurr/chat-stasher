/**
 * W400 · A null parent at a possible response boundary does not make missing
 * evidence of the current branch complete.
 *
 * This is synthetic and exercises only the measured contract: the visible
 * branch must contain `current_message_id`, and a `null` parent terminates the
 * chain that is present. It does not establish whether a server can truncate a
 * body and rewrite that boundary link to `null`; that residual remains open.
 */

import { describe, expect, it } from 'vitest';
import {
  parseDeepSeekDetailPage,
  parseDeepSeekDetailTree,
} from '../lib/backfill/enumerate';

function possibleBoundaryBody(): string {
  return JSON.stringify({
    code: 0,
    msg: 'ok',
    data: {
      biz_code: 0,
      biz_msg: 'ok',
      biz_data: {
        chat_session: { id: 'synthetic-w400', current_message_id: 4 },
        // The response ends at a null-parent message, but the named current
        // message is absent. The null link alone cannot supply that evidence.
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER' },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT' },
        ],
      },
    },
  });
}

describe('W400 · DeepSeek null-parent possible boundary', () => {
  it('keeps a response with a missing current message incomplete despite a null parent', () => {
    const response = possibleBoundaryBody();

    expect(parseDeepSeekDetailTree(response)).toEqual({
      ok: false,
      kind: 'incomplete',
      detail: 'the current message is not among the chat messages this response carries',
    });
    expect(parseDeepSeekDetailPage(response)).toEqual({
      ok: true,
      outcome: 'detail-tree-incomplete',
    });
  });
});
