/**
 * Live host connections must not survive in the back/forward cache.
 *
 * Chrome keeps a page in the back/forward cache after a navigation, open
 * WebSockets included. A cached control page kept its session's admission
 * on the host, so the virtual display stayed plugged in on the local
 * desktop until the page was evicted or restored (on device 2026-11-20).
 *
 * Every owner of a host socket registers its close with {@link onPageFrozen}.
 * When the page enters the cache (`pagehide` with `persisted`), all of them
 * run. Everything the page held on the host is gone then, so a page
 * restored from the cache (`pageshow` with `persisted`) reloads once.
 * Ordinary unloads are left alone: the browser closes sockets itself.
 */

type Close = () => void;

const closers = new Set<Close>();
let listening = false;
let closedOnHide = false;
let reload: () => void = () => window.location.reload();

function isPersisted(event: Event): boolean {
    return (event as PageTransitionEvent).persisted === true;
}

function handlePageHide(event: Event) {
    if (!isPersisted(event) || closers.size === 0) return;
    closedOnHide = true;
    for (const close of [...closers]) {
        try {
            close();
        } catch (error) {
            console.warn('[page-lifecycle] closing a connection failed', error);
        }
    }
}

function handlePageShow(event: Event) {
    if (!isPersisted(event) || !closedOnHide) return;
    closedOnHide = false;
    reload();
}

function listen() {
    if (listening || typeof window === 'undefined') return;
    listening = true;
    window.addEventListener('pagehide', handlePageHide);
    window.addEventListener('pageshow', handlePageShow);
}

/**
 * Run `close` when the page enters the back/forward cache. Returns the
 * unregister function; call it when the connection is closed anyway.
 */
export function onPageFrozen(close: Close): () => void {
    listen();
    closers.add(close);
    return () => {
        closers.delete(close);
    };
}

/** Tests: replace the reload and forget all state. */
export function resetPageLifecycleForTests(reloadPage: () => void = () => {}) {
    closers.clear();
    closedOnHide = false;
    reload = reloadPage;
}
