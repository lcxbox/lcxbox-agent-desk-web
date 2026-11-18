import { describe, expect, it } from 'vitest'
import type { TFunction } from 'i18next'
import { deskErrorCodeEnum } from '@/services/types'
import { terminalStartFailure } from './terminal-start-result'

const t = ((key: string) => key) as unknown as TFunction

describe('terminalStartFailure', () => {
    it('treats a reply without an error state as started', () => {
        expect(terminalStartFailure(t, undefined)).toBeNull()
        expect(terminalStartFailure(t, { error_code: deskErrorCodeEnum.SUCCESS })).toBeNull()
    })

    it('explains a start the device refused', () => {
        expect(terminalStartFailure(t, {
            error_code: deskErrorCodeEnum.PERMISSION_ERROR,
            message: 'Terminal access denied by security settings or user',
        })).toBe('pages.terminal.startDenied')
        expect(terminalStartFailure(t, { error_code: deskErrorCodeEnum.INVALID_PARAMS, message: 'Missing command' }))
            .toBe('Missing command')
        expect(terminalStartFailure(t, { error_code: deskErrorCodeEnum.INVALID_PARAMS }))
            .toBe('pages.terminal.startFailed')
    })
})
