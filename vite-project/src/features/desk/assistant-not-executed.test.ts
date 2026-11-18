import { createInstance } from 'i18next';
import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';
import { notExecutedMessage } from './assistant-not-executed';

async function translator(lng: 'zh-CN' | 'en-US') {
    const i18n = createInstance();
    await i18n.init({ lng, resources: { 'zh-CN': { translation: zh }, 'en-US': { translation: en } },
        interpolation: { escapeValue: false } });
    return i18n.t;
}

// Results as the server writes them, including trailing model guidance.
const SERVER_RESULTS = [
    'not executed: the owner refused this call',
    'not executed: the owner denied this call',
    'not executed: AI review refused this call; see the approval record for the reason',
    'not executed: not executed: independent AI review denied this exact action. reason_code=scope. Ask the owner.',
    'not executed: this call was not approved',
    'not executed: task permission request was rejected',
    'not executed: waiting for user permission decision',
    'not executed: waiting for the existing user permission decision',
    'not executed: waiting for the proposal decision',
    'not executed: waiting for directory confirmation',
    'not executed: a prior command in this turn was not run',
    'not executed: a prior command in this turn was cancelled',
    'not executed: a prior action did not pass dispatch validation',
    'not executed: the background action failed',
    'not executed: superseded by newer user input',
    'not executed: an automation turn cannot start a new command',
    'not executed: the wait for approval ended before this call ran; call it again if still needed',
    'not executed: the turn was interrupted before this command ran',
    'not executed: the turn ended before this approved call ran; call it again if still needed',
    'not executed: the scheduled task was cancelled before approval',
    'not executed: the previous request ended before this call ran',
    'not executed: the previous request ended before approval',
    'not executed: the owner stopped generation before approval',
    'not executed: the original turn lost its lease before dispatch; do not retry this action automatically',
    'not executed: the original scheduled executor lost its lease before dispatch; do not retry automatically',
    'not executed: the durable action was closed before dispatch; newer user input superseded this turn',
    'not executed: the delegated task source is paused',
    'not executed: the approved command target or input changed',
    'not executed: the approval no longer covers this call (it expired, was used up, or its target changed); call it again if still needed',
    'not executed: goal control or goal opening must be the only tool in its model response',
    'not executed: call budget admission failed',
    'not executed: call budget admission failed before dispatch',
    'not executed: approval expired before this call ran',
    'not executed: a new owner message replaced this request before approval; call it again if still needed',
    'not executed: mutating execution is not supported by this runtime',
    'not executed: the command is not admitted by the current policy',
    'not executed: the command is not admitted by its approved plan and current policy',
    'not executed: confirmed command execution is disabled by current policy',
    'not executed: execution policy changed; preview the command again',
    'not executed: execution policy changed; request permission again',
    'not executed: the approval session was cancelled',
    'not executed: it was cancelled while awaiting approval',
    'not executed: the interactive carrier was not ready at approval',
    'not executed: a carrier was supplied for a non-interactive command',
    "not executed: the owner's interactive terminal is no longer open; the command was not executed",
    'not executed: Approval AI is unavailable; the command was not executed',
    'not executed: independent AI review was unavailable; this UI action was not executed. Retry later or ask the owner for another safe approach',
    'not executed: command is not executable through the AI path (no matching template)',
    'not executed: the target does not currently report interactive execution readiness',
    'not executed: the target does not currently report interactive elevation readiness',
    'not executed: interactive elevation is unavailable until the Linux ServiceDaemon containment supervisor is ready',
    'not executed: this AI Assistant mutation is not enabled',
    'not executed: tool `exec_command` failed earlier in this group; this tool was not dispatched',
    'not executed: independent AI review already refused exec_command 3 times for this request. Do not call it again.',
];

describe('not-executed tool results', () => {
    it('localizes every fixed server result without leaving English behind', async () => {
        const t = await translator('zh-CN');
        for (const output of SERVER_RESULTS) {
            const message = notExecutedMessage(t, output);
            expect(message, output).toMatch(/^未执行：/);
            expect(message, output).not.toMatch(/\b[a-z]{4,}\b/);
        }
    });

    it('names the owner, the reviewer and the tool in a reason', async () => {
        const zhT = await translator('zh-CN');
        expect(notExecutedMessage(zhT, 'not executed: the owner refused this call')).toBe('未执行：你拒绝了这次调用');
        expect(notExecutedMessage(zhT, 'not executed: tool `exec_command` failed earlier in this group; this tool was not dispatched',
            name => name === 'exec_command' ? '执行命令' : name)).toBe('未执行：同组中的 执行命令 先失败，此工具未派发');
        const enT = await translator('en-US');
        expect(notExecutedMessage(enT, 'not executed: the owner refused this call')).toBe('Not executed: you refused this call');
    });

    it('keeps an unknown reason and ignores other results', async () => {
        const t = await translator('zh-CN');
        expect(notExecutedMessage(t, 'not executed: device said no')).toBe('未执行：device said no');
        expect(notExecutedMessage(t, 'Window closed before step 2')).toBeNull();
        expect(notExecutedMessage(t, '{"result":"outcome_unknown"}')).toBeNull();
    });
});
