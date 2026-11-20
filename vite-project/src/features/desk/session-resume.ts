/**
 * Carries "a live remote session was interrupted" across the remount that a
 * host restart causes (the device re-registers with a new connection id, so
 * the whole session runtime is rebuilt).
 *
 * Only an unexpected signaling drop of a session that had reached a live media
 * link arms the slot. The slot is consumed by the next mount's config dialog,
 * which submits itself when the user's saved settings still resolve unchanged
 * against the restarted host. It is cleared only by an outcome — the resumed
 * link connecting, a user action, the TTL, or leaving the page — so a mount
 * that is torn down again before it connects does not lose it.
 */

/** Long enough to cover a host reboot, short enough not to surprise later. */
export const SESSION_RESUME_TTL_MS = 10 * 60 * 1000;

export interface SessionResumeIntent {
    hadControl: boolean;
    /** Clipboard choice to restore with control; null = host default. */
    clipboard: boolean | null;
    armedAt: number;
}

export interface SessionResumeSlot {
    arm(intent: SessionResumeIntent): void;
    /** The armed intent, or null when none is armed or it has expired. */
    peek(now?: number): SessionResumeIntent | null;
    clear(): void;
}

export function createSessionResumeSlot(
    ttlMs: number = SESSION_RESUME_TTL_MS,
): SessionResumeSlot {
    let intent: SessionResumeIntent | null = null;
    return {
        arm(next) {
            intent = next;
        },
        peek(now = Date.now()) {
            if (intent && now - intent.armedAt > ttlMs) intent = null;
            return intent;
        },
        clear() {
            intent = null;
        },
    };
}
