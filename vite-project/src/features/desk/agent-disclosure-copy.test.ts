import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';

describe('agent capability disclosure', () => {
    it('removes redundant general notices', () => {
        for (const locale of [zh, en]) {
            expect(Object.keys(locale)).not.toContain('pages.aiAgent.workspace.reviewNotice');
            expect(Object.keys(locale)).not.toContain('pages.aiAgent.disclosure');
            expect(Object.keys(locale)).not.toContain('pages.aiAgent.disclosureTitle');
        }
    });
    it('avoids obsolete layout and developer wording', () => {
        for (const locale of [zh, en]) {
            expect(locale['pages.aiAgent.sessionDescription']).not.toContain('daemon/worker');
            expect(locale['pages.aiAgent.providerBoundary']).toContain('{{model}}');
            expect(locale['pages.aiAgent.providerBoundary']).toContain('{{provider}}');
        }
    });
});
