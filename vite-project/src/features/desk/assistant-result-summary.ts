import type { TFunction } from 'i18next';

const PREFIX = 'not executed: ';

// Fixed reasons the server writes for the model after "not executed: "
// (diagnose-core agent loop, approval and schedule paths, and the OSS and
// Manager command seams). Matching is by prefix: some reasons carry trailing
// guidance for the model. Unlisted reasons, such as text reported by the
// device, keep their original wording.
const REASONS: ReadonlyArray<readonly [string, string]> = [
    ['the owner refused this call', 'ownerRefused'],
    ['the owner denied this call', 'ownerRefused'],
    ['AI review refused this call', 'aiReviewRefused'],
    ['independent AI review denied this exact action', 'aiReviewRefused'],
    ['this call was not approved', 'notApproved'],
    ['task permission request was rejected', 'taskPermissionRejected'],
    ['waiting for user permission decision', 'waitingForPermission'],
    ['waiting for the existing user permission decision', 'waitingForExistingPermission'],
    ['waiting for the proposal decision', 'waitingForProposal'],
    ['waiting for directory confirmation', 'waitingForDirectory'],
    ['a prior command in this turn was not run', 'priorCommandNotRun'],
    ['a prior command in this turn was cancelled', 'priorCommandCancelled'],
    ['a prior action did not pass dispatch validation', 'priorActionInvalid'],
    ['the background action failed', 'backgroundActionFailed'],
    ['superseded by newer user input', 'superseded'],
    ['an automation turn cannot start a new command', 'automationCannotStartCommand'],
    ['the wait for approval ended before this call ran', 'approvalWaitEnded'],
    ['the turn was interrupted before this command ran', 'turnInterrupted'],
    ['the turn ended before this approved call ran', 'turnEndedBeforeApprovedCall'],
    ['the scheduled task was cancelled before approval', 'scheduledTaskCancelled'],
    ['the previous request ended before this call ran', 'previousRequestEndedBeforeRun'],
    ['the previous request ended before approval', 'previousRequestEndedBeforeApproval'],
    ['the owner stopped generation before approval', 'ownerStoppedGeneration'],
    ['the original turn lost its lease before dispatch', 'turnLeaseLost'],
    ['the original scheduled executor lost its lease before dispatch', 'scheduledLeaseLost'],
    ['the durable action was closed before dispatch', 'actionClosedBeforeDispatch'],
    ['the delegated task source is paused', 'delegatedSourcePaused'],
    ['the approved command target or input changed', 'approvedCommandChanged'],
    ['the approval no longer covers this call', 'approvalNoLongerCovers'],
    ['goal control or goal opening must be the only tool in its model response', 'goalToolMustBeAlone'],
    ['call budget admission failed', 'callBudgetFailed'],
    ['approval expired before this call ran', 'approvalExpired'],
    ['a new owner message replaced this request before approval', 'replacedByNewMessage'],
    ['mutating execution is not supported by this runtime', 'mutationUnsupported'],
    ['the command is not admitted by the current policy', 'commandNotAdmitted'],
    ['the command is not admitted by its approved plan and current policy', 'commandNotAdmittedByPlan'],
    ['confirmed command execution is disabled by current policy', 'confirmedExecutionDisabled'],
    ['execution policy changed; preview the command again', 'policyChangedPreview'],
    ['execution policy changed; request permission again', 'policyChangedRequest'],
    ['the approval session was cancelled', 'approvalSessionCancelled'],
    ['it was cancelled while awaiting approval', 'cancelledAwaitingApproval'],
    ['the interactive carrier was not ready at approval', 'interactiveCarrierNotReady'],
    ['a carrier was supplied for a non-interactive command', 'carrierForNonInteractive'],
    ["the owner's interactive terminal is no longer open", 'interactiveTerminalClosed'],
    ['Approval AI is unavailable', 'approvalAiUnavailable'],
    ['independent AI review was unavailable', 'approvalAiUnavailableUi'],
    ['command is not executable through the AI path', 'commandNoTemplate'],
    ['the target does not currently report interactive execution readiness', 'interactiveExecutionNotReady'],
    ['the target does not currently report interactive elevation readiness', 'interactiveElevationNotReady'],
    ['interactive elevation is unavailable until the Linux ServiceDaemon containment supervisor is ready', 'linuxElevationNotReady'],
    ['this AI Assistant mutation is not enabled', 'mutationNotEnabled'],
];

const TOOL_REASONS: ReadonlyArray<readonly [RegExp, string]> = [
    [/^tool `([^`]+)` failed earlier in this group/, 'earlierToolFailed'],
    [/^independent AI review already refused (\S+) \d+ times/, 'reviewRefusedRepeatedly'],
];

// Fixed outcome-unknown results and errors the server writes for the model
// (session recovery, released dispatch, command reconciliation, file and UI
// providers). Matched by prefix.
const OUTCOME_UNKNOWN: ReadonlyArray<readonly [string, string]> = [
    ['Execution did not return a conclusive receipt', 'noConclusiveReceipt'],
    ['Execution failed to return a conclusive result', 'noConclusiveResult'],
    ['The turn was interrupted without a recoverable action receipt', 'interruptedWithoutReceipt'],
    ['The original action outcome is unknown', 'originalAction'],
    ['The original command outcome is unknown', 'originalCommand'],
    ['Original command outcome unknown', 'originalCommand'],
    ['The original dispatch intent was committed, but no original transport binding is available', 'noTransportBinding'],
    ["The dispatched action's outcome is unknown", 'dispatchedAction'],
    ['File change outcome is unknown', 'fileChange'],
    ['File restore outcome is unknown', 'fileRestore'],
    ['execution outcome unknown; the command may have executed', 'commandMayHaveRun'],
    ['Provider dispatch outcome is unknown', 'providerDispatch'],
    ['native UI request timed out after starting', 'uiTimedOut'],
    ['AT-SPI native action has no reliable completion receipt', 'uiNoReceipt'],
    ['browser exact send outcome is unknown', 'browserSend'],
    ['the AI call outcome is unknown', 'aiCall'],
    ['Office file copy outcome unknown', 'officeCopy'],
];

// Carried inside a formatted command result rather than at its start.
const DEVICE_RECONNECTED = "The device restarted or reconnected, so this command's result can no longer be obtained";

function strip(text: string): string {
    const trimmed = text.trimStart();
    return trimmed.startsWith('tool error: ') ? trimmed.slice('tool error: '.length) : trimmed;
}

/**
 * Localized summary for a fixed "not executed" or outcome-unknown tool result
 * or error text, or null when `text` is neither. `toolLabel` names a tool
 * mentioned in the reason.
 */
export function resultSummaryMessage(t: TFunction, text: string, toolLabel: (name: string) => string = name => name): string | null {
    const body = strip(text);
    const unknown = body.includes(DEVICE_RECONNECTED) ? 'deviceReconnected'
        : OUTCOME_UNKNOWN.find(([prefix]) => body.startsWith(prefix))?.[1];
    if (unknown) {
        return t('pages.aiAssistant.outcomeUnknown.format', { reason: t(`pages.aiAssistant.outcomeUnknown.${unknown}`) });
    }
    return notExecutedMessage(t, body, toolLabel);
}

function notExecutedMessage(t: TFunction, output: string, toolLabel: (name: string) => string): string | null {
    let reason = output;
    if (!reason.startsWith(PREFIX)) return null;
    // A reviewed denial repeats the prefix in its own text.
    while (reason.startsWith(PREFIX)) reason = reason.slice(PREFIX.length);
    const fixed = REASONS.find(([text]) => reason.startsWith(text));
    let localized: string | null = fixed ? t(`pages.aiAssistant.notExecuted.${fixed[1]}`) : null;
    if (!localized) {
        for (const [pattern, key] of TOOL_REASONS) {
            const match = pattern.exec(reason);
            if (match) {
                localized = t(`pages.aiAssistant.notExecuted.${key}`, { tool: toolLabel(match[1]) });
                break;
            }
        }
    }
    return t('pages.aiAssistant.notExecuted.format', { reason: localized ?? reason });
}
