/**
 * Shared contracts between the three must be one file with no imports,
 * because the MAIN-world script is bundled without extension APIs.
 */

/** Message names used by the page-world hook and the isolated bridge. */
export const CAPTURE_MESSAGE = '__chat_stasher_capture__';
export const MAIN_READY_MESSAGE = '__chat_stasher_main_ready__';
export const MAIN_PROBE_MESSAGE = '__chat_stasher_main_probe__';
/**
 * Page-world signal for an OBSERVED WebSocket frame. Deliberately a separate
 * name from CAPTURE_MESSAGE: observation is not capture, nothing downstream
 * saves it yet, and the bridge must not mistake one for the other.
 */
export const WS_OBSERVED_MESSAGE = '__chat_stasher_ws_observed__';

/**
 * 🔴 W29 · **Gemini's bootstrap tokens, pulled from the page world when a request
 * needs them.**
 *
 * Why a pull message instead of reading them ourselves: the tokens live in the
 * page's `WIZ_global_data`, and a content script runs in its own JS context where
 * a page global is not visible at all. The MAIN-world hook is the only code we
 * run that can see it, so it is the code that answers this.
 *
 * 🔴 What crosses, and what that costs, stated plainly: the reply carries the
 *    same values that sit in `window.WIZ_global_data`, and **every script on that
 *    page can already read them directly**. So this channel discloses nothing to
 *    the page that the page does not already hold — that is the whole argument for
 *    it existing, and it is why it is a channel and not a hole.
 * 🔴 Neither message is a capture: nothing from them is stored, logged, or sent to
 *    the native host, the request is answered only on the origin whose own row
 *    needs it, and the values are attached to one request each
 *    (lib/platform-auth.ts, `createGeminiAuthorizedFetch`).
 */
export const GEMINI_TOKENS_REQUEST_MESSAGE = '__chat_stasher_gemini_tokens_request__';
export const GEMINI_TOKENS_REPLY_MESSAGE = '__chat_stasher_gemini_tokens_reply__';

/**
 * The page's bootstrap blob and the three values read out of it.
 *
 * 🔴 Names, not values: nothing from those slots is ever written into source,
 *    storage, a log or a message to the native host.
 */
export const GEMINI_WIZ_GLOBAL_DATA_KEY = 'WIZ_global_data';
export const GEMINI_AT_KEY = 'SNlM0e';
export const GEMINI_BL_KEY = 'cfb2h';
export const GEMINI_SESSION_ID_KEY = 'FdrFJe';

/** The one origin whose page the token pull above is answered on. Same closed set as the platform row. */
export const GEMINI_ORIGIN = 'https://gemini.google.com';

/**
 * The three values, named exactly as the page's own blob names them so that
 * "where did this come from" is answerable without a second lookup.
 *
 * Every one of them may be null: absent, or present with a type this code will
 * not use. `null` is not "empty" — it means the request goes out without that
 * value and the platform's own answer is what the caller sees.
 */
export interface GeminiBootstrapTokens {
  /** `WIZ_global_data.SNlM0e` — the XSRF token the request's `at` field carries. */
  at: string | null;
  /** `WIZ_global_data.cfb2h` — the backend release label the `bl` query parameter carries. */
  bl: string | null;
  /** `WIZ_global_data.FdrFJe` — the front-end session id the `f.sid` query parameter carries. */
  fSid: string | null;
}

/** The reply as the ISOLATED side reads it: a recognisable shape, or nothing usable. */
export function isGeminiTokensReply(value: unknown): value is { type: string } & GeminiBootstrapTokens {
  if (!isRecord(value)) return false;
  if (value.type !== GEMINI_TOKENS_REPLY_MESSAGE) return false;
  for (const key of ['at', 'bl', 'fSid']) {
    const field = value[key];
    if (field !== null && typeof field !== 'string') return false;
  }
  return true;
}

/** Shared page-world marker: both injection paths consult the same state. */
export const PAGE_HOOK_VERSION = 'v1';
export const PAGE_HOOK_STATE_KEY = '__chat_stasher_fetch_hook_state__';
export const PAGE_HOOK_FETCH_MARKER = '__chat_stasher_fetch_hook_marker__';

/** Capability wait: short enough to precede normal app traffic, no browser sniffing. */
export const MAIN_FALLBACK_TIMEOUT_MS = 100;

/** Open string: adding a platform must not require a type/logic edit. */
export type PlatformId = string;
export type CaptureConfidence = 'from-source' | 'unverified';

export interface ResponseShape {
  encoding: 'json' | 'text';
  /** Every listed path must be present for JSON responses. */
  requiredPaths?: readonly string[];
  /** At least one listed path must be present for JSON responses. */
  requiredAnyPaths?: readonly string[];
  /** Every listed marker must be present for text responses. */
  requiredTextIncludes?: readonly string[];
}

/** Data-only description of one capturable platform. */
export interface ChatPlatform {
  id: PlatformId;
  /** Exact page/API origins. Never <all_urls>. */
  origins: readonly string[];
  pathHints: readonly string[];
  methods: readonly string[];
  status: { min: number; max: number };
  responseShape: ResponseShape;
  /** Regex source strings; the first capture group is the session id. */
  sessionIdPatterns: readonly string[];
  /** Source-backed is not the same as live verified. */
  credibility: CaptureConfidence;
  /**
   * Opt-in, per row: "this platform carries conversation data over WebSocket".
   * Absent/false (the default for every row shipped today) means the MAIN-world
   * WebSocket wrapper observes NOTHING on that origin. Turning it on is how a
   * future task onboards a WS platform; it is not something to flip casually,
   * because it is the only switch that makes us read frame payloads at all.
   */
  webSocketCapture?: boolean;
}

/**
 * The platform table. Adding support means adding one row of data; the hook,
 * bridge, validator, and saver all consume these generic fields.
 */
export const PLATFORMS: readonly ChatPlatform[] = [
  {
    id: 'deepseek',
    origins: ['https://chat.deepseek.com'],
    pathHints: ['/api/v0/chat', '/chat/session'],
    methods: ['GET', 'POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      requiredAnyPaths: [
        // Live shape of GET /api/v0/chat/history_messages, observed in a
        // logged-in session on 2026-09-13 (key names only, loaded over XHR):
        // { code, msg, data: { biz_code, biz_msg, biz_data: { chat_session: {
        // id, ... }, chat_messages: [...], cache_control, cache_reset_at } } }.
        // None of the older paths below match it; they are kept for the other
        // endpoints and earlier shapes this row has matched.
        'data.biz_data.chat_messages',
        'data.biz_data.chat_session.id',
        'session_id',
        'sessionId',
        'data.session_id',
        'data.sessionId',
        'data.chat_session_id',
        'data.messages',
        'messages',
      ],
    },
    sessionIdPatterns: [
      '/chat/session/([0-9a-fA-F-]{8,})',
      '[?&]chat_session_id=([^&]+)',
    ],
    // External source evidence checked 2026-08-17 (source code, not README), from
    // THREE independent reference implementations. No project name, licence
    // identifier, commit hash or URL is recorded here on purpose: the public
    // surface of this repository does not name third-party exporters.
    //  · The first (2026-08-14) uses https://chat.deepseek.com plus
    //    /api/v0/chat/history_messages and /api/v0/chat_session/fetch_page, and
    //    requires chat_sessions/chat_messages in the decoded business data.
    //  · A second (2026-08-15) independently uses /api/v0/chat/history_messages,
    //    /chat/s/<id>, role/fragments and non-empty content for export.
    //  · A third (2026-04-05) independently identifies non-empty DeepSeek DOM
    //    message nodes.
    // The external API route/shape differences may represent different entry
    // points or versions; this task changes credibility only, not match data.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'perplexity',
    origins: ['https://www.perplexity.ai'],
    // 🔴 W28 · The route this row registers is now the CONVERSATION-CONTENT one,
    // and the conversation-LIST route ('/rest/thread/list_ask_threads') is
    // deliberately outside it. That reverses C27, and the reason C27 gave no
    // longer holds: C27 registered the list route because "the body path has no
    // source" and must not be loosened into a prefix. The body path now has a
    // source (four independent reference implementations and the endpoint table
    // extracted from the site's own front-end bundle; checked 2026-08-17), so the
    // route that carries a conversation is the one worth capturing — exactly the
    // call the kimi row makes when it keeps the message route and lets the
    // conversation-INDEX route fall outside.
    // Not a prefix by accident: '/rest/thread/' also covers the named sibling
    // routes the page posts to (list, mark-viewed, set-title, delete), and those
    // are not conversation data. What separates them is the METHOD below — the
    // content route is the GET under this prefix, and the siblings are POSTs —
    // which is why `methods` is exactly ['GET'] and why the shape gate below can
    // be strict without the list request ever reaching it.
    pathHints: ['/rest/thread/'],
    // Measured route, and the only method the content route uses: the page GETs
    // /rest/thread/<entry_uuid_or_slug> with the thread's own id in the path. The
    // list (POST) is out of scope for capture; the backfill leg reaches it through
    // its own allowlist (lib/backfill/tab-port.ts), not through this row.
    methods: ['GET'],
    status: { min: 200, max: 299 },
    // 🔴 The one field the content response is keyed by in every source:
    // { entries: [{ uuid, query_str, blocks, updated_datetime, thread_title }] }.
    // Required (not "any of"): on this route a body without `entries` is the drift
    // case, so it must fail the shape gate and get warned about rather than pass
    // through as an empty-looking capture. An EMPTY array passes — `[]` is a
    // measurement, not a missing field. One source spells the same array
    // `messages` as a fallback; that name reaches one source only, so it is not
    // required here (naming both would accept a body neither source agrees on).
    responseShape: { encoding: 'json', requiredPaths: ['entries'] },
    // 🔴 Where the session id comes from, and why this order.
    // The thread's identity on the wire is the SLUG: the page URL is
    // /search/<slug>, the list response carries `slug` on each record, and the
    // content route accepts slug or uuid but is fed the slug first. The live leg
    // has to name a file by the value the backfill list would give that same
    // thread, or the same conversation lands under two names — so the page URL is
    // tried first: it is the only place where the value is unambiguously the slug.
    // A prefetched request for a different thread would then be named after the
    // address bar; that is the same trade the kimi row takes, and it is preferred
    // here to the alternative failure, where the same thread is filed twice.
    // The second pattern is the content URL itself. Its path parameter is named
    // `entry_uuid_or_slug`, so a value read there may be either shape; it is the
    // fallback for a page whose URL carries no slug yet (a brand-new thread) and
    // for a capture with no page URL at all. Its lookahead is what keeps the
    // named sibling routes from being read as ids: without it the list request
    // would yield the string 'list_ask_threads' and a list response would be
    // filed as if it were a conversation. A negative list is a closed set of
    // known route names, so a NEW sibling route would have to be added here; the
    // method gate above is the primary defence and this is the second.
    sessionIdPatterns: [
      '/search/([^/?#]+)',
      '/rest/thread/(?!(?:list_ask_threads|list_recent|list_threads|mark_viewed|set_thread_title|delete_thread_by_entry_uuid)(?:[/?#]|$))([^/?#]+)',
    ],
    // R26 cross-check (2026-08-17) supplied the list route and its request shape;
    // W28 (2026-09-14) read the content route out of four independent reference
    // implementations and the site's own extracted endpoint table. Still
    // 'from-source': nobody on either change opened a logged-in perplexity.ai
    // page, so the capture path is source-backed, never live-verified. If the
    // real route or envelope differs, the shape gate above rejects it and
    // page-hook.ts warns — it never guesses.
    credibility: 'from-source',
    webSocketCapture: false,
  },
  {
    id: 'chatgpt',
    origins: ['https://chatgpt.com', 'https://chat.openai.com'],
    pathHints: ['/backend-api/conversation/'],
    methods: ['GET'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      requiredPaths: ['mapping', 'current_node'],
    },
    sessionIdPatterns: ['/backend-api/conversation/([0-9a-fA-F-]{8,})'],
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'gemini',
    origins: ['https://gemini.google.com'],
    pathHints: ['/_/BardChatUi/data/batchexecute'],
    methods: ['POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'text',
      requiredTextIncludes: ['wrb.fr', 'hNvQHb'],
    },
    // 🔴 W29 · **Where a capture's identity comes from here, and why this pattern
    //    is not it.** The list endpoint returns conversation ids in the
    //    `c_`-prefixed form (measured 2026-09-14), so a backfill debt key is one,
    //    and a live capture is filed under the same value — read out of the
    //    response's own turns and carried down as the authoritative identity
    //    (lib/gemini-capture.ts). The pattern below reads the **bare** id out of
    //    the page URL (`/app/<id>`), which is the one place the prefix is absent,
    //    so it would name the same conversation differently. It is kept as the
    //    last-resort fallback every row has and is deliberately **not** what a
    //    capture is filed under: a conversation filed twice under two names is the
    //    failure the canonical form exists to prevent. If a future path ever
    //    reaches it, that is the bug to fix, not the pattern to widen.
    sessionIdPatterns: ['/app/([A-Za-z0-9_-]{8,})'],
    // The measured route, and the only method that carries it: the page POSTs a
    // URL-encoded form to this one path for **two** RPC ids (list and detail),
    // which is why the backfill plan tells its segments apart by the rpcid in the
    // query rather than by path (lib/backfill/enumerate.ts's formSegmentFor).
    // 🔴 W29 · The request also carries three values out of the page's own
    //    `WIZ_global_data` — an XSRF token in the body's `at` field and two
    //    identifiers in the query. They are read through the page-world hook at
    //    request time and attached by lib/platform-auth.ts's
    //    `createGeminiAuthorizedFetch`; no plan-built URL or body carries one, and
    //    the allowlist refuses a message that arrives with one. Without the XSRF
    //    token the server answers HTTP 400 with a structured error entry — a real
    //    refusal, which is why a 400 here is never read as "you have no
    //    conversations".
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'claude',
    origins: ['https://claude.ai'],
    // Deliberately NOT '/api/organizations/' — every org-scoped call (settings,
    // projects, ...) would then become a "conversation data" candidate and the
    // shape-mismatch warning would turn into noise. This hint is the
    // conversation-DETAIL route only. Note it has a trailing slash, so the
    // conversation-LIST route ('/chat_conversations', an array of summaries we
    // do not capture) falls outside and is skipped silently, which is correct:
    // it is not the data we claim to back up.
    // Also deliberately query-free: the two sources below disagree on the
    // casing of the tree flag ('?tree=true' vs '?tree=True').
    pathHints: ['/chat_conversations/'],
    methods: ['GET'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      // Exactly the check the reference implementation performs before it will
      // export.
      // Required (not "any of"): on this route a body without chat_messages is
      // the drift case, so it must fail the shape gate and get warned about
      // rather than pass through as an empty-looking capture.
      requiredPaths: ['chat_messages'],
    },
    sessionIdPatterns: [
      '/chat_conversations/([0-9a-fA-F-]{8,})',
      '/chat/([0-9a-fA-F-]{8,})',
    ],
    // 🔴 The route shape below is SECOND-HAND: it comes from reading public
    // open-source exporters, NOT from a logged-in claude.ai session. Nobody on
    // this change ever opened claude.ai, so this row is source-backed, never
    // live-verified. If the real route or envelope differs, the generic gate
    // above rejects it and page-hook.ts warns — it never guesses.
    //
    // External source evidence checked 2026-08-17 (source code, not README), from
    // three independent reference implementations. No project name, licence
    // identifier, commit hash or URL is recorded here on purpose: the public
    // surface of this repository does not name third-party exporters.
    //  · The first (2026-08-08) requests
    //    /api/organizations/<org>/chat_conversations/<uuid>?tree=true&... and
    //    validates the response with Array.isArray(data.chat_messages), treating
    //    a missing chat_messages as "the endpoint may have changed" rather than
    //    as an empty conversation. Its own notes document the envelope as
    //    { name, model, current_leaf_message_uuid, chat_messages: [{ uuid,
    //    parent_message_uuid, index, sender, created_at, content }] }.
    //  · A second (2026-06-10) independently hooks the same route from a
    //    MAIN-world fetch interceptor, matching on the URL alone
    //    (/api/organizations/<org>/chat_conversations/<uuid> with the tree flag)
    //    and never inspecting the body, so it cannot tell drift from an empty
    //    chat — which is exactly why we add the body gate instead of copying its
    //    approach.
    //  · A third documents the same GET route but ships NO licence, so it was
    //    read for architecture only and no code from it was used.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'kimi',
    // 🔴 DOMAIN: verified against source, not against my own assumptions. The
    // brief suggested 'https://kimi.moonshot.cn'; the CURRENT web app is
    // https://www.kimi.com and that is the only origin here. kimi.moonshot.cn
    // is the LEGACY origin (it still appears in a 2026-02 reverse-API project,
    // paired with a completely different '/api/chat/<id>/...' route family);
    // we have no source-backed evidence that today's page serves the route
    // below from it, so it is deliberately NOT onboarded. Adding it would widen
    // the content-script match set for a guess. One origin, closed set.
    origins: ['https://www.kimi.com'],
    // Deliberately NOT '/apiv2/' or 'ChatService' — every gateway call (send,
    // list, usage, ...) would then become a "conversation data" candidate and
    // the shape-mismatch warning would turn into noise. This hint is the
    // message-LIST-for-one-conversation route only.
    // 🔴 Measured 2026-09-14, this is the call the page makes to load one
    // conversation's messages, and the only one on the origin carrying a
    // conversation. The hint stops at 'ChatService/ListMessages' rather than
    // naming a service package: the measured package is
    // 'kimi.gateway.chat.v1.ChatService' and one external implementation spells
    // the same call 'kimi.chat.v1.ChatService', so the shorter hint matches the
    // call under either spelling and the response-shape gate below is what
    // decides whether a body really is conversation data.
    // The conversation-INDEX route (measured: 'FeedService/ListFeeds', a paged
    // array of feed items whose chat entries carry a NAME and a
    // `messageContent` preview, not messages) falls outside this hint and is
    // skipped silently, which is correct for the capture leg: a preview is not
    // the conversation, and capturing it would file a summary as the data.
    pathHints: ['ChatService/ListMessages'],
    // Connect-style unary RPC: the request is a POST with a JSON body, and the
    // chat id lives in that body (`{ chat_id }`, measured), not in the URL.
    // Hence the page-URL fallback in sessionIdPatterns below.
    methods: ['POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      // Measured 2026-09-14: the response carries `messages` at the top level and
      // nothing beside it that names the conversation. Each message carries
      // `id, parentId, role, status, blocks, scenario, createTime, isGoal`; not
      // one of those is required here, because the gate's job is "is this the
      // messages envelope at all", and pinning a nested key would turn a
      // *message* shape change into a dropped conversation rather than a
      // warning. Required (not "any of"): a body without `messages` is the drift
      // case, so it must fail the shape gate and be warned about rather than
      // pass through as an empty-looking capture.
      requiredPaths: ['messages'],
    },
    // 🔴 Measured 2026-09-14: chat ids appear in `/chat/<id>`, and the ids seen
    // have more than one shape (one hex-like, one alphanumeric), so the character
    // class is deliberately wide and there is **no** minimum length. The cost of
    // that width is stated rather than hidden: a `/chat/<segment>` URL that is not
    // a conversation id would also yield a value, and it becomes the file name
    // fragment. What keeps that from being a way to file one conversation under
    // another name is everything else that has to pass first — same origin, POST,
    // this row's path hint, a 2xx, and a body carrying `messages` — plus the fact
    // that the value comes from the page's own URL, not from the page's message.
    // An over-narrow pattern, by contrast, silently drops real conversations
    // whose ids happen not to match it, and "we did not save it" is worse here
    // than "we saved it under the id in the address bar".
    sessionIdPatterns: ['/chat/([A-Za-z0-9_-]+)'],
    // 🔴 OBSERVED IN A LOGGED-IN SESSION on 2026-09-14 — list + short-chat
    // detail; long-chat paging unverified. Counts and field names only:
    //  · list    POST /apiv2/kimi.gateway.feed.v1.FeedService/ListFeeds, body
    //            `{ page_size, page_token }`. The response is
    //            `{ items, nextPageToken }`, each item
    //            `{ type: 'FEED_TYPE_CHAT', chat: { id, name, messageContent,
    //            createTime, updateTime } }`. page_size 3 returned 3 items and a
    //            non-empty token; the next page returned 2 items with the token
    //            ABSENT and the two pages did not overlap.
    //  · detail  POST /apiv2/kimi.gateway.chat.v1.ChatService/ListMessages, body
    //            `{ chat_id }`. The response carries `messages` at the top level
    //            (the message keys are on the shape gate above). Five
    //            conversations were sampled, each 2-3 messages.
    //  · auth    the page sends `authorization: Bearer <token>` — the JWT this
    //            origin keeps in localStorage under `access_token` — plus
    //            `x-msh-platform: web`, `x-language: <locale>` and
    //            `content-type: application/json`. A COOKIE-ONLY list request
    //            answers HTTP 401 with a body carrying `code` and `details`:
    //            a refusal, not an empty list. Hence "a 401 is an auth halt,
    //            never 'you have no conversations'"; what the backfill leg does
    //            with that is in lib/platform-auth.ts and lib/backfill/engine.ts.
    //  · 🔴 NOT verified, and this row must not be read as if it were: whether a
    //            LONG conversation's detail response pages. Every sampled
    //            conversation was short and carried no page-token field, so the
    //            implementation treats a page-token field appearing as an
    //            incomplete body and refuses to archive it (parseKimiDetailPage,
    //            lib/backfill/enumerate.ts) rather than storing a truncated
    //            conversation as a complete one.
    // The generic gate above is what decides whether a body is conversation data
    // at all: if the envelope differs, it is rejected and page-hook.ts warns —
    // it never guesses. Kimi is also known to run front-end signing/WAF
    // challenges; that affects the page's own requests, not us — we only read
    // what the page already fetched.
    //
    // The external source evidence below was what stood here BEFORE that
    // measurement. It is kept: it is the record of how the row was first
    // written, and it independently corroborates the route above (checked
    // 2026-08-17, source code, not README), from FOUR reference implementations.
    // No project name, licence identifier, commit hash or URL is recorded here on
    // purpose: the public surface of this repository does not name third-party
    // exporters.
    //  · The first (2026-06-06) POSTs JSON to
    //    https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages
    //    with { chatId }, reads `data.messages`, and treats an absent/empty list
    //    as an error rather than as an empty conversation; its own manifest
    //    matches only https://www.kimi.com/* and its page menus key off
    //    https://www.kimi.com/chat/*.
    //  · A second (2026-03-15) independently uses BASE_URL
    //    https://www.kimi.com and a ListMessages call keyed by chat id, reading
    //    `messages` (with `items` / `data.messages` as fallbacks) — note it
    //    spells the service 'kimi.chat.v1.ChatService', which is why the path
    //    hint above stops at 'ChatService/ListMessages'.
    //  · A third (2026-08-02) independently hooks the same
    //    '/apiv2/kimi.gateway.chat.v1.ChatService/' gateway on www.kimi.com from
    //    a MAIN-world fetch interceptor.
    //  · A fourth (2026-05-14) independently hard-codes its API base as
    //    https://www.kimi.com and the same
    //    '/apiv2/kimi.gateway.chat.v1.ChatService/' service prefix.
    // One further reference implementation is GPL-3.0, so it was read for
    // architecture only and no code from it was used; it is cited solely for the
    // fact that the LEGACY origin https://kimi.moonshot.cn used an unrelated
    // '/api/chat/...' route family, which is why that origin is not in `origins`.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    webSocketCapture: false,
  },
  {
    id: 'grok',
    // 🔴 DOMAIN: exactly one origin, and it is the one the sources use. No
    // wildcard, no *.grok.com, no xi.grok.com or any other subdomain — nothing
    // in the reviewed scope serves this route from another host.
    origins: ['https://grok.com'],
    // 🔴 Deliberately NOT '/rest/' or '/app-chat/'. Three endpoints live under
    // this prefix — the conversation list (GET), the message-tree skeleton
    // (GET .../response-node) and the message content (POST .../load-responses)
    // — and only the third one is the data this leg claims to back up. A
    // broader hint would make the list and the skeleton "candidates" whose
    // bodies then fail the shape gate below, turning the shape-mismatch warning
    // into noise for three requests per conversation opening. The narrow hint
    // is also why the skeleton GET is not in `methods`: it carries no content.
    pathHints: ['/load-responses'],
    methods: ['POST'],
    status: { min: 200, max: 299 },
    responseShape: {
      encoding: 'json',
      // Exactly the one field the content-bearing response is keyed by in every
      // source: { responses: [{ responseId, message, sender, createTime,
      // parentResponseId, model, ... }] }. Required (not "any of"): on this route
      // a body without `responses` is the drift case, so it must fail the shape
      // gate and get warned about rather than pass through as an empty-looking
      // capture. An EMPTY array passes — `[]` is a measurement, not a missing
      // field, and that is the difference this row has to keep.
      requiredPaths: ['responses'],
    },
    // The conversation id is a path segment of the API URL, so the API URL alone
    // is enough; the page URL (/c/<id>) is the fallback the other rows use, and
    // it is here because the content response also arrives while the user is on
    // that page. Both patterns are anchored on a path segment, never on a query.
    sessionIdPatterns: [
      '/rest/app-chat/conversations/([^/]+)/load-responses',
      '/c/([A-Za-z0-9_-]{8,})',
    ],
    // 🔴 The route shape below is SOURCE-BACKED but NOT live-verified: it comes
    // from reading public open-source projects and one closed-source store build,
    // NOT from a logged-in grok.com session. Nobody on this change ever opened
    // grok.com, so this row is 'from-source', never 'verified'. If the real route
    // or envelope differs, the generic gate above rejects it and page-hook.ts
    // warns — it never guesses.
    //
    // External source evidence checked 2026-09-14 (source code, not README):
    //  · One reference implementation (MIT) builds the base URL
    //    https://grok.com/rest/, GETs app-chat/conversations?pageSize=<n>[&pageToken=<t>]
    //    for the list, GETs app-chat/conversations/<id>/response-node for the tree
    //    skeleton, and POSTs { responseIds: [...] } to
    //    app-chat/conversations/<id>/load-responses for the content. Its own
    //    type declarations carry the exact response field names used here:
    //    { conversations: [{ conversationId, title, starred, createTime,
    //    modifyTime }], nextPageToken, textSearchMatches } for the list,
    //    { responseNodes: [{ responseId, sender, parentResponseId }] } for the
    //    skeleton, and { responses: [{ responseId, message, sender, createTime,
    //    parentResponseId, model, webSearchResults, citedWebSearchResults,
    //    fileAttachmentAssetMetadata }] } for the content. Its recorded note is
    //    that all three were verified in a logged-in browser session in 2026-07
    //    and then dogfooded against a real account. That is somebody else's
    //    verification, not this repository's.
    //  · A second reference implementation (MIT) independently uses
    //    https://grok.com/rest/app-chat with the same response-node →
    //    load-responses pair and the same { responseIds } POST body, and reads
    //    `conversations[].conversationId` / `responses[].responseId` /
    //    `responses[].message` / `responses[].sender` / `responses[].createTime`.
    //    It disagrees with the first on the LIST cursor (an integer `page` query
    //    instead of `pageToken`) — see GROK_PLAN's provenance; the content
    //    endpoint is the same in both.
    //  · A closed-source unpacked store build was read for architecture only. It
    //    corroborates the pair (.../response-node then a POST to
    //    .../load-responses whose body is exactly { responseIds } built from the
    //    skeleton's ids), and its own URL allow-list names the same two path
    //    shapes. It also shows that the page posts to the same
    //    `/app-chat/conversations` prefix for unrelated operations (title edit,
    //    delete), which is part of why the hint above is the content path itself.
    // No project name, licence identifier or URL is recorded here on purpose: the
    // public surface of this repository does not name third-party exporters.
    credibility: 'from-source',
    // No shipped row observes WebSocket frames. Stated explicitly, not left to
    // the default, so that "did anyone turn this on?" is one grep away.
    // 🔴 Grok is a case where this default was checked rather than assumed: the
    //    content arrives over plain REST (GET skeleton, then POST content), and
    //    the live *streaming* of a reply is a separate transport that no source
    //    in the reviewed scope captures or documents. Recording that as "not
    //    found" is the honest state; turning this switch on would claim we know
    //    how to read those frames, which we do not.
    webSocketCapture: false,
  },
];

/** Content-script matches derived from the table — a closed set. */
export const CONTENT_MATCHES: string[] = Array.from(
  new Set(PLATFORMS.flatMap((platform) => platform.origins.map((origin) => `${origin}/*`))),
);

/** Convenience back-compat alias for the incumbent platform origin. */
export const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
export const CHAT_PATH_HINTS = ['/api/v0/chat', '/chat/session'];

/** Look up a platform by exact origin. */
export function getPlatformByOrigin(origin: string): ChatPlatform | undefined {
  return PLATFORMS.find((platform) => platform.origins.includes(origin));
}

/**
 * Largest response body carried as a conversation. Was 4 MiB ("larger is
 * streamed media"), until a real ChatGPT conversation measured 8.45 MB
 * (2026-09-14) — long conversations are the ones most worth keeping.
 * 16 MiB because the body travels as a string inside a bundle inside the Native
 * Messaging request: double JSON escaping can at worst quadruple a quote-heavy
 * body, and 4 × 16 MiB is the host's 64 MiB request cap. Anything larger is
 * skipped with a visible warning, never silently.
 */
export const MAX_RAW_BYTES = 16 * 1024 * 1024;

export const INBOX_PREFIX = 'chat-stasher/inbox';

export interface CapturedFetch {
  /** Full response URL, e.g. https://chat.deepseek.com/api/v0/chat/... */
  url: string;
  method: string;
  status: number;
  /** Raw response text, passed through untouched so nothing is lost. */
  text: string;
  /** Needed when the provider API URL does not contain the conversation id. */
  pageUrl?: string;
  capturedAt: number;
  /**
   * 🔴 C21 · The **authoritative value** of the conversation's identity, carried
   * down by whichever side already knows who it is.
   *
   * The root cause, revisited: the identity of one conversation used to be
   * expressed twice — the debt key was the list API's items[].id, while the file
   * name was "scraped out of the URL once more". A lossy function sat between the
   * two expressions, so two different debt keys could collapse onto one file name
   * (the later write overwriting the earlier one).
   * This field is the **elimination point** of that second expression: the backfill
   * leg puts the id the enumerator gave it straight in, and the write-down path
   * stops deriving.
   *
   * 🔴 Who may fill it: only the extension itself (lib/backfill/engine.ts).
   *    A payload from a page must **never** carry this field — see
   *    isCapturedFetchShape: being able to specify the identity is the same as
   *    being able to specify which file name it is written to.
   * 🔴 Left undefined ⇒ the old extractSessionId route, byte-for-byte the same
   *    live-leg behaviour.
   */
  sessionId?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object';
}

/**
 * 🔴 W28 · **Own properties only.** The `in` operator walks the prototype chain,
 * and an array inherits a method from `Array.prototype` for every name that
 * collides with one — `entries`, `keys`, `values`, `map`, `find`, `slice`, ...
 * So `'entries' in []` is TRUE and `[]['entries']` is a FUNCTION, which is not
 * null and therefore used to satisfy the shape gate. A top-level array would
 * then pass a `requiredPaths: ['entries']` gate and be captured as if it were
 * the content envelope — the "an unknown recorded as a known" failure this
 * gate exists to prevent, arriving through the one path nobody looks at.
 * The gate asks "did the body NAME this field", so it must ask the object's own
 * keys. Kept in step with the copy in lib/page-hook.ts: the page hook posts a
 * payload iff its copy passes, and the bridge accepts it iff this one does.
 */
function getJsonPath(value: unknown, path: string): unknown {
  let current: unknown = value;
  for (const part of path.split('.')) {
    if (!current || typeof current !== 'object') return undefined;
    if (!Object.prototype.hasOwnProperty.call(current, part)) return undefined;
    current = (current as Record<string, unknown>)[part];
  }
  return current;
}

function hasUsablePath(value: unknown, path: string): boolean {
  const found = getJsonPath(value, path);
  return found !== undefined && found !== null;
}

export function findPlatformForUrl(url: string): ChatPlatform | null {
  try {
    const origin = new URL(url).origin;
    return getPlatformByOrigin(origin) ?? null;
  } catch {
    return null;
  }
}

export function matchesResponseShape(platform: ChatPlatform, text: string): boolean {
  const shape = platform.responseShape;
  if (shape.encoding === 'text') {
    return (shape.requiredTextIncludes ?? []).every((marker) => text.includes(marker));
  }

  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return false;
  }
  const requiredPaths = shape.requiredPaths ?? [];
  const requiredAnyPaths = shape.requiredAnyPaths ?? [];
  return (
    requiredPaths.every((path) => hasUsablePath(body, path)) &&
    (requiredAnyPaths.length === 0 || requiredAnyPaths.some((path) => hasUsablePath(body, path)))
  );
}

/** Validate page-originated capture payloads before they reach extension APIs. */
export function isCapturedFetchShape(value: unknown): value is CapturedFetch {
  if (!isRecord(value)) return false;
  if (typeof value.url !== 'string' || typeof value.method !== 'string') return false;
  const platform = platformForTraffic(value.url, value.method);
  if (!platform) return false;
  if (
    typeof value.status !== 'number' ||
    !Number.isInteger(value.status) ||
    value.status < platform.status.min ||
    value.status > platform.status.max
  ) return false;
  if (typeof value.text !== 'string' || value.text.length === 0) return false;
  if (value.pageUrl !== undefined && typeof value.pageUrl !== 'string') return false;
  // 🔴 C21: `sessionId` is the extension-internal authoritative identity channel
  //    (used by the backfill leg) and it decides the file name directly. A page
  //    being able to fill it = a page being able to choose which file is written
  //    (overwriting another conversation). So this is not "validate it", it is
  //    **reject on sight** — a payload from a page should never have this field at
  //    all.
  if ('sessionId' in value) return false;
  if (typeof value.capturedAt !== 'number' || !Number.isFinite(value.capturedAt) || value.capturedAt <= 0) {
    return false;
  }
  return (
    new TextEncoder().encode(value.text).byteLength <= MAX_RAW_BYTES &&
    matchesResponseShape(platform, value.text)
  );
}

export function isCaptureMessage(
  value: unknown,
): value is { type: typeof CAPTURE_MESSAGE; payload: CapturedFetch } {
  return isRecord(value) && value.type === CAPTURE_MESSAGE && isCapturedFetchShape(value.payload);
}

export function isMainReadyMessage(
  value: unknown,
): value is { type: typeof MAIN_READY_MESSAGE; version: string; token: string } {
  return (
    isRecord(value) &&
    value.type === MAIN_READY_MESSAGE &&
    value.version === PAGE_HOOK_VERSION &&
    typeof value.token === 'string' &&
    value.token.length >= 8
  );
}

/** v1 kept only so the CLI can recognise legacy bundles; producers write @2. */
export const SCHEMA_V1 = 'chat-stasher/inbox@1';
export const SCHEMA = 'chat-stasher/inbox@2';

/**
 * ADR-002 identity axis: the account, not the machine. Two hosts capturing the
 * same session must produce the same identity value so the CLI can dedupe.
 * Degradation chain (weakest protection for the CLI is 'default'):
 *   platform_uid -> email -> handle -> default
 */
export type IdentityLevel = 'platform_uid' | 'email' | 'handle' | 'default';

export interface InboxIdentity {
  /** Where `value` came from. 'default' means "not found, identity unreliable". */
  level: IdentityLevel;
  /** Stable per-account value. '' when level === 'default'. */
  value: string;
}

export interface InboxBundle {
  schema: typeof SCHEMA;
  platform: PlatformId;
  sessionId: string;
  identity: InboxIdentity;
  url: string;
  method: string;
  status: number;
  capturedAt: string;
  /** Parsed-once envelope fields, best-effort. Raw text is authoritative. */
  parsed: {
    hasJson: boolean;
    keys: string[];
  };
  raw: {
    text: string;
    bytes: number;
  };
}

/** READS raw body. After completion the response body is already consumed; use response.clone(). */
export function isChatTraffic(url: string, method: string): boolean {
  return platformForTraffic(url, method) !== null;
}

export function platformForTraffic(url: string, method: string): ChatPlatform | null {
  try {
    const u = new URL(url);
    return PLATFORMS.find(
      (platform) =>
        platform.origins.includes(u.origin) &&
        platform.methods.includes(method.toUpperCase()) &&
        platform.pathHints.some((hint) => u.pathname.includes(hint)),
    ) ?? null;
  } catch {
    return null;
  }
}

/**
 * Extract a session id from URL or parsed body so the inbox file is stable per session.
 * Returns null when no id can be found — such captures are skipped (logged, not saved).
 */
export function extractSessionId(url: string, text: string, pageUrl?: string): string | null {
  const platform = findPlatformForUrl(url) ?? (pageUrl ? findPlatformForUrl(pageUrl) : null);
  if (platform) {
    for (const pattern of platform.sessionIdPatterns) {
      const match = new RegExp(pattern).exec(url) ?? (pageUrl ? new RegExp(pattern).exec(pageUrl) : null);
      if (match?.[1]) {
        try {
          return decodeURIComponent(match[1]);
        } catch {
          return match[1];
        }
      }
    }
  }
  try {
    const obj = JSON.parse(text);
    if (!obj || typeof obj !== 'object') return null;
    const cand =
      obj.session_id ??
      obj.sessionId ??
      obj.data?.session_id ??
      obj.chat_session?.id ??
      obj.meta?.session_id ??
      null;
    if (typeof cand === 'string' && cand.length >= 8) return cand;
  } catch { /* not JSON — not a capturable session envelope */ }
  return null;
}

/** ADR-002 chain order. Unit tests assert this exact order. */
export const IDENTITY_LEVEL_ORDER: readonly IdentityLevel[] = [
  'platform_uid',
  'email',
  'handle',
  'default',
];

/**
 * Resolve the account-stable identity (ADR-002: account axis, not machine) from
 * a captured chat/session response BODY, then fall down the chain. Reads only
 * `text` — never opens new network calls, never touches storage, so this cannot
 * widen the extension's surface beyond the traffic we already hook.
 *
 * HONESTY: the candidate key names below were NOT verified against a real
 * logged-in DeepSeek page during this spike (no live login allowed), so they are
 * conjectured naming variants, not confirmed wire fields. The structural guards
 * (email regex / digit-long-id / not-session-shaped / not-the-session-id itself)
 * make a wrong guess degrade to `default` instead of emitting a bogus id.
 */
export const IDENTITY_KEY_CANDIDATES: Array<{ level: IdentityLevel; keys: string[] }> = [
  { level: 'platform_uid', keys: ['user_id', 'userId', 'uid', 'author_id', 'owner_id', 'creator_id', 'account_id'] },
  { level: 'email', keys: ['email', 'user_email', 'email_address', 'mail'] },
  { level: 'handle', keys: ['username', 'user_name', 'handle', 'nickname', 'display_name', 'name'] },
];

const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const DASHY_RE = /^[0-9a-fA-F-]+$/;
const IDENTIFIER_RE = /^[A-Za-z0-9_\-:.]{6,}$/;

function isSessionShaped(s: string): boolean {
  // A dashed UUID-ish token is session-shaped; a plain digit run is NOT.
  return s.includes('-') && DASHY_RE.test(s);
}

function acceptIdentityValue(level: IdentityLevel, raw: unknown, sessionId: string | null): string | null {
  if (typeof raw === 'number') raw = String(raw);
  if (typeof raw !== 'string') return null;
  const s = raw.trim();
  if (!s || s === 'null' || s === 'undefined' || s.length > 4096) return null;
  // Never let the per-session id itself impersonate an account id.
  if (sessionId && s === sessionId) return null;
  if (level === 'email') {
    return EMAIL_RE.test(s) ? s : null;
  }
  if (level === 'platform_uid') {
    // Dashed/UUID values are session-shaped, not account-shaped.
    if (isSessionShaped(s)) return null;
    if (/^\d+$/.test(s)) return s.length >= 6 ? s : null;
    return IDENTIFIER_RE.test(s) ? s : null;
  }
  // handle: a human-ish name, not an email, not a bare number.
  if (s.length > 64 || EMAIL_RE.test(s) || /^\d+$/.test(s)) return null;
  return s;
}

/** Bounded walk (depth <= 3) over JSON bodies looking for the candidate keys. */
function collectFirst(node: unknown, keys: ReadonlyArray<string>, out: unknown[], depth: number): void {
  if (depth > 3 || out.length) return;
  if (Array.isArray(node)) {
    for (const item of node) collectFirst(item, keys, out, depth + 1);
    return;
  }
  if (!node || typeof node !== 'object') return;
  for (const [key, val] of Object.entries(node as Record<string, unknown>)) {
    if (!keys.includes(key)) {
      collectFirst(val, keys, out, depth + 1);
    } else if (typeof val === 'string' || typeof val === 'number') {
      out.push(val);
    }
  }
}

export function extractIdentity(text: string, sessionId: string | null = null): InboxIdentity {
  let body: unknown = null;
  try {
    body = JSON.parse(text);
  } catch { /* not JSON → no account fields to read → default */ }
  if (!body || typeof body !== 'object') return { level: 'default', value: '' };
  for (const group of IDENTITY_KEY_CANDIDATES) {
    const found: unknown[] = [];
    collectFirst(body, group.keys, found, 0);
    for (const raw of found) {
      const accepted = acceptIdentityValue(group.level, raw, sessionId);
      if (accepted !== null) return { level: group.level, value: accepted };
    }
  }
  return { level: 'default', value: '' };
}

export function sanitizePathSegment(s: string): string {
  return s.replace(/[\x00-\x1f/\\:*?"<>| ]/g, '_');
}

/**
 * 🔴 C21 · conversation id → file-name fragment, which **must be injective** (two
 * different ids can never produce the same fragment).
 *
 * Why not "replace the unsafe characters with _ and be done": `sanitizePathSegment`
 * is **many-to-one** ('a b' and 'a/b' both become 'a_b'), and lib/download.ts used
 * conflictAction:'overwrite' — two different conversations collapse onto one name
 * and the later write **wipes the earlier one off disk**.
 *
 * What is chosen here is [identity + refusal], not [escaping]:
 *  · the fragment is safe (sanitize is a no-op) ⇒ returned unchanged. On this
 *    domain the naming function is the identity map, so injectivity is
 *    **structural** and depends on no detail of any character table.
 *  · the fragment is unsafe ⇒ null, and the caller must treat it as a **traced
 *    failure**; never force a name through.
 *
 * 🔴 Why not escaping (e.g. '_'→'_5f', ' '→'_20'): that would rename every one of
 *    **existing users'** conversations containing '_' (gemini / kimi ids use the
 *    character set [A-Za-z0-9_-]), which amounts to re-downloading the whole
 *    inbox. The identity scheme leaves every existing, path-safe id
 *    **byte-for-byte unchanged** — so the live leg's existing behaviour is
 *    preserved character for character.
 */
export function pathSafeSessionId(id: string): string | null {
  if (!id) return null;
  // Identity is injective: accept only ids that already work as a file name as-is.
  if (sanitizePathSegment(id) !== id) return null;
  // '.'/'..' would be read as a path rather than a name, and a leading '.' should not appear in the inbox either.
  if (id.startsWith('.')) return null;
  return id;
}
