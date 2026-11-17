import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import {
    byteLimitLabel, CommandFileChangesView, CommandRestoreConfirmationCard, CommandSnapshotNotice, commandRestoreApprovalBlocked,
    parseCommandRestoreReceipt, parseFileChanges, validCommandRestoreReview, validSnapshotNotice,
} from './assistant-command-changes';
import { fileApprovalBlocked } from './ai-assistant-file-confirmation';
import { CommandConfirmationCard, validCommandReview } from './ai-assistant-command';
import { AssistantCommandResult, parseCommandReceipt } from './assistant-command-result';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string, values?: Record<string, unknown>) => values ? `${key}:${JSON.stringify(values)}` : key }) }));

const sha = (c: string) => c.repeat(64);
const notice = {
    roots: [{ kind: 'cwd', path: '/work/repo' }, { kind: 'grant', path: '/home/u/docs' }],
    excluded_names: ['.git', 'node_modules'], honors_ignore_files: true,
    max_files: 20000, max_file_bytes: 16 * 1024 * 1024, scan_time_limit_ms: 10000,
    estimated_file_count: null, unprotected: ['excluded_paths', 'outside_roots', 'future_gap'],
};
const directory = { token: 'dir', snapshot_id: 'snap', object_kind: 'directory', expires_at: '2099-01-01T00:00:00Z' };
const taken = {
    snapshot: { state: 'taken' }, recovery_id: sha('a'), started_at_unix_ms: 1_700_000_000_000, finished_at_unix_ms: 1_700_000_005_000,
    roots: [{ root: { kind: 'cwd', path: '/work/repo' }, directory }, { root: { kind: 'grant', directory_request_id: 'dir-req-1', directory }, directory }],
    changes: [
        { path: 'src/a.rs', root: 0, kind: 'modified', before_sha256: sha('b'), after_sha256: sha('c'), size_bytes: 10, protected: true },
        { path: 'img.png', root: 0, kind: 'added', before_sha256: null, after_sha256: null, size_bytes: 99, protected: false },
        { path: 'old.txt', root: 1, kind: 'deleted', before_sha256: sha('d'), after_sha256: null, size_bytes: 3, protected: true },
    ],
    truncated: true,
};
const skipped = { snapshot: { state: 'skipped', reason: 'protected_root' }, recovery_id: null, started_at_unix_ms: 1, finished_at_unix_ms: 2, roots: [], changes: [], truncated: false };
const restoreReview = {
    sourceResultCallId: 'call-exec', rootKind: 'cwd', rootPath: '/work/repo', directoryRequestId: null,
    executionStartedAtUnixMs: 1_700_000_000_000, executionFinishedAtUnixMs: 1_700_000_005_000,
    files: [{ path: 'src/a.rs', change: 'modified' }, { path: 'new.txt', change: 'added' }, { path: 'old.txt', change: 'deleted' }],
    oneShot: true,
};
const exec = (fileChanges: unknown) => JSON.stringify({ Exec: {
    exit_code: 0, duration_ms: 5, streams: { type: 'split', stdout: '', stderr: '', stdout_truncated: false, stderr_truncated: false }, file_changes: fileChanges,
} });
const restoreReceipt = (outcomes: string[], result: string, fileChanges: unknown) => JSON.stringify({ result, output: { kind: 'command_changes_restore', value: {
    results: outcomes.map((outcome, index) => ({ path: `f${index}.txt`, outcome })), file_changes: fileChanges,
} } });
const restoreChanges = { ...taken, roots: [taken.roots[0]], changes: [{ ...taken.changes[0], path: 'f0.txt' }], truncated: false };

describe('best-effort snapshot notice', () => {
    it('accepts the device notice and rejects malformed shapes', () => {
        expect(validSnapshotNotice(notice)).toBe(true);
        expect(validSnapshotNotice({ ...notice, estimated_file_count: 120 })).toBe(true);
        for (const invalid of [null, [], { ...notice, roots: [] }, { ...notice, roots: [{ kind: 'other', path: '/x' }] },
            { ...notice, roots: [{ kind: 'cwd', path: '' }] }, { ...notice, max_files: 0 }, { ...notice, max_file_bytes: -1 },
            { ...notice, estimated_file_count: -1 }, { ...notice, honors_ignore_files: 'yes' }, { ...notice, unprotected: [1] }]) {
            expect(validSnapshotNotice(invalid)).toBe(false);
        }
    });

    it('makes the whole command review invalid when its snapshot notice is malformed', () => {
        const review = { command: 'npm run fix', shell: 'bash', cwd: '/work/repo', targetDeviceId: 'd', targetSessionId: 's', timeoutMs: 1000,
            maxStdoutBytes: 1, maxStderrBytes: 1, executionBasis: 'template', oneShot: true,
            sandbox: { mode: 'sandboxed', attention: 'sandboxed', writableRoots: notice.roots, implicitWritable: ['exec_temp_dir'], identity: null } };
        expect(validCommandReview({ ...review, snapshot: notice })).toBe(true);
        expect(validCommandReview({ ...review, snapshot: null, sandbox: { mode: 'unrestricted', attention: 'neutral', writableRoots: [], implicitWritable: [] } })).toBe(true);
        expect(validCommandReview({ ...review, snapshot: { ...notice, roots: [] } })).toBe(false);
        render(<CommandConfirmationCard value={{ ...review, snapshot: notice } as never} />);
        expect(screen.getByTestId('command-snapshot')).toBeTruthy();
    });

    it('lists roots, exclusions, limits, gaps with a fallback, and the warning', () => {
        const { container } = render(<CommandSnapshotNotice value={{ ...notice, estimated_file_count: 120 } as never} />);
        const text = container.textContent ?? '';
        expect(text).toContain('commandSnapshot.root.cwd:{"path":"/work/repo"}');
        expect(text).toContain('commandSnapshot.root.grant:{"path":"/home/u/docs"}');
        expect(text).toContain('"names":".git, node_modules"');
        expect(text).toContain('commandSnapshot.ignoreFiles');
        expect(text).toContain('"files":20000,"size":"16 MiB","seconds":10');
        expect(text).toContain('commandSnapshot.estimate:{"count":120}');
        expect(text).toContain('commandSnapshot.gap.outside_roots');
        expect(text).toContain('commandSnapshot.gap.unknown');
        expect(screen.getByRole('alert').textContent).toContain('commandSnapshot.warning');
    });

    it('omits the estimate when the device has no manifest yet', () => {
        const { container } = render(<CommandSnapshotNotice value={notice as never} />);
        expect(container.textContent).not.toContain('commandSnapshot.estimate');
    });

    it('labels limits in the largest dividing binary unit', () => {
        expect(byteLimitLabel(16 * 1024 * 1024)).toBe('16 MiB');
        expect(byteLimitLabel(2 ** 30)).toBe('1 GiB');
        expect(byteLimitLabel(1536)).toBe('1536 B');
        expect(byteLimitLabel(2048)).toBe('2 KiB');
        expect(byteLimitLabel(1000)).toBe('1000 B');
    });
});

describe('command file change report', () => {
    it('parses taken and skipped reports', () => {
        expect(parseFileChanges(taken)).toEqual({
            skipReason: null, startedAt: taken.started_at_unix_ms, finishedAt: taken.finished_at_unix_ms,
            roots: [{ kind: 'cwd', label: '/work/repo' }, { kind: 'grant', label: 'dir-req-1' }],
            changes: [{ path: 'src/a.rs', root: 0, kind: 'modified', protected: true }, { path: 'img.png', root: 0, kind: 'added', protected: false },
                { path: 'old.txt', root: 1, kind: 'deleted', protected: true }],
            truncated: true,
        });
        expect(parseFileChanges(skipped)?.skipReason).toBe('protected_root');
    });

    it.each([
        ['taken without recovery id', { ...taken, recovery_id: null }],
        ['taken without roots', { ...taken, roots: [], changes: [] }],
        ['root index out of range', { ...taken, changes: [{ ...taken.changes[0], root: 2 }] }],
        ['unknown kind', { ...taken, changes: [{ ...taken.changes[0], kind: 'renamed' }] }],
        ['finished before started', { ...taken, finished_at_unix_ms: 1 }],
        ['skipped with changes', { ...skipped, changes: taken.changes }],
        ['skipped with recovery id', { ...skipped, recovery_id: sha('a') }],
        ['skipped without reason', { ...skipped, snapshot: { state: 'skipped' } }],
        ['unknown root kind', { ...taken, roots: [{ root: { kind: 'other' }, directory }] }],
    ])('rejects %s', (_, value) => {
        expect(parseFileChanges(value)).toBeNull();
        expect(parseCommandReceipt(exec(value))).toBeNull();
    });

    it('renders changes grouped by root with protection, the window, truncation and the notes', () => {
        render(<AssistantCommandResult text={exec(taken)} />);
        const view = screen.getByTestId('command-file-changes');
        expect(view.textContent).toContain('commandChanges.root.cwd:{"value":"/work/repo"}');
        expect(view.textContent).toContain('commandChanges.root.grant:{"value":"dir-req-1"}');
        expect(view.textContent).toContain('commandChanges.window:');
        const rows = screen.getAllByTestId('command-file-change').map(row => row.textContent);
        expect(rows).toEqual([
            'pages.aiAssistant.commandChanges.kind.modifiedsrc/a.rspages.aiAssistant.commandChanges.restorable',
            'pages.aiAssistant.commandChanges.kind.addedimg.pngpages.aiAssistant.commandChanges.notProtected',
            'pages.aiAssistant.commandChanges.kind.deletedold.txtpages.aiAssistant.commandChanges.restorable',
        ]);
        expect(view.textContent).toContain('commandChanges.truncated');
        for (const note of ['otherPrograms', 'excluded', 'detached']) expect(view.textContent).toContain(`commandChanges.note.${note}`);
    });

    it('states that a skipped snapshot leaves the changes unprotected, with the reason', () => {
        render(<CommandFileChangesView value={parseFileChanges(skipped)!} />);
        const view = screen.getByTestId('command-file-changes');
        expect(view.textContent).toContain('commandChanges.skipped');
        expect(view.textContent).toContain('commandChanges.skipReason.protected_root');
        render(<CommandFileChangesView value={{ ...parseFileChanges(skipped)!, skipReason: 'future_reason' }} />);
        expect(screen.getAllByTestId('command-file-changes')[1].textContent).toContain('commandChanges.skipReason.unknown');
    });

    it('reports no changes when a taken snapshot found none', () => {
        render(<CommandFileChangesView value={parseFileChanges({ ...taken, changes: [], truncated: false })!} />);
        expect(screen.getByTestId('command-file-changes').textContent).toContain('commandChanges.none');
    });
});

describe('command restore approval', () => {
    it('validates the review and blocks approval without it', () => {
        expect(validCommandRestoreReview(restoreReview)).toBe(true);
        expect(validCommandRestoreReview({ ...restoreReview, rootKind: 'grant', rootPath: null, directoryRequestId: 'dir-req-1' })).toBe(true);
        for (const invalid of [null, { ...restoreReview, oneShot: false }, { ...restoreReview, files: [] },
            { ...restoreReview, files: [{ path: 'a', change: 'renamed' }] }, { ...restoreReview, rootPath: null },
            { ...restoreReview, directoryRequestId: 'x' }, { ...restoreReview, rootKind: 'grant' },
            { ...restoreReview, executionFinishedAtUnixMs: 1 }, { ...restoreReview, sourceResultCallId: '' }]) {
            expect(validCommandRestoreReview(invalid)).toBe(false);
        }
        expect(commandRestoreApprovalBlocked({ toolName: 'restore_command_changes', commandRestoreConfirmation: restoreReview })).toBe(false);
        expect(commandRestoreApprovalBlocked({ toolName: 'restore_command_changes' })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'restore_command_changes', commandRestoreConfirmation: null })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'restore_command_changes', commandRestoreConfirmation: restoreReview })).toBe(false);
        expect(commandRestoreApprovalBlocked({ toolName: 'exec_command' })).toBe(false);
    });

    it('lists each file with its restore action under a prominent warning', () => {
        const { container } = render(<CommandRestoreConfirmationCard value={restoreReview as never} />);
        expect(screen.getByRole('alert').textContent).toContain('commandRestore.warning');
        const text = container.textContent ?? '';
        expect(text).toContain('commandChanges.root.cwd:{"value":"/work/repo"}');
        expect(text).toContain('commandChanges.window:');
        for (const action of ['modified', 'added', 'deleted']) expect(text).toContain(`commandRestore.action.${action}`);
        expect(text).toContain('commandRestore.conflicts');
    });
});

describe('command restore receipt', () => {
    it('parses per-file outcomes and requires the envelope result to match', () => {
        const receipt = parseCommandRestoreReceipt(restoreReceipt(['restored', 'conflict'], 'verified', restoreChanges));
        expect(receipt?.verified).toBe(true);
        expect(receipt?.results).toEqual([{ path: 'f0.txt', outcome: 'restored' }, { path: 'f1.txt', outcome: 'conflict' }]);
        expect(parseCommandRestoreReceipt(restoreReceipt(['restored', 'outcome_unknown'], 'outcome_unknown', restoreChanges))?.verified).toBe(false);
        expect(parseCommandRestoreReceipt(restoreReceipt(['restored', 'outcome_unknown'], 'verified', restoreChanges))).toBeNull();
        expect(parseCommandRestoreReceipt(restoreReceipt(['restored'], 'verified', taken))).toBeNull();
        expect(parseCommandRestoreReceipt(restoreReceipt([], 'verified', restoreChanges))).toBeNull();
        expect(parseCommandRestoreReceipt(restoreReceipt(['done'], 'verified', restoreChanges))).toBeNull();
    });

    it('shows outcomes and the undo note only when the restore was recorded', () => {
        render(<AssistantCommandResult text={restoreReceipt(['restored', 'conflict'], 'verified', restoreChanges)} />);
        const view = screen.getByTestId('command-restore-receipt');
        expect(view.textContent).toContain('commandRestore.outcome.restored');
        expect(view.textContent).toContain('commandRestore.outcome.conflict');
        expect(view.textContent).toContain('commandRestore.undoable');
        render(<AssistantCommandResult text={restoreReceipt(['conflict'], 'verified', { ...skipped, snapshot: { state: 'skipped', reason: 'storage_unavailable' } })} />);
        expect(screen.getAllByTestId('command-restore-receipt')[1].textContent).not.toContain('commandRestore.undoable');
    });
});
