import type { TFunction } from 'i18next'
import { deskErrorCodeEnum } from '@/services/types'
import { deskErrorMessage, type ErrorCodeKeyMap } from '@/lib/desk-error-i18n'

const SIGNALING_TYPE_CODE_TERMINAL_STARTED = 10013
const SIGNALING_TYPE_CODE_ERROR = -1

const START_ERRORS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: 'pages.terminal.startDenied',
}

type ResponseState = { error_code?: number | null; message?: string | null } | null | undefined

export type TerminalStartOutcome =
    | { kind: 'started' }
    | { kind: 'retry' }
    | { kind: 'failed'; message: string }

/**
 * How the device answered a terminal start, or null when the message is not
 * an answer to it. The device answers on the start reply type (or a generic
 * error frame); a refusal or a "try again" carries an error state there.
 */
export function terminalStartOutcome(
    t: TFunction,
    signalingType: number,
    responseState: ResponseState,
): TerminalStartOutcome | null {
    if (signalingType !== SIGNALING_TYPE_CODE_TERMINAL_STARTED && signalingType !== SIGNALING_TYPE_CODE_ERROR) {
        return null
    }
    const code = responseState?.error_code
    if (code === deskErrorCodeEnum.ACTION_NEED_RETRY) return { kind: 'retry' }
    if (code === undefined || code === null || code === deskErrorCodeEnum.SUCCESS) {
        return signalingType === SIGNALING_TYPE_CODE_TERMINAL_STARTED ? { kind: 'started' } : null
    }
    if (signalingType !== SIGNALING_TYPE_CODE_TERMINAL_STARTED) return null
    return {
        kind: 'failed',
        message: deskErrorMessage(t, START_ERRORS, code, responseState?.message, t('pages.terminal.startFailed')),
    }
}
