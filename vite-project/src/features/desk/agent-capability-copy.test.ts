import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';
import { capabilityDescriptionKey } from './agent-capability-copy';

describe('capability inventory copy', () => {
    it('uses an explicit fallback for unknown key namespaces', () => {
        expect(capabilityDescriptionKey('future.capability')).toBe('pages.aiAgent.workspace.descriptionUnavailable');
        expect(capabilityDescriptionKey('agent.capability.systemCommandExecute'))
            .toBe('agent.capabilityDescription.systemCommandExecute');
    });
    it('provides a name and description in both languages for every registered capability', () => {
        const moduleRoot = '../diagnose-core/src/ai_agent';
        const source = [readFileSync('../diagnose-core/src/ai_agent.rs', 'utf8'),
            ...readdirSync(moduleRoot, { recursive: true }).filter(name => name.endsWith('.rs'))
                .map(name => readFileSync(join(moduleRoot, name), 'utf8'))].join('\n');
        const keys = [...new Set([...source.matchAll(/"(agent\.capability\.[A-Za-z]+)"/g)].map((match) => match[1]))];
        expect(keys.length).toBeGreaterThan(0);
        expect(keys).toContain('agent.capability.wordBatchInspect');
        expect(keys).toContain('agent.capability.wordBatchPatch');
        expect(keys).toContain('agent.capability.powerpointBatchInspect');
        for (const key of keys) {
            for (const locale of [zh, en]) {
                const copy = locale as Record<string, string>;
                expect(copy[key], key).toBeTruthy();
                expect(copy[key.replace('agent.capability.', 'agent.capabilityDescription.')], key).toBeTruthy();
            }
        }
    });
});
