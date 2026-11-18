import { afterEach, describe, expect, it, vi } from 'vitest';
import { readAssistantDraft, writeAssistantDraft } from './assistant-draft';

afterEach(() => {
    vi.restoreAllMocks();
    sessionStorage.clear();
});

describe('assistant draft', () => {
    it('keeps the composer text per scope and forgets it once empty', () => {
        writeAssistantDraft('device-a', 'check the disk');
        expect(readAssistantDraft('device-a')).toBe('check the disk');
        expect(readAssistantDraft('device-b')).toBe('');
        writeAssistantDraft('device-a', '');
        expect(sessionStorage.getItem('ai-assistant-draft:device-a')).toBeNull();
    });

    it('falls back to memory when storage is unavailable', () => {
        vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('blocked'); });
        vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('blocked'); });
        expect(() => writeAssistantDraft('device-a', 'text')).not.toThrow();
        expect(readAssistantDraft('device-a')).toBe('');
    });
});
