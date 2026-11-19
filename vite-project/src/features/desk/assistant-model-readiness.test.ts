import { describe, expect, it } from 'vitest';
import { assistantModelMissing } from './assistant-model-readiness';

const answered = (config: { api_key_set?: boolean; model?: string | null }) => ({
    isPending: false, isSuccess: true, data: { data: config },
});

describe('assistantModelMissing', () => {
    it('waits while the provider query is loading', () => {
        expect(assistantModelMissing({ isPending: true, isSuccess: false })).toBe(true);
    });

    it('blocks when the host reports no key or no model', () => {
        expect(assistantModelMissing(answered({ api_key_set: false, model: 'm' }))).toBe(true);
        expect(assistantModelMissing(answered({ api_key_set: true, model: null }))).toBe(true);
        expect(assistantModelMissing(answered({ api_key_set: true, model: 'm' }))).toBe(false);
    });

    it('does not block on a host without the provider query', () => {
        expect(assistantModelMissing({ isPending: false, isSuccess: false })).toBe(false);
    });
});
