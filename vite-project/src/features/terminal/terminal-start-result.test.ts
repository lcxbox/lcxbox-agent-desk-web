import { describe, expect, it } from 'vitest'
import type { TFunction } from 'i18next'
import { deskErrorCodeEnum } from '@/services/types'
import { terminalStartOutcome } from './terminal-start-result'

const t = ((key: string) => key) as unknown as TFunction
const STARTED = 10013
const ERROR = -1
const REPLY = 10011

describe('terminalStartOutcome', () => {
    it('treats a start reply without an error state as started', () => {
        expect(terminalStartOutcome(t, STARTED, undefined)).toEqual({ kind: 'started' })
        expect(terminalStartOutcome(t, STARTED, { error_code: deskErrorCodeEnum.SUCCESS })).toEqual({ kind: 'started' })
    })

    it('retries when the device asks for it on the start reply or an error frame', () => {
        const retry = { error_code: deskErrorCodeEnum.ACTION_NEED_RETRY, message: 'Manager credential verification is temporarily unavailable' }
        expect(terminalStartOutcome(t, STARTED, retry)).toEqual({ kind: 'retry' })
        expect(terminalStartOutcome(t, ERROR, retry)).toEqual({ kind: 'retry' })
    })

    it('explains a start the device refused', () => {
        expect(terminalStartOutcome(t, STARTED, {
            error_code: deskErrorCodeEnum.PERMISSION_ERROR,
            message: 'Terminal access denied by security settings or user',
        })).toEqual({ kind: 'failed', message: 'pages.terminal.startDenied' })
        expect(terminalStartOutcome(t, STARTED, { error_code: deskErrorCodeEnum.INVALID_PARAMS, message: 'Missing command' }))
            .toEqual({ kind: 'failed', message: 'Missing command' })
        expect(terminalStartOutcome(t, STARTED, { error_code: deskErrorCodeEnum.INVALID_PARAMS }))
            .toEqual({ kind: 'failed', message: 'pages.terminal.startFailed' })
    })

    it('ignores frames that do not answer the start', () => {
        expect(terminalStartOutcome(t, REPLY, undefined)).toBeNull()
        expect(terminalStartOutcome(t, ERROR, { error_code: deskErrorCodeEnum.INVALID_PARAMS })).toBeNull()
    })
})
