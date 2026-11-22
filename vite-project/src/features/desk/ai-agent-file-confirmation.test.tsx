import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { TextFileConfirmationCard, validTextFileReview, fileApprovalBlocked } from './ai-agent-file-confirmation';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string, values?: Record<string, unknown>) => values ? `${key}:${JSON.stringify(values)}` : key }) }));

const review = {
    fileName: 'notes.txt', sourceResultCallId: 'call-real', expectedSha256: 'a'.repeat(64),
    operation: 'update' as const, edits: [{ old_text: '版本=1\n', new_text: '版本=2\n', replace_all: false }],
    write: null, restore: null, oneShot: true, recoverable: true,
};
const restore = {
    ...review, operation: 'restore' as const, expectedSha256: null, edits: null,
    restore: { originalOperation: 'update' as const, backupCreatedAtUnixMs: 1_700_000_000_000, overridesNonAiVersion: false, fileExpectedToExist: true },
};

describe('exact text file approval', () => {
    it('blocks missing, malformed and mismatched operation reviews', () => {
        expect(fileApprovalBlocked({ toolName: 'update_text_file', textFileConfirmation: review })).toBe(false);
        expect(fileApprovalBlocked({ toolName: 'delete_text_file', textFileConfirmation: review })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'restore_text_file', textFileConfirmation: review })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'restore_text_file', textFileConfirmation: restore })).toBe(false);
        expect(fileApprovalBlocked({ toolName: 'update_text_file' })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'restore_text_file' })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'read_text_file' })).toBe(false);
        for (const invalid of [null, {}, { ...review, expectedSha256: 'bad' }, { ...review, recoverable: false },
            { ...review, oneShot: false }, { ...review, edits: null }, { ...review, edits: [] },
            { ...review, edits: [{ old_text: '', new_text: 'x' }] }, { ...review, write: 'both' },
            { ...review, edits: Array.from({ length: 33 }, () => ({ old_text: 'a', new_text: 'b' })) },
            { ...review, operation: 'delete', write: 'x', edits: null }, { ...restore, restore: null },
            { ...restore, restore: { ...restore.restore, backupCreatedAtUnixMs: 0 } }, { ...restore, edits: review.edits }]) {
            expect(validTextFileReview(invalid)).toBe(false);
        }
    });

    it('shows each edit as removed and added blocks with only the change marked, never as HTML', () => {
        const value = { ...review, edits: [...review.edits, { old_text: 'a', new_text: '<script>x</script>', replace_all: true }] };
        const { container, getAllByTestId } = render(<TextFileConfirmationCard value={value} />);
        expect(getAllByTestId('text-edit')).toHaveLength(2);
        const removed = [...container.querySelectorAll('pre[data-diff="removed"]')];
        const added = [...container.querySelectorAll('pre[data-diff="added"]')];
        expect(removed.map(node => node.textContent)).toEqual(['版本=1\n', 'a']);
        expect(added.map(node => node.textContent)).toEqual(['版本=2\n', '<script>x</script>']);
        expect([...removed[0].querySelectorAll('mark')].map(node => node.textContent)).toEqual(['1']);
        expect([...added[0].querySelectorAll('mark')].map(node => node.textContent)).toEqual(['2']);
        expect(container.querySelector('script')).toBeNull();
        expect(container.textContent).toContain('fileConfirmEditIndex:{"index":2,"count":2}');
        expect(container.textContent).toContain('fileConfirmReplaceEvery');
        expect(container.textContent).toContain('SHA-256: ' + 'a'.repeat(64));
    });

    it('collapses a long full rewrite until expanded and shows deletion as recoverable', () => {
        const write = Array.from({ length: 40 }, (_, index) => `line ${index}`).join('\n');
        const { container, getByRole, rerender } = render(<TextFileConfirmationCard value={{ ...review, edits: null, write }} />);
        const block = () => container.querySelector('pre[data-diff="write"]')!.textContent;
        expect(block()).not.toContain('line 39');
        fireEvent.click(getByRole('button', { name: 'pages.aiAgent.fileConfirmShowAll' }));
        expect(block()).toBe(write);
        rerender(<TextFileConfirmationCard value={{ ...review, operation: 'delete', edits: null }} />);
        expect(container.textContent).toContain('fileConfirmRecoverable');
        expect(container.querySelector('pre')).toBeNull();
    });

    it('describes a restore, warns before overwriting a non-AI version and notes recreation', () => {
        const { container, queryByTestId, rerender } = render(<TextFileConfirmationCard value={restore} />);
        expect(container.textContent).toContain('pages.aiAgent.fileConfirmRestore: notes.txt');
        expect(container.textContent).toContain('fileConfirmRestoreUndo.update');
        expect(container.textContent).toContain('fileConfirmRestoreBackupTime');
        expect(container.textContent).not.toContain('fileConfirmRestoreRecreate');
        expect(container.textContent).not.toContain('SHA-256');
        expect(queryByTestId('restore-overrides')).toBeNull();
        rerender(<TextFileConfirmationCard value={{ ...restore, restore: { ...restore.restore, originalOperation: 'delete', overridesNonAiVersion: true, fileExpectedToExist: false } }} />);
        expect(container.textContent).toContain('fileConfirmRestoreUndo.delete');
        expect(container.textContent).toContain('fileConfirmRestoreRecreate');
        expect(queryByTestId('restore-overrides')?.textContent).toContain('fileConfirmRestoreOverrides');
    });

    it('accepts an explicit empty full replacement, never an omitted body', () => {
        expect(validTextFileReview({ ...review, edits: null, write: '' })).toBe(true);
        expect(validTextFileReview({ ...review, edits: undefined, write: undefined })).toBe(false);
    });
});
