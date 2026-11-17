import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { parseFileReceipt, parseTextFileChange, AssistantFileResult } from './assistant-file-result';
import { AssistantCommandResult, parseReadWindow } from './assistant-command-result';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const mutation = (verified = true, operation = 'delete') => ({ result: verified ? 'verified' : 'outcome_unknown', output: { kind: 'text_file_mutation', value: { operation, original_file_name: 'notes.txt', original_size_bytes: 3, original_sha256: 'a'.repeat(64), recovery: { recovery_id: 'b'.repeat(64), created_at_unix_ms: 1, expires_at_unix_ms: 1000, cleanup_pending: false }, verified, updated_file: null } } });
describe('native file receipts', () => {
    it('presents Windows and macOS batch copies through the same file receipt', () => {
        for (const fileName of ['copy.pptx', 'copy.docx', `${'a'.repeat(250)}.DOCX`, 'copy.xlsx', `${'a'.repeat(250)}.XLSX`, 'copy.key', 'copy.pages', 'copy.numbers']) {
            const fileOnly = /\.(pptx|docx|xlsx)$/i.test(fileName);
            const value = fileOnly
                ? { file_name: fileName, size_bytes: 123, digest_sha256: 'a'.repeat(64) }
                : { file_name: fileName, byte_len: 123, sha256: 'a'.repeat(64), validation_byte_len: 32, validation_sha256: 'b'.repeat(64) };
            const output = { kind: fileOnly ? 'file_artifact' : 'batch_document_artifact', value };
            expect(parseFileReceipt(JSON.stringify({ result: 'verified', output }))).toEqual({
                operation: 'create', verified: true, fileName, bytes: 123, digest: 'a'.repeat(64),
            });
            for (const result of ['outcome_unknown', 'definitely_not_started', 'changed_but_unverified']) {
                expect(parseFileReceipt(JSON.stringify({ result, output }))).toBeNull();
            }
        }
        const value = { file_name: 'copy.key', byte_len: 123, sha256: 'a'.repeat(64), validation_byte_len: 32, validation_sha256: 'b'.repeat(64) };
        for (const changed of [{ byte_len: -1 }, { validation_byte_len: 0 }, { validation_sha256: 'bad' }, { file_name: '' }]) {
            expect(parseFileReceipt(JSON.stringify({ result: 'verified', output: { kind: 'batch_document_artifact', value: { ...value, ...changed } } }))).toBeNull();
        }
    });
    it.each([
        ['Word', 'DOCX', 'application/vnd.openxmlformats-officedocument.wordprocessingml.document'],
        ['Excel', 'XLSX', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet'],
    ])('renders the verified %s copy through the shared result component without granting another action', (_host, extension, mediaType) => {
        const fileName = `${'报告 '.repeat(35)}x.${extension}`;
        const output = { kind: 'file_artifact', value: {
            file_name: fileName, size_bytes: 123, digest_sha256: 'a'.repeat(64),
            media_type: mediaType,
        } };
        const { container, rerender } = render(<AssistantCommandResult text={JSON.stringify({ result: 'verified', output })} />);
        const title = screen.getByRole('button', { name: /fileReceipt.title.*fileReceipt.create.*fileReceipt.verified/ });
        expect(title.getAttribute('aria-expanded')).toBe('false');
        fireEvent.click(title);
        expect(title.getAttribute('aria-expanded')).toBe('true');
        expect(screen.getByText(fileName)).toBeVisible();
        expect(screen.getByText('123', { selector: 'dd' })).toBeVisible();
        expect(screen.getByText('a'.repeat(64))).toBeVisible();
        expect(container.querySelector('a')).toBeNull();
        expect(screen.queryByRole('button', { name: 'pages.fileRecovery.export' })).toBeNull();
        for (const result of ['outcome_unknown', 'definitely_not_started', 'changed_but_unverified']) {
            rerender(<AssistantCommandResult text={JSON.stringify({ result, output })} />);
            expect(screen.queryByRole('button', { name: /fileReceipt.title.*fileReceipt.verified/ })).toBeNull();
        }
    });
    it('exports the backup reference and leaves a readable failure that can be retried', async () => {
        const text = JSON.stringify(mutation());
        const download = vi.fn().mockRejectedValueOnce(new Error('Offline')).mockResolvedValue(undefined);
        render(<AssistantFileResult receipt={parseFileReceipt(text)!} text={text} onExportBackup={download} />);
        const button = screen.getByRole('button', { name: 'pages.fileRecovery.export', hidden: true });
        fireEvent.click(button);
        await waitFor(() => expect(screen.getByRole('alert', { hidden: true })).toBeTruthy());
        expect(download).toHaveBeenCalledWith('b'.repeat(64));
        fireEvent.click(button);
        await waitFor(() => expect(download).toHaveBeenCalledTimes(2));
        expect(screen.queryByRole('alert', { hidden: true })).toBeNull();
    });
    it('keeps recovery details collapsed and reports unknown without inventing success', () => {
        const text = JSON.stringify(mutation(false, 'update'));
        const receipt = parseFileReceipt(text)!;
        expect(receipt.operation).toBe('update');
        expect(receipt.verified).toBe(false);
        const { container } = render(<AssistantFileResult receipt={receipt} text={text} />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(screen.getByText('pages.aiAssistant.fileReceipt.unknownHint')).toBeTruthy();
        expect(container.querySelector('a')).toBeNull();
    });
    it('keeps empty creation byte counts and rejects malformed or contradictory facts', () => {
        expect(parseFileReceipt(JSON.stringify({ result: 'verified', output: { kind: 'file_artifact', value: { file_name: 'empty.txt', size_bytes: 0, digest_sha256: 'a'.repeat(64) } } }))?.bytes).toBe(0);
        expect(parseFileReceipt(JSON.stringify(mutation()))?.operation).toBe('delete');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.fileName).toBe('notes.txt');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.bytes).toBe(3);
        expect(parseFileReceipt(JSON.stringify({ ...mutation(), result: 'outcome_unknown' }))).toBeNull();
        const withoutReference = parseFileReceipt(JSON.stringify(mutation(true, 'update')));
        expect(withoutReference?.verified).toBe(true);
        expect(withoutReference?.referenceUnavailable).toBe(true);
        expect(withoutReference?.digest).toBeUndefined();
        expect(withoutReference?.bytes).toBeUndefined();
        expect(parseFileReceipt('{bad')).toBeNull();
    });
    it('accepts restores, including one that recreated a missing file without previous content', () => {
        const recreated = mutation(true, 'restore');
        Object.assign(recreated.output.value, { original_sha256: null, original_size_bytes: 0 });
        const receipt = parseFileReceipt(JSON.stringify(recreated));
        expect(receipt).toMatchObject({ operation: 'restore', verified: true, fileName: 'notes.txt' });
        expect(receipt?.digest).toBeUndefined();
        expect(receipt?.bytes).toBeUndefined();
        const written = mutation(true, 'restore');
        Object.assign(written.output.value, { updated_file: { file_name: 'notes.txt', size_bytes: 9, digest_sha256: 'c'.repeat(64) } });
        expect(parseFileReceipt(JSON.stringify(written))).toMatchObject({ operation: 'restore', bytes: 9, digest: 'c'.repeat(64) });
        for (const value of [{ original_sha256: null }, { original_sha256: null, operation: 'update' }, { original_sha256: null, original_size_bytes: 0, operation: 'delete' }]) {
            const invalid = mutation(true, 'restore');
            Object.assign(invalid.output.value, value);
            expect(parseFileReceipt(JSON.stringify(invalid))).toBeNull();
        }
    });
    it('shows the update diff from the call arguments and that the previous version can be restored', () => {
        const args = JSON.stringify({ file_result_call_id: 'read', expected_sha256: 'a'.repeat(64),
            change: { kind: 'edit', edits: [{ old_text: 'port = 80', new_text: 'port = 8080', replace_all: false }] } });
        const text = JSON.stringify(mutation(true, 'update'));
        const { container } = render(<AssistantCommandResult text={text} tool={{ callId: 'call', name: 'update_text_file', argumentsJson: args } as never} />);
        expect(container.querySelector('pre[data-diff="removed"]')?.textContent).toBe('port = 80');
        expect(container.querySelector('pre[data-diff="added"] mark')?.textContent).toBe('8080');
        expect(container.textContent).toContain('pages.aiAssistant.fileReceipt.restorable');
        expect(parseTextFileChange(JSON.stringify({ change: { kind: 'write', content_utf8: '' } }))).toEqual({ write: '' });
        for (const invalid of ['{bad', '{}', JSON.stringify({ change: { kind: 'edit', edits: [] } }), JSON.stringify({ change: { kind: 'replace_all' } })]) {
            expect(parseTextFileChange(invalid)).toBeNull();
        }
        const { container: unknown } = render(<AssistantFileResult receipt={parseFileReceipt(JSON.stringify(mutation(false, 'update')))!} text="" />);
        expect(unknown.textContent).not.toContain('fileReceipt.restorable');
    });
    it('summarizes the window of a text read or search', () => {
        const read = (value: Record<string, unknown>) => JSON.stringify({ ReadContext: { FileContentRead: {
            display_name: 'app.log', content_utf8: 'x', byte_len: 10, sha256: 'a'.repeat(64), total_lines: 500,
            start_line: 101, end_line: 200, truncated: true, next_start_byte: null, matches: [], ...value } } });
        expect(parseReadWindow(read({}))).toEqual({ fileName: 'app.log', totalLines: 500, startLine: 101, endLine: 200, truncated: true, matches: null });
        expect(parseReadWindow(read({ content_utf8: '', start_line: 0, end_line: 0, truncated: false, matches: [{ line: 3, byte_offset: 9, text: 'err' }] }))?.matches).toBe(1);
        expect(parseReadWindow(read({ total_lines: -1 }))).toBeNull();
        expect(parseReadWindow('{"Exec":{}}')).toBeNull();
        render(<AssistantCommandResult text={read({})} />);
        expect(screen.getByTestId('read-window').textContent).toContain('app.log · pages.aiAssistant.readWindow.lines · pages.aiAssistant.readWindow.truncated');
    });
});
