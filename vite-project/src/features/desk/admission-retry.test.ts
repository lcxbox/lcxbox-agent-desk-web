import { describe, expect, it } from 'vitest';

import { deskErrorCodeEnum } from '@/services/types';
import { ADMISSION_RETRY_INTERVALS_MS, AdmissionRetrySchedule, isRetryableAdmissionError } from './admission-retry';

describe('AdmissionRetrySchedule', () => {
    it('uses adjacent intervals whose total covers the host proof recovery window', () => {
        const schedule = new AdmissionRetrySchedule();
        expect([
            schedule.nextDelay(),
            schedule.nextDelay(),
            schedule.nextDelay(),
            schedule.nextDelay(),
        ]).toEqual([...ADMISSION_RETRY_INTERVALS_MS]);
        expect(schedule.nextDelay()).toBeNull();
        expect(ADMISSION_RETRY_INTERVALS_MS.reduce((sum, delay) => sum + delay, 0)).toBe(37_000);
    });

    it('can be reset for a new logical attempt', () => {
        const schedule = new AdmissionRetrySchedule();
        expect(schedule.nextDelay()).toBe(2_000);
        expect(schedule.nextDelay()).toBe(5_000);
        schedule.reset();
        expect(schedule.nextDelay()).toBe(2_000);
    });
});

describe('isRetryableAdmissionError', () => {
    it('retries a host that is still starting its session worker', () => {
        expect(isRetryableAdmissionError(deskErrorCodeEnum.SESSION_UNAVAILABLE, true)).toBe(true);
        // Without an interrupted session it is a host with no session: answer at once.
        expect(isRetryableAdmissionError(deskErrorCodeEnum.SESSION_UNAVAILABLE)).toBe(false);
        expect(isRetryableAdmissionError(deskErrorCodeEnum.ACTION_NEED_RETRY)).toBe(true);
        expect(isRetryableAdmissionError(deskErrorCodeEnum.REMOTE_DESKTOP_CAPABILITIES_NOT_READY)).toBe(true);
    });

    it('does not retry decisions', () => {
        expect(isRetryableAdmissionError(deskErrorCodeEnum.PERMISSION_ERROR)).toBe(false);
        expect(isRetryableAdmissionError(deskErrorCodeEnum.SESSION_SELECTION_REQUIRED)).toBe(false);
        expect(isRetryableAdmissionError(undefined)).toBe(false);
    });
});
