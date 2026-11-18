import { isOutcomeUnknownText } from './assistant-result-summary';

/** Display-only projection; this never authorizes retry or further execution. */
export function hasUnknownActionResult(text: string | null, expectedCallId?: string): boolean {
    if (!text) return false;
    try {
        const value: unknown = JSON.parse(text);
        if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
        const result = value as Record<string, unknown>;
        return result.result === 'outcome_unknown'
            && typeof result.action_request_id === 'string' && result.action_request_id.length > 0
            && (expectedCallId === undefined || result.action_request_id === expectedCallId)
            && typeof result.work_id === 'string' && result.work_id.length > 0
            && typeof result.execution_generation === 'string' && result.execution_generation.length > 0
            && Array.isArray(result.facts);
    } catch { return false; }
}

/**
 * A result whose outcome is unknown: a native unknown receipt, or a fixed
 * outcome-unknown text the server wrote for the model. Display-only.
 */
export function hasUnknownOutcome(text: string | null, expectedCallId?: string): boolean {
    return hasUnknownActionResult(text, expectedCallId) || isOutcomeUnknownText(text);
}
