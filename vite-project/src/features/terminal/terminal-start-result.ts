import type { TFunction } from 'i18next'
import { deskErrorCodeEnum } from '@/services/types'
import { deskErrorMessage, type ErrorCodeKeyMap } from '@/lib/desk-error-i18n'

const START_ERRORS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: 'pages.terminal.startDenied',
}

/**
 * Why the device refused to start the terminal, or null when the reply
 * started it. A refusal arrives as the start reply carrying an error state.
 */
export function terminalStartFailure(
    t: TFunction,
    responseState: { error_code?: number | null; message?: string | null } | null | undefined,
): string | null {
    const code = responseState?.error_code
    if (code === undefined || code === null || code === deskErrorCodeEnum.SUCCESS) return null
    return deskErrorMessage(t, START_ERRORS, code, responseState?.message, t('pages.terminal.startFailed'))
}
