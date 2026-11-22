import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { AlertTriangle } from 'lucide-react';
import { formatLocalTime } from '@/lib/local-time';

/** Tool that puts back the files a command changed under its snapshot roots. */
export const COMMAND_RESTORE_TOOL = 'restore_command_changes';

type ChangeKind = 'added' | 'modified' | 'deleted';
type RootKind = 'cwd' | 'grant';
const CHANGE_KINDS: readonly string[] = ['added', 'modified', 'deleted'];
const RESTORE_OUTCOMES: readonly string[] = ['restored', 'conflict', 'failed', 'outcome_unknown'];
const KNOWN_GAPS: readonly string[] = ['excluded_paths', 'outside_roots', 'binary_files', 'oversized_files',
    'hard_linked_files', 'metadata_changes', 'detached_processes'];
const KNOWN_SKIP_REASONS: readonly string[] = ['file_limit', 'time_limit', 'system_identity', 'root_unavailable',
    'protected_root', 'storage_unavailable'];

/** Approval-card summary of the best-effort snapshot a mutating command's plan covers. */
export interface SnapshotNotice {
    roots: { kind: RootKind; path: string }[];
    excluded_names: string[];
    honors_ignore_files: boolean;
    max_files: number;
    max_file_bytes: number;
    scan_time_limit_ms: number;
    estimated_file_count?: number | null;
    unprotected: string[];
}

export interface CommandRestoreReview {
    sourceResultCallId: string;
    rootKind: RootKind;
    rootPath?: string | null;
    directoryRequestId?: string | null;
    executionStartedAtUnixMs: number;
    executionFinishedAtUnixMs: number;
    files: { path: string; change: ChangeKind }[];
    oneShot: boolean;
}

export type FileChanges = {
    /** Set when no snapshot was taken. */
    skipReason: string | null;
    startedAt: number;
    finishedAt: number;
    /** `cwd` roots carry their path, `grant` roots their directory request id. */
    roots: { kind: RootKind; label: string }[];
    changes: { path: string; root: number; kind: ChangeKind; protected: boolean }[];
    truncated: boolean;
};

export type CommandRestoreReceipt = {
    verified: boolean;
    results: { path: string; outcome: string }[];
    changes: FileChanges;
};

const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
const text = (value: unknown): value is string => typeof value === 'string' && value.length > 0;
const count = (value: unknown, min = 0): value is number => Number.isSafeInteger(value) && (value as number) >= min;
const sha256 = (value: unknown) => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const rootKind = (value: unknown): value is RootKind => value === 'cwd' || value === 'grant';

export function validSnapshotNotice(input: unknown): input is SnapshotNotice {
    if (!record(input)) return false;
    const v = input as unknown as SnapshotNotice;
    return Array.isArray(v.roots) && v.roots.length > 0 && v.roots.every(root => record(root) && rootKind(root.kind) && text(root.path))
        && Array.isArray(v.excluded_names) && v.excluded_names.every(text)
        && typeof v.honors_ignore_files === 'boolean'
        && count(v.max_files, 1) && count(v.max_file_bytes, 1) && count(v.scan_time_limit_ms, 1)
        && (v.estimated_file_count == null || count(v.estimated_file_count))
        && Array.isArray(v.unprotected) && v.unprotected.every(text);
}

export function validCommandRestoreReview(input: unknown): input is CommandRestoreReview {
    if (!record(input)) return false;
    const v = input as unknown as CommandRestoreReview;
    const root = v.rootKind === 'cwd' ? text(v.rootPath) && v.directoryRequestId == null
        : v.rootKind === 'grant' && text(v.directoryRequestId) && v.rootPath == null;
    return text(v.sourceResultCallId) && v.oneShot === true && root
        && count(v.executionStartedAtUnixMs, 1) && count(v.executionFinishedAtUnixMs, v.executionStartedAtUnixMs)
        && Array.isArray(v.files) && v.files.length > 0
        && v.files.every(file => record(file) && text(file.path) && CHANGE_KINDS.includes(file.change as string));
}

/** A `restore_command_changes` request cannot be approved without its exact review. */
export function commandRestoreApprovalBlocked(item: { toolName: string; commandRestoreConfirmation?: unknown }): boolean {
    return item.toolName === COMMAND_RESTORE_TOOL && !validCommandRestoreReview(item.commandRestoreConfirmation);
}

/** A device change report (`ExecFileChanges`); null when its shape is inconsistent. */
export function parseFileChanges(value: unknown): FileChanges | null {
    if (!record(value) || !record(value.snapshot)) return null;
    const { snapshot } = value;
    const skipReason = snapshot.state === 'taken' ? null : snapshot.state === 'skipped' && text(snapshot.reason) ? snapshot.reason : undefined;
    if (skipReason === undefined || !count(value.started_at_unix_ms, 1) || !count(value.finished_at_unix_ms, value.started_at_unix_ms as number)
        || typeof value.truncated !== 'boolean' || !Array.isArray(value.roots) || !Array.isArray(value.changes)) return null;
    const roots: FileChanges['roots'] = [];
    for (const entry of value.roots) {
        const root = record(entry) ? entry.root : null;
        if (!record(root)) return null;
        if (root.kind === 'cwd' && text(root.path)) roots.push({ kind: 'cwd', label: root.path });
        else if (root.kind === 'grant' && text(root.directory_request_id)) roots.push({ kind: 'grant', label: root.directory_request_id });
        else return null;
    }
    const changes: FileChanges['changes'] = [];
    for (const change of value.changes) {
        if (!record(change) || !text(change.path) || !count(change.root) || (change.root as number) >= roots.length
            || !CHANGE_KINDS.includes(change.kind as string) || !count(change.size_bytes) || typeof change.protected !== 'boolean') return null;
        changes.push({ path: change.path, root: change.root as number, kind: change.kind as ChangeKind, protected: change.protected });
    }
    if (skipReason === null ? roots.length === 0 || !sha256(value.recovery_id)
        : value.recovery_id != null || roots.length > 0 || changes.length > 0 || value.truncated) return null;
    return { skipReason, startedAt: value.started_at_unix_ms as number, finishedAt: value.finished_at_unix_ms as number,
        roots, changes, truncated: value.truncated };
}

/** Device receipt of a `restore_command_changes` call. */
export function parseCommandRestoreReceipt(raw: string): CommandRestoreReceipt | null {
    try {
        const completion: unknown = JSON.parse(raw);
        if (!record(completion) || !record(completion.output) || completion.output.kind !== 'command_changes_restore'
            || !record(completion.output.value)) return null;
        const value = completion.output.value;
        if (!Array.isArray(value.results) || value.results.length === 0) return null;
        const results: CommandRestoreReceipt['results'] = [];
        for (const result of value.results) {
            if (!record(result) || !text(result.path) || !RESTORE_OUTCOMES.includes(result.outcome as string)) return null;
            results.push({ path: result.path, outcome: result.outcome as string });
        }
        const changes = parseFileChanges(value.file_changes);
        // A restore reports against the single root it was dispatched to.
        if (!changes || changes.roots.length > 1) return null;
        const verified = results.every(result => result.outcome !== 'outcome_unknown');
        if (completion.result !== (verified ? 'verified' : 'outcome_unknown')) return null;
        return { verified, results, changes };
    } catch { return null; }
}

/** A byte limit in the largest binary unit that divides it, such as `16 MiB`. */
export function byteLimitLabel(bytes: number): string {
    for (const [unit, size] of [['GiB', 2 ** 30], ['MiB', 2 ** 20], ['KiB', 2 ** 10]] as const) {
        if (bytes > 0 && bytes % size === 0) return `${bytes / size} ${unit}`;
    }
    return `${bytes} B`;
}

const localTime = (unixMs: number) => formatLocalTime(new Date(unixMs).toISOString());
const warning = 'flex items-start gap-2 rounded-md border border-red-500/50 bg-red-500/5 p-2 font-semibold text-red-700 dark:text-red-300';

export function CommandSnapshotNotice({ value }: { value: SnapshotNotice }) {
    const { t } = useTranslation();
    return <div data-testid="command-snapshot" className="space-y-1 rounded-md border p-2">
        <p className="font-medium">{t('pages.aiAgent.commandSnapshot.title')}</p>
        <p>{t('pages.aiAgent.commandSnapshot.intro')}</p>
        <ul className="break-all font-mono">
            {value.roots.map((root, index) => <li key={index}>{t(`pages.aiAgent.commandSnapshot.root.${root.kind}`, { path: root.path })}</li>)}
        </ul>
        {value.excluded_names.length > 0 && <p className="break-words">{t('pages.aiAgent.commandSnapshot.excluded', { names: value.excluded_names.join(', ') })}</p>}
        {value.honors_ignore_files && <p>{t('pages.aiAgent.commandSnapshot.ignoreFiles')}</p>}
        <p>{t('pages.aiAgent.commandSnapshot.limits', { files: value.max_files, size: byteLimitLabel(value.max_file_bytes),
            seconds: Math.ceil(value.scan_time_limit_ms / 1000) })}</p>
        {value.estimated_file_count != null && <p>{t('pages.aiAgent.commandSnapshot.estimate', { count: value.estimated_file_count })}</p>}
        {value.unprotected.length > 0 && <div>
            <p>{t('pages.aiAgent.commandSnapshot.unprotected')}</p>
            <ul className="list-disc pl-5">
                {value.unprotected.map(gap => <li key={gap}>{t(`pages.aiAgent.commandSnapshot.gap.${KNOWN_GAPS.includes(gap) ? gap : 'unknown'}`)}</li>)}
            </ul>
        </div>}
        <p role="alert" className={warning}><AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />{t('pages.aiAgent.commandSnapshot.warning')}</p>
    </div>;
}

const rootLabel = (t: TFunction, root: FileChanges['roots'][number]) =>
    t(`pages.aiAgent.commandChanges.root.${root.kind}`, { value: root.label });

/** What a command (or a restore) changed: grouped by root, or the reason nothing is protected. */
export function CommandFileChangesView({ value }: { value: FileChanges }) {
    const { t } = useTranslation();
    if (value.skipReason !== null) {
        return <div data-testid="command-file-changes" className="space-y-1">
            <p className="font-medium text-red-700 dark:text-red-300">{t('pages.aiAgent.commandChanges.skipped')}</p>
            <p className="text-muted-foreground">{t(`pages.aiAgent.commandChanges.skipReason.${KNOWN_SKIP_REASONS.includes(value.skipReason) ? value.skipReason : 'unknown'}`)}</p>
        </div>;
    }
    const groups = value.roots.map((root, index) => ({ root, changes: value.changes.filter(change => change.root === index) }))
        .filter(group => group.changes.length > 0);
    return <div data-testid="command-file-changes" className="space-y-2">
        <p className="font-medium">{t('pages.aiAgent.commandChanges.title')}</p>
        <p className="text-muted-foreground">{t('pages.aiAgent.commandChanges.window', { start: localTime(value.startedAt), end: localTime(value.finishedAt) })}</p>
        {groups.length === 0 && <p>{t('pages.aiAgent.commandChanges.none')}</p>}
        {groups.map(({ root, changes }, index) => <div key={index} className="space-y-1">
            <p className="break-all font-mono text-xs">{rootLabel(t, root)}</p>
            <ul className="max-h-72 space-y-0.5 overflow-auto">
                {changes.map(change => <li key={change.path} data-testid="command-file-change" className="flex flex-wrap items-baseline gap-x-2">
                    <span className="shrink-0">{t(`pages.aiAgent.commandChanges.kind.${change.kind}`)}</span>
                    <span className="min-w-0 flex-1 break-all font-mono text-xs">{change.path}</span>
                    <span className={change.protected ? 'text-muted-foreground' : 'text-amber-700 dark:text-amber-300'}>
                        {t(`pages.aiAgent.commandChanges.${change.protected ? 'restorable' : 'notProtected'}`)}</span>
                </li>)}
            </ul>
        </div>)}
        {value.truncated && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAgent.commandChanges.truncated')}</p>}
        <ul className="list-disc space-y-0.5 pl-5 text-xs text-muted-foreground">
            {(['otherPrograms', 'excluded', 'detached'] as const).map(note => <li key={note}>{t(`pages.aiAgent.commandChanges.note.${note}`)}</li>)}
        </ul>
    </div>;
}

export function CommandRestoreConfirmationCard({ value }: { value: CommandRestoreReview }) {
    const { t } = useTranslation();
    const root = value.rootKind === 'cwd' ? { kind: 'cwd' as const, label: value.rootPath ?? '' } : { kind: 'grant' as const, label: value.directoryRequestId ?? '' };
    return <div data-testid="command-restore-confirmation" className="mt-3 max-w-full space-y-2 rounded-md border p-3 text-xs">
        <p className="font-semibold">{t('pages.aiAgent.commandRestore.title')}</p>
        <p role="alert" className={warning}><AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />{t('pages.aiAgent.commandRestore.warning')}</p>
        <p className="break-all font-mono">{rootLabel(t, root)}</p>
        <p>{t('pages.aiAgent.commandChanges.window', { start: localTime(value.executionStartedAtUnixMs), end: localTime(value.executionFinishedAtUnixMs) })}</p>
        <ul className="max-h-72 space-y-0.5 overflow-auto">
            {value.files.map(file => <li key={file.path} className="flex flex-wrap items-baseline gap-x-2">
                <span className="min-w-0 flex-1 break-all font-mono">{file.path}</span>
                <span className="shrink-0">{t(`pages.aiAgent.commandRestore.action.${file.change}`)}</span>
            </li>)}
        </ul>
        <p>{t('pages.aiAgent.commandRestore.conflicts')}</p>
        <p>{t('pages.aiAgent.fileConfirmOneShot')}</p>
        <p className="break-all">source_result_call_id: {value.sourceResultCallId}</p>
    </div>;
}

export function CommandRestoreReceiptView({ receipt }: { receipt: CommandRestoreReceipt }) {
    const { t } = useTranslation();
    return <div data-testid="command-restore-receipt" className="space-y-2">
        <ul className="max-h-72 space-y-0.5 overflow-auto">
            {receipt.results.map(result => <li key={result.path} className="flex flex-wrap items-baseline gap-x-2">
                <span className="min-w-0 flex-1 break-all font-mono text-xs">{result.path}</span>
                <span className={result.outcome === 'restored' ? 'text-emerald-700 dark:text-emerald-300' : 'text-red-700 dark:text-red-300'}>
                    {t(`pages.aiAgent.commandRestore.outcome.${result.outcome}`)}</span>
            </li>)}
        </ul>
        {receipt.changes.skipReason === null && <p>{t('pages.aiAgent.commandRestore.undoable')}</p>}
        {!receipt.verified && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAgent.fileReceipt.unknownHint')}</p>}
    </div>;
}
