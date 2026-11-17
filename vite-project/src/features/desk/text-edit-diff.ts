/** One run of text on one side of an edit; `changed` marks what the edit removes or adds. */
export type DiffSegment = { text: string; changed: boolean };

/** The removed (old) and added (new) side of one exact edit, with shared context collapsed. */
export type InlineTextDiff = { removed: DiffSegment[]; added: DiffSegment[] };

export const DIFF_CONTEXT_CHARS = 40;
export const ELLIPSIS = '…';
// Token-level LCS runs only while the table stays small; larger changes are
// shown as one removed and one added block.
const MAX_LCS_CELLS = 40_000;
// Han and kana compare per character; other words, whitespace runs and single symbols are tokens.
const TOKEN = /[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}]|(?:(?![\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}])[\p{L}\p{N}_])+|\s+|[^]/gu;

// Word characters that compare as one token; Han and kana are excluded.
const WORD = /^[\p{L}\p{N}_]$/u;
const HAN_OR_KANA = /^[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}]$/u;
const isWord = (char: string | undefined) => !!char && WORD.test(char) && !HAN_OR_KANA.test(char);
const isHigh = (code: number) => code >= 0xd800 && code <= 0xdbff;
const isLow = (code: number) => code >= 0xdc00 && code <= 0xdfff;

type Op = { kind: 'equal' | 'remove' | 'add'; text: string };

function tokenOps(oldText: string, newText: string): Op[] {
    const a = oldText.match(TOKEN) ?? [];
    const b = newText.match(TOKEN) ?? [];
    if (a.length === 0 || b.length === 0 || (a.length + 1) * (b.length + 1) > MAX_LCS_CELLS) {
        return [{ kind: 'remove', text: oldText }, { kind: 'add', text: newText }];
    }
    const width = b.length + 1;
    const table = new Uint16Array((a.length + 1) * width);
    for (let i = a.length - 1; i >= 0; i--) {
        for (let j = b.length - 1; j >= 0; j--) {
            table[i * width + j] = a[i] === b[j] ? table[(i + 1) * width + j + 1] + 1
                : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
        }
    }
    const ops: Op[] = [];
    const push = (kind: Op['kind'], text: string) => {
        const last = ops[ops.length - 1];
        if (last?.kind === kind) last.text += text; else ops.push({ kind, text });
    };
    let i = 0, j = 0;
    while (i < a.length && j < b.length) {
        if (a[i] === b[j]) { push('equal', a[i]); i++; j++; }
        else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) push('remove', a[i++]);
        else push('add', b[j++]);
    }
    while (i < a.length) push('remove', a[i++]);
    while (j < b.length) push('add', b[j++]);
    // A whitespace-only match between two changes is noise: fold it into the change.
    return ops.flatMap((op, index): Op[] => op.kind === 'equal' && !op.text.trim() && index > 0 && index < ops.length - 1
        ? [{ kind: 'remove', text: op.text }, { kind: 'add', text: op.text }] : [op]);
}

function side(ops: Op[], keep: Op['kind']): DiffSegment[] {
    const segments: DiffSegment[] = [];
    for (const op of ops) {
        if (op.kind !== 'equal' && op.kind !== keep) continue;
        const changed = op.kind !== 'equal';
        const last = segments[segments.length - 1];
        if (last && last.changed === changed) last.text += op.text;
        else if (op.text) segments.push({ text: op.text, changed });
    }
    return segments;
}

/**
 * Inline diff of one exact edit: the shared prefix and suffix are cut to
 * `context` characters around the change, and the changed middle is aligned
 * by a token-level LCS so only the differing runs are marked.
 */
export function inlineTextDiff(oldText: string, newText: string, context = DIFF_CONTEXT_CHARS): InlineTextDiff {
    const limit = Math.min(oldText.length, newText.length);
    let start = 0;
    while (start < limit && oldText.charCodeAt(start) === newText.charCodeAt(start)) start++;
    // Never split a surrogate pair or a word between the shared and changed parts.
    if (start > 0 && isHigh(oldText.charCodeAt(start - 1))) start--;
    while (start > 0 && isWord(oldText[start - 1]) && (isWord(oldText[start]) || isWord(newText[start]))) start--;
    let end = 0;
    while (end < limit - start
        && oldText.charCodeAt(oldText.length - 1 - end) === newText.charCodeAt(newText.length - 1 - end)) end++;
    if (end > 0 && isLow(oldText.charCodeAt(oldText.length - end))) end--;
    while (end > 0 && isWord(oldText[oldText.length - end])
        && (isWord(oldText[oldText.length - end - 1]) || isWord(newText[newText.length - end - 1]))) end--;
    let prefix = oldText.slice(0, start);
    let suffix = oldText.slice(oldText.length - end);
    if (prefix.length > context) {
        let cut = prefix.length - context;
        if (isLow(prefix.charCodeAt(cut))) cut++;
        prefix = ELLIPSIS + prefix.slice(cut);
    }
    if (suffix.length > context) {
        let cut = context;
        if (isHigh(suffix.charCodeAt(cut - 1))) cut--;
        suffix = suffix.slice(0, cut) + ELLIPSIS;
    }
    const ops: Op[] = [
        { kind: 'equal', text: prefix },
        ...tokenOps(oldText.slice(start, oldText.length - end), newText.slice(start, newText.length - end)),
        { kind: 'equal', text: suffix },
    ];
    return { removed: side(ops, 'remove'), added: side(ops, 'add') };
}

/** The head of a long text, or null when the whole text is short enough to show. */
export function textPreviewHead(text: string, maxLines = 12, maxChars = 1200): string | null {
    let lines = 0;
    let index = 0;
    while (index < text.length && index < maxChars) {
        if (text.charCodeAt(index) === 10 && ++lines === maxLines) break;
        index++;
    }
    if (index >= text.length) return null;
    if (index > 0 && isHigh(text.charCodeAt(index - 1))) index--;
    return text.slice(0, index);
}
