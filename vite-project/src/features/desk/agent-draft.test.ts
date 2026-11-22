import { afterEach, describe, expect, it, vi } from 'vitest';
import { readAgentDraft, writeAgentDraft } from './agent-draft';

afterEach(() => {
    vi.restoreAllMocks();
    sessionStorage.clear();
});

describe('agent draft', () => {
    it('keeps the composer text per scope and forgets it once empty', () => {
        writeAgentDraft('device-a', 'check the disk');
        expect(readAgentDraft('device-a')).toBe('check the disk');
        expect(readAgentDraft('device-b')).toBe('');
        writeAgentDraft('device-a', '');
        expect(sessionStorage.getItem('ai-agent-draft:device-a')).toBeNull();
    });

    it('falls back to memory when storage is unavailable', () => {
        vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('blocked'); });
        vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('blocked'); });
        expect(() => writeAgentDraft('device-a', 'text')).not.toThrow();
        expect(readAgentDraft('device-a')).toBe('');
    });
});
