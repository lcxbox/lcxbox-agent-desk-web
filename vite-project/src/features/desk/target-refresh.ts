import { deskErrorCodeEnum } from '@/services/types';
import { SIGNALING_TYPE_CODE_AI_AGENT_UPDATED } from './constants';

const TARGET_GONE = new Set<number>([
    deskErrorCodeEnum.TARGET_CONNECTION_CHANGED,
    deskErrorCodeEnum.REMOTE_DESK_OFFLINE,
    deskErrorCodeEnum.DEVICE_NOT_FOUND,
]);

/** Whether an error code means the device connection this page targets is gone. */
export function isTargetGoneCode(code: number | null | undefined): boolean {
    return typeof code === 'number' && TARGET_GONE.has(code);
}

type RefreshProbe = {
    signaling_type: number;
    response_state?: { error_code?: number | null } | null;
    signaling_data?: unknown;
};

/**
 * Whether a frame says the device must be looked up again: a refused request,
 * or an AI Agent error event (the central answers AI requests with typed
 * events rather than an error state).
 */
export function needsTargetRefresh(message: RefreshProbe): boolean {
    if (isTargetGoneCode(message.response_state?.error_code)) return true;
    if (message.signaling_type !== SIGNALING_TYPE_CODE_AI_AGENT_UPDATED) return false;
    const event = message.signaling_data as { kind?: string; error?: { error_code?: number | null } | null } | null;
    return event?.kind === 'error' && isTargetGoneCode(event.error?.error_code);
}
