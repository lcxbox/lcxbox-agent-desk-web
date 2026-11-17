import { describe, expect, it } from 'vitest';
import { inlineTextDiff, textPreviewHead, type DiffSegment } from './text-edit-diff';

const join = (segments: DiffSegment[]) => segments.map(segment => segment.text).join('');
const changed = (segments: DiffSegment[]) => segments.filter(segment => segment.changed).map(segment => segment.text);

describe('inline text edit diff', () => {
    it('marks only the differing token and keeps both sides intact', () => {
        const diff = inlineTextDiff('timeout = 30\nretries = 3\n', 'timeout = 60\nretries = 3\n');
        expect(join(diff.removed)).toBe('timeout = 30\nretries = 3\n');
        expect(join(diff.added)).toBe('timeout = 60\nretries = 3\n');
        expect(changed(diff.removed)).toEqual(['30']);
        expect(changed(diff.added)).toEqual(['60']);
    });

    it('marks whole words when the old and new word share a prefix', () => {
        const diff = inlineTextDiff('port = 80;', 'port = 8080;');
        expect(changed(diff.removed)).toEqual(['80']);
        expect(changed(diff.added)).toEqual(['8080']);
        expect(join(diff.added)).toBe('port = 8080;');
    });

    it('aligns separate changes in the middle with a token LCS', () => {
        const diff = inlineTextDiff('alpha beta gamma delta', 'alpha BETA gamma DELTA');
        expect(changed(diff.removed)).toEqual(['beta', 'delta']);
        expect(changed(diff.added)).toEqual(['BETA', 'DELTA']);
        expect(diff.removed.find(segment => segment.text.includes('gamma'))?.changed).toBe(false);
    });

    it('collapses long shared context around the change', () => {
        const head = 'x '.repeat(50);
        const tail = ' y'.repeat(50);
        const diff = inlineTextDiff(`${head}old${tail}`, `${head}new${tail}`, 10);
        expect(join(diff.removed)).toBe(`…${'x '.repeat(5)}old${' y'.repeat(5)}…`);
        expect(join(diff.added)).toBe(`…${'x '.repeat(5)}new${' y'.repeat(5)}…`);
    });

    it('shows pure insertions and deletions on one side only', () => {
        const insert = inlineTextDiff('a\nc\n', 'a\nb\nc\n');
        expect(changed(insert.removed)).toEqual([]);
        expect(changed(insert.added)).toEqual(['b\n']);
        const remove = inlineTextDiff('keep drop', 'keep ');
        expect(changed(remove.removed)).toEqual(['drop']);
        expect(join(remove.added)).toBe('keep ');
    });

    it('compares Han text per character and never splits surrogate pairs', () => {
        const han = inlineTextDiff('版本号为一', '版本号为二');
        expect(changed(han.removed)).toEqual(['一']);
        expect(changed(han.added)).toEqual(['二']);
        const mixed = inlineTextDiff('name 名称 value', 'name 名字 value');
        expect(changed(mixed.removed)).toEqual(['称']);
        const latinHan = inlineTextDiff('id:abc中文x', 'id:abd中文x');
        expect(changed(latinHan.removed)).toEqual(['abc']);
        expect(changed(latinHan.added)).toEqual(['abd']);
        const emoji = inlineTextDiff('x😀y', 'x😁y');
        expect(changed(emoji.removed)).toEqual(['😀']);
        expect(changed(emoji.added)).toEqual(['😁']);
    });

    it('falls back to whole blocks when the changed middle is too large to align', () => {
        const oldText = Array.from({ length: 400 }, (_, index) => `a${index}`).join(' ');
        const newText = Array.from({ length: 400 }, (_, index) => `b${index}`).join(' ');
        const diff = inlineTextDiff(oldText, newText);
        expect(changed(diff.removed)).toEqual([oldText]);
        expect(join(diff.added)).toBe(newText);
    });
});

describe('text preview head', () => {
    it('returns null for short text and a line-bounded head otherwise', () => {
        expect(textPreviewHead('one\ntwo\n')).toBeNull();
        expect(textPreviewHead('1\n2\n3\n4\n', 2)).toBe('1\n2');
        expect(textPreviewHead('x'.repeat(50), 12, 10)).toBe('x'.repeat(10));
    });
});
