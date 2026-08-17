import {
  CONTENT_MATCHES,
  PLATFORMS,
  MAIN_FALLBACK_TIMEOUT_MS,
  MAIN_PROBE_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_VERSION,
  MAIN_VERIFY_RESULT_MESSAGE,
  isCaptureMessage,
  isMainReadyMessage,
  isMainVerifyResultMessage,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  BACKFILL_TAB_HELLO_MESSAGE,
  handleBackfillMessage,
} from '../lib/backfill/tab-port';
import {
  warnIfFallbackHookUnverified,
  FALLBACK_HOOK_VERIFICATION_WARNING,
} from '../lib/fallback-verification';

/**
 * ISOLATED-world bridge. WHY ISOLATED: MAIN/page world has no extension API
 * access, so this script converts validated page messages into
 * browser.runtime.sendMessage for the background service worker.
 * Matches only the explicit origins in the platform table, never <all_urls>.
 */
export default defineContentScript({
  matches: CONTENT_MATCHES,
  runAt: 'document_start',
  main() {
    const probeToken = makeProbeToken();
    let mainReady = false;
    let fallbackInjectionAttempted = false;
    let fallbackScriptAppended = false;
    let mainVerificationRequested = false;
    let fallbackVerificationRequested = false;
    let fallbackVerificationTimer: ReturnType<typeof setTimeout> | undefined;

    const pageOrigin = window.location.origin;
    const isPageMessage = (event: MessageEvent<unknown>): boolean =>
      event.source === window &&
      event.origin === pageOrigin &&
      PLATFORMS.some((platform) => platform.origins.includes(event.origin));

    const onMessage = (event: MessageEvent<unknown>): void => {
      if (!isPageMessage(event)) return;

      if (isMainReadyMessage(event.data) && event.data.token === probeToken) {
        if (!mainReady && !mainVerificationRequested && !fallbackVerificationRequested) {
          mainVerificationRequested = true;
          if (!injectPageVerifier(probeToken)) {
            mainVerificationRequested = false;
            injectFallbackHook();
          }
        }
        return;
      }

      if (isMainVerifyResultMessage(event.data) && event.data.token === probeToken) {
        if (fallbackVerificationRequested) {
          fallbackVerificationRequested = false;
          if (fallbackVerificationTimer !== undefined) {
            clearTimeout(fallbackVerificationTimer);
            fallbackVerificationTimer = undefined;
          }
          if (
            warnIfFallbackHookUnverified({
              scriptAppended: fallbackScriptAppended,
              markerInstalled: event.data.installed,
            })
          ) {
            return;
          }
          mainReady = true;
          return;
        }

        if (!mainVerificationRequested) return;
        mainVerificationRequested = false;
        if (event.data.installed) {
          mainReady = true;
        } else {
          injectFallbackHook();
        }
        return;
      }

      if (!isCaptureMessage(event.data)) return;
      browser.runtime
        .sendMessage({ type: 'chat-captured', payload: event.data.payload })
        .catch(() => {
          // A failed extension channel must never disturb the page.
        });
    };

    function injectPageScript(source: string): boolean {
      if (typeof document === 'undefined') return false;
      const parent = document.documentElement ?? document.head;
      if (!parent) return false;
      const script = document.createElement('script');
      script.textContent = source;
      parent.appendChild(script);
      script.remove();
      return true;
    }

    function injectPageVerifier(token: string): boolean {
      const marker = JSON.stringify(PAGE_HOOK_FETCH_MARKER);
      const version = JSON.stringify(PAGE_HOOK_VERSION);
      const origin = JSON.stringify(pageOrigin);
      return injectPageScript(`(() => {
        const fetchFn = window.fetch;
        const installed = typeof fetchFn === 'function' && fetchFn[${marker}] === ${version};
        window.postMessage({
          type: ${JSON.stringify(MAIN_VERIFY_RESULT_MESSAGE)},
          version: ${version},
          token: ${JSON.stringify(token)},
          installed,
        }, ${origin});
      })();`);
    }

    function injectFallbackHook(): void {
      if (fallbackInjectionAttempted || mainReady) return;
      fallbackInjectionAttempted = true;
      const source = `(${installPageFetchHook.toString()})(${JSON.stringify(PAGE_HOOK_OPTIONS)});`;
      fallbackScriptAppended = injectPageScript(source);
      if (!fallbackScriptAppended) {
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
        return;
      }

      fallbackVerificationRequested = true;
      if (!injectPageVerifier(probeToken)) {
        fallbackVerificationRequested = false;
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
        return;
      }
      fallbackVerificationTimer = setTimeout(() => {
        if (!fallbackVerificationRequested) return;
        fallbackVerificationRequested = false;
        fallbackVerificationTimer = undefined;
        console.warn(FALLBACK_HOOK_VERIFICATION_WARNING);
      }, MAIN_FALLBACK_TIMEOUT_MS);
    }

    // -----------------------------------------------------------------------
    // 🔴 C19 · 回溯腿的取数通道就在这里落地。
    //
    // 这段代码跑在【用户已登录的那个页面】的上下文里，所以下面这个 fetch 是
    // **同源**请求，带的就是用户自己那个页面的 cookie —— 与用户手动点开一条
    // 历史对话时浏览器发出的请求同源同凭据。因此：
    //   · 不需要任何 host 权限（同源请求本来就不受 host 权限约束）；
    //   · matches 一个字都不用改（本来就注入在这些平台上）；
    //   · 取数没有被挪出用户的登录上下文（架构前提保住了）。
    // 允许代发哪些 URL 由 lib/backfill/tab-port.ts 的三道检查决定
    //（同源 + 在平台表里 + 只有回溯腿那两条路径），这里不自己判断。
    // -----------------------------------------------------------------------
    browser.runtime.onMessage.addListener(
      (message: unknown, _sender: unknown, sendResponse: (r: unknown) => void) => {
        const pending = handleBackfillMessage(message, pageOrigin, async (url, init) => {
          // 🔴 C23：method / body / Content-Type 都已经被 checkBackfillRequest 过完闭集
          //    才会走到这里（handleBackfillMessage → serveBackfillFetch）。
          //    这里不再做任何判断，也【不许】做 —— 判断只有一处，就是那个白名单。
          //    init 省略（GET 段）⇒ 下面这个 fetch 的实参与 C19/C22 逐字相同。
          const res = init && init.method === 'POST'
            ? await fetch(url, {
                method: 'POST',
                credentials: 'same-origin',
                headers: {
                  accept: 'application/json',
                  ...(init.contentType ? { 'content-type': init.contentType } : {}),
                },
                body: init.body,
              })
            : await fetch(url, {
                credentials: 'same-origin',
                headers: { accept: 'application/json' },
              });
          return { status: res.status, text: () => res.text() };
        });
        if (!pending) return;   // 不是给我的消息，让给别的监听器
        pending
          .then(sendResponse)
          .catch((err: Error) => sendResponse({ ok: false, error: err.message }));
        return true;            // MV3：异步 sendResponse 必须返回 true
      },
    );

    // 报到：把 tab id（由浏览器填在 sender 上）留给 background，
    // 好让【闹钟醒来时】知道该找哪个标签页取数。失败无所谓 —— 实时腿那条路
    // 用的是当场的 sender，不依赖这张登记表。
    browser.runtime
      .sendMessage({ type: BACKFILL_TAB_HELLO_MESSAGE, origin: pageOrigin })
      .catch(() => { /* background 没醒/没人接：不打扰页面 */ });

    window.addEventListener('message', onMessage);
    // A tokenized probe makes the readiness handshake insensitive to which
    // document_start content script runs first.
    window.postMessage({ type: MAIN_PROBE_MESSAGE, token: probeToken }, pageOrigin);
    setTimeout(() => {
      if (!mainReady && !mainVerificationRequested && !fallbackVerificationRequested) {
        injectFallbackHook();
      }
    }, MAIN_FALLBACK_TIMEOUT_MS);
  },
});

function makeProbeToken(): string {
  try {
    return crypto.randomUUID();
  } catch {
    return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  }
}
