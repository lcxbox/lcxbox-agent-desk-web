// The composer text survives a remount of the assistant page, which happens
// when the device target is looked up again after the host reconnected.
const PREFIX = 'ai-assistant-draft:';

export function readAssistantDraft(scope: string): string {
    try {
        return sessionStorage.getItem(PREFIX + scope) ?? '';
    } catch {
        return '';
    }
}

export function writeAssistantDraft(scope: string, text: string): void {
    try {
        if (text) sessionStorage.setItem(PREFIX + scope, text);
        else sessionStorage.removeItem(PREFIX + scope);
    } catch {
        // Storage unavailable: the draft lives only in memory.
    }
}
