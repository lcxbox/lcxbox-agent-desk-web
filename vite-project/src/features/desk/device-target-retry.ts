import { isAxiosError } from 'axios';
import { errorCodeOf } from '@/lib/desk-error-i18n';
import { deskErrorCodeEnum } from '@/services/types';

const BASE_DELAY_MS = 1_000;
// An offline device left open in a visible tab is polled at most twice a minute.
const MAX_DELAY_MS = 30_000;
const BACKOFF = 1.5;

/**
 * Whether a failed device-target lookup is worth retrying on its own.
 *
 * A host restart or reboot answers "offline" (or nothing at all) for a few
 * seconds before the device registers again. Those failures heal without the
 * user, so the page must not wait for a click or a focus change. Answers that
 * describe the user's access or a deleted device do not heal and keep the
 * manual retry only.
 */
export function isTransientTargetFailure(error: unknown): boolean {
    const code = errorCodeOf(error);
    if (code === deskErrorCodeEnum.REMOTE_DESK_OFFLINE
        || code === deskErrorCodeEnum.SHARED_STORE_UNAVAILABLE) return true;
    if (code !== undefined) return false;
    if (!isAxiosError(error)) return false;
    const status = error.response?.status;
    // No response at all: the server itself is restarting or unreachable.
    return status === undefined || status >= 500;
}

/**
 * Delay before the next automatic lookup, or `false` for no automatic retry.
 * `consecutiveFailures` counts failures since the last success (>= 1).
 */
export function transientTargetRetryDelayMs(
    error: unknown,
    consecutiveFailures: number,
): number | false {
    if (!isTransientTargetFailure(error)) return false;
    const attempt = Math.max(1, consecutiveFailures);
    return Math.min(BASE_DELAY_MS * BACKOFF ** (attempt - 1), MAX_DELAY_MS);
}
