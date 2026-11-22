// The composer text survives a remount of the agent page, which happens
// when the device target is looked up again after the host reconnected.
const PREFIX = 'ai-agent-draft:';

export function readAgentDraft(scope: string): string {
    try {
        return sessionStorage.getItem(PREFIX + scope) ?? '';
    } catch {
        return '';
    }
}

export function writeAgentDraft(scope: string, text: string): void {
    try {
        if (text) sessionStorage.setItem(PREFIX + scope, text);
        else sessionStorage.removeItem(PREFIX + scope);
    } catch {
        // Storage unavailable: the draft lives only in memory.
    }
}
