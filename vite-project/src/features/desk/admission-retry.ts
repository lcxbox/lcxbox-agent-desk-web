import { deskErrorCodeEnum } from '@/services/types';

export const ADMISSION_RETRY_INTERVALS_MS = [2_000, 5_000, 10_000, 20_000] as const;

export class AdmissionRetrySchedule {
    private index = 0;

    reset(): void {
        this.index = 0;
    }

    nextDelay(): number | null {
        const delay = ADMISSION_RETRY_INTERVALS_MS[this.index];
        if (delay === undefined) return null;
        this.index += 1;
        return delay;
    }
}

/**
 * Admission refusals that describe a host still starting up rather than a
 * decision.
 *
 * `SESSION_UNAVAILABLE` is ambiguous: a freshly restarted host answers it
 * before its session worker has registered, but it is also the steady answer
 * of a host where nobody is logged in. It is retried only while
 * `hostMayBeRestarting` — an interrupted session is waiting to resume — so a
 * plain visit still sees the refusal at once.
 */
export function isRetryableAdmissionError(
    code: number | undefined,
    hostMayBeRestarting = false,
): boolean {
    return code === deskErrorCodeEnum.ACTION_NEED_RETRY
        || code === deskErrorCodeEnum.REMOTE_DESKTOP_CAPABILITIES_NOT_READY
        || (hostMayBeRestarting && code === deskErrorCodeEnum.SESSION_UNAVAILABLE);
}
