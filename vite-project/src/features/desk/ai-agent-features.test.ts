import { describe, expect, it } from 'vitest';

import {
    hasAiAgentBrowserEntry,
} from './ai-agent-features';

const features = {
    schema_version: 1,
    turn_stream: true,
    capability_inventory: true,
    full_session_snapshot: true,
    permission_decision: true,
    approval_delegation: true,
    grant_revoke: true,
    background_task_cancel: true,
    object_context: true,
    exec_pty: true,
    device_settings: false,
};

describe('AI Agent feature profile', () => {
    it('keeps the Manager browser entry while independently gating unfinished controls', () => {
        const manager = {
            ...features,
            permission_decision: false,
            grant_revoke: false,
            background_task_cancel: false,
            object_context: false,
        };

        expect(hasAiAgentBrowserEntry(manager)).toBe(true);
        expect(manager.permission_decision).toBe(false);
        expect(manager.grant_revoke).toBe(false);
        expect(manager.background_task_cancel).toBe(false);
        expect(manager.object_context).toBe(false);
    });

    it('requires the complete minimum read-turn contract', () => {
        expect(hasAiAgentBrowserEntry(null)).toBe(false);
        expect(hasAiAgentBrowserEntry({
            ...features,
            full_session_snapshot: false,
        })).toBe(false);
    });
});
