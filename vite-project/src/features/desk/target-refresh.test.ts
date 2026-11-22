import { describe, expect, it } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';
import { SIGNALING_TYPE_CODE_AI_AGENT_UPDATED, SIGNALING_TYPE_CODE_TERMINAL_AI_AGENT_UPDATED } from './constants';
import { needsTargetRefresh } from './target-refresh';

const errorEvent = (signalingType: number, code: number) => ({
    signaling_type: signalingType,
    signaling_data: { kind: 'error', request_id: 'r', seq: 0, error: { error_code: code } },
});

describe('needsTargetRefresh', () => {
    it('refreshes on a refused request whose target is gone', () => {
        expect(needsTargetRefresh({ signaling_type: -1, response_state: { error_code: deskErrorCodeEnum.REMOTE_DESK_OFFLINE } })).toBe(true);
        expect(needsTargetRefresh({ signaling_type: -1, response_state: { error_code: deskErrorCodeEnum.PERMISSION_ERROR } })).toBe(false);
    });

    it('refreshes on an AI Agent error event whose target is gone', () => {
        for (const code of [deskErrorCodeEnum.REMOTE_DESK_OFFLINE, deskErrorCodeEnum.TARGET_CONNECTION_CHANGED, deskErrorCodeEnum.DEVICE_NOT_FOUND]) {
            expect(needsTargetRefresh(errorEvent(SIGNALING_TYPE_CODE_AI_AGENT_UPDATED, code))).toBe(true);
        }
        expect(needsTargetRefresh(errorEvent(SIGNALING_TYPE_CODE_AI_AGENT_UPDATED, deskErrorCodeEnum.INVALID_PARAMS))).toBe(false);
    });

    it('leaves other frames alone', () => {
        expect(needsTargetRefresh(errorEvent(SIGNALING_TYPE_CODE_TERMINAL_AI_AGENT_UPDATED, deskErrorCodeEnum.REMOTE_DESK_OFFLINE))).toBe(false);
        expect(needsTargetRefresh({ signaling_type: SIGNALING_TYPE_CODE_AI_AGENT_UPDATED, signaling_data: { kind: 'text' } })).toBe(false);
        expect(needsTargetRefresh({ signaling_type: SIGNALING_TYPE_CODE_AI_AGENT_UPDATED, signaling_data: null })).toBe(false);
    });
});
