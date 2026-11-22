import { describe, expect, it } from 'vitest';

import { isAiAgentEnabled } from './ai-agent-switch';

describe('isAiAgentEnabled', () => {
    it('requires an explicit enabled projection', () => {
        expect(isAiAgentEnabled({ enabled: true })).toBe(true);
        expect(isAiAgentEnabled({ enabled: false })).toBe(false);
        expect(isAiAgentEnabled({})).toBe(false);
        expect(isAiAgentEnabled(null)).toBe(false);
    });
});
