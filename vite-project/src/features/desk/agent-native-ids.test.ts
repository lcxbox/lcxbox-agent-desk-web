import { describe, expect, it } from 'vitest';
import { agentDataControlId, agentMessageId } from './agent-native-ids';

describe('native agent message identities', () => {
    it('encodes all bytes using the shared URL-safe representation', () => {
        const bytes = Uint8Array.from({ length: 16 }, (_, i) => i);
        expect(agentMessageId(bytes)).toBe('m_AAECAwQFBgcICQoLDA0ODw');
        expect(agentMessageId(bytes)).toHaveLength(24);
        const different = bytes.slice();
        different[15] = 255;
        expect(agentMessageId(different)).not.toBe(agentMessageId(bytes));
    });

    it('rejects incomplete identity material', () => {
        expect(() => agentMessageId(new Uint8Array(15))).toThrow();
    });

    it('keeps data controls in their independent native identity domain', () => {
        const bytes = Uint8Array.from({ length: 16 }, (_, i) => i);
        expect(agentDataControlId(bytes)).toBe('dc_AAECAwQFBgcICQoLDA0ODw');
        expect(agentDataControlId(bytes)).not.toBe(agentMessageId(bytes));
        expect(() => agentDataControlId(new Uint8Array(15))).toThrow();
    });
});
