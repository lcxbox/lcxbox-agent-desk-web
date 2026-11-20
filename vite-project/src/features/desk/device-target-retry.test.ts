import { AxiosError, AxiosHeaders } from 'axios';
import { describe, expect, it } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import { isTransientTargetFailure, transientTargetRetryDelayMs } from './device-target-retry';

const coded = (code: number) => Object.assign(new Error('rest'), { code });
const http = (status?: number) => new AxiosError('http', undefined, undefined, undefined,
    status === undefined ? undefined : { status, statusText: '', data: null, headers: {}, config: { headers: new AxiosHeaders() } });

describe('device target retry classification', () => {
    it('retries failures that heal once the host is back', () => {
        expect(isTransientTargetFailure(coded(deskErrorCodeEnum.REMOTE_DESK_OFFLINE))).toBe(true);
        expect(isTransientTargetFailure(coded(deskErrorCodeEnum.SHARED_STORE_UNAVAILABLE))).toBe(true);
        expect(isTransientTargetFailure(http())).toBe(true);
        expect(isTransientTargetFailure(http(502))).toBe(true);
    });

    it('leaves access and existence answers to the user', () => {
        expect(isTransientTargetFailure(coded(deskErrorCodeEnum.DEVICE_NOT_FOUND))).toBe(false);
        expect(isTransientTargetFailure(coded(deskErrorCodeEnum.PERMISSION_ERROR))).toBe(false);
        expect(isTransientTargetFailure(http(401))).toBe(false);
        expect(isTransientTargetFailure(http(403))).toBe(false);
        expect(isTransientTargetFailure(new Error('unknown'))).toBe(false);
    });

    it('backs off and caps the delay', () => {
        const offline = coded(deskErrorCodeEnum.REMOTE_DESK_OFFLINE);
        expect(transientTargetRetryDelayMs(offline, 1)).toBe(1_000);
        expect(transientTargetRetryDelayMs(offline, 2)).toBe(1_500);
        expect(transientTargetRetryDelayMs(offline, 50)).toBe(30_000);
        expect(transientTargetRetryDelayMs(coded(deskErrorCodeEnum.PERMISSION_ERROR), 1)).toBe(false);
    });
});
