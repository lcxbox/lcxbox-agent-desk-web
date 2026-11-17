import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { formatLocalTime } from '@/lib/local-time';
import { commandRestoreApprovalBlocked, COMMAND_RESTORE_TOOL } from './assistant-command-changes';
import { ELLIPSIS, inlineTextDiff, textPreviewHead, type DiffSegment } from './text-edit-diff';

type Operation = 'update' | 'delete' | 'restore';
export type TextEditReview = { old_text: string; new_text: string; replace_all?: boolean };

interface TextFileReview {
    fileName: string;
    sourceResultCallId: string;
    expectedSha256?: string | null;
    operation: Operation;
    edits?: TextEditReview[] | null;
    write?: string | null;
    restore?: {
        originalOperation: Operation;
        backupCreatedAtUnixMs: number;
        overridesNonAiVersion: boolean;
        fileExpectedToExist: boolean;
    } | null;
    oneShot: boolean;
    recoverable: boolean;
}

const MAX_EDITS = 32;
const TOOL_OPERATIONS: Record<string, Operation> = {
    update_text_file: 'update', delete_text_file: 'delete', restore_text_file: 'restore',
};
const sha256 = (value: unknown) => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const operation = (value: unknown): value is Operation => value === 'update' || value === 'delete' || value === 'restore';

/** Exact edits of an update: 1 to 32 non-empty matches, as the device accepts them. */
export function validTextEdits(value: unknown): value is TextEditReview[] {
    return Array.isArray(value) && value.length >= 1 && value.length <= MAX_EDITS && value.every(edit =>
        edit && typeof edit === 'object' && typeof edit.old_text === 'string' && edit.old_text.length > 0
        && typeof edit.new_text === 'string' && (edit.replace_all === undefined || typeof edit.replace_all === 'boolean'));
}

export function validTextFileReview(input: unknown): input is TextFileReview {
    if (!input || typeof input !== 'object') return false;
    const v = input as TextFileReview;
    if (typeof v.fileName !== 'string' || !v.fileName || typeof v.sourceResultCallId !== 'string' || !v.sourceResultCallId
        || !operation(v.operation) || v.oneShot !== true || v.recoverable !== true) return false;
    if (v.operation === 'restore') {
        const restore = v.restore;
        return v.edits == null && v.write == null && (v.expectedSha256 == null || sha256(v.expectedSha256))
            && !!restore && typeof restore === 'object' && operation(restore.originalOperation)
            && Number.isSafeInteger(restore.backupCreatedAtUnixMs) && restore.backupCreatedAtUnixMs > 0
            && typeof restore.overridesNonAiVersion === 'boolean' && typeof restore.fileExpectedToExist === 'boolean';
    }
    if (!sha256(v.expectedSha256) || v.restore != null) return false;
    if (v.operation === 'delete') return v.edits == null && v.write == null;
    return v.write == null ? validTextEdits(v.edits) : typeof v.write === 'string' && v.edits == null;
}

export function fileApprovalBlocked(item: { toolName: string; textFileConfirmation?: unknown; commandRestoreConfirmation?: unknown }): boolean {
    if (item.toolName === COMMAND_RESTORE_TOOL) return commandRestoreApprovalBlocked(item);
    const expected = TOOL_OPERATIONS[item.toolName];
    if (!expected) return false;
    return !validTextFileReview(item.textFileConfirmation) || item.textFileConfirmation.operation !== expected;
}

function DiffBlock({ segments, removed }: { segments: DiffSegment[]; removed: boolean }) {
    const { t } = useTranslation();
    const mark = removed ? 'bg-red-500/30 text-red-900 dark:text-red-100' : 'bg-green-500/30 text-green-900 dark:text-green-100';
    return <pre data-diff={removed ? 'removed' : 'added'} className={`max-h-60 overflow-auto whitespace-pre-wrap break-words rounded border-l-2 p-2 font-mono ${removed ? 'border-red-500 bg-red-500/10' : 'border-green-500 bg-green-500/10'}`}>
        {segments.length === 0 ? <span className="text-muted-foreground">{t('pages.aiAssistant.fileConfirmEmpty')}</span>
            : segments.map((segment, index) => segment.changed
                ? <mark key={index} className={`rounded-sm ${mark}`}>{segment.text}</mark>
                : <span key={index} className="text-muted-foreground">{segment.text}</span>)}
    </pre>;
}

/** Exact edits as red/green blocks with the shared context collapsed, or the complete new content. */
export function TextFileChangeView({ edits, write }: { edits?: TextEditReview[] | null; write?: string | null }) {
    const { t } = useTranslation();
    const diffs = useMemo(() => edits?.map(edit => inlineTextDiff(edit.old_text, edit.new_text)) ?? [], [edits]);
    const head = useMemo(() => write == null ? null : textPreviewHead(write), [write]);
    const [expanded, setExpanded] = useState(false);
    return <div className="space-y-2">
        {edits?.map((edit, index) => {
            const heading = [edits.length > 1 ? t('pages.aiAssistant.fileConfirmEditIndex', { index: index + 1, count: edits.length }) : null,
                edit.replace_all ? t('pages.aiAssistant.fileConfirmReplaceEvery') : null].filter(Boolean).join(' · ');
            return <div key={index} data-testid="text-edit" className="space-y-1">
                {heading && <p className="font-medium">{heading}</p>}
                <DiffBlock segments={diffs[index].removed} removed />
                <DiffBlock segments={diffs[index].added} removed={false} />
            </div>;
        })}
        {write != null && <div className="space-y-1">
            <p className="font-medium">{t('pages.aiAssistant.fileConfirmReplaceAll')}</p>
            <pre data-diff="write" className="max-h-60 overflow-auto whitespace-pre-wrap break-words rounded border-l-2 border-green-500 bg-green-500/10 p-2 font-mono">
                {write ? (head === null || expanded ? write : head + ELLIPSIS) : <span className="text-muted-foreground">{t('pages.aiAssistant.fileConfirmEmpty')}</span>}
            </pre>
            {head !== null && <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs" onClick={() => setExpanded(value => !value)}>
                {t(expanded ? 'pages.aiAssistant.fileConfirmShowLess' : 'pages.aiAssistant.fileConfirmShowAll')}</Button>}
        </div>}
    </div>;
}

const TITLE: Record<Operation, string> = {
    update: 'pages.aiAssistant.fileConfirmUpdate',
    delete: 'pages.aiAssistant.fileConfirmDelete',
    restore: 'pages.aiAssistant.fileConfirmRestore',
};

export function TextFileConfirmationCard({ value }: { value: TextFileReview }) {
    const { t } = useTranslation();
    const restore = value.restore;
    return <div data-testid="text-file-confirmation" className="mt-3 max-w-full space-y-2 rounded-md border p-3 text-xs">
        <p className="font-semibold">{t(TITLE[value.operation])}: {value.fileName}</p>
        <p>{t('pages.aiAssistant.fileConfirmOneShot')}</p>
        {value.operation === 'delete' && <p>{t('pages.aiAssistant.fileConfirmRecoverable')}</p>}
        {value.operation === 'update' && <TextFileChangeView edits={value.edits} write={value.write} />}
        {value.operation === 'restore' && restore && <>
            <p>{t(`pages.aiAssistant.fileConfirmRestoreUndo.${restore.originalOperation}`)}</p>
            <p>{t('pages.aiAssistant.fileConfirmRestoreBackupTime', { time: formatLocalTime(new Date(restore.backupCreatedAtUnixMs).toISOString()) })}</p>
            {!restore.fileExpectedToExist && <p>{t('pages.aiAssistant.fileConfirmRestoreRecreate')}</p>}
            {restore.overridesNonAiVersion && <p role="alert" data-testid="restore-overrides" className="flex items-start gap-2 rounded-md border border-red-500/50 bg-red-500/5 p-2 font-semibold text-red-700 dark:text-red-300">
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />{t('pages.aiAssistant.fileConfirmRestoreOverrides')}</p>}
        </>}
        {value.expectedSha256 && <p className="break-all">SHA-256: {value.expectedSha256}</p>}
        <p className="break-all">source_result_call_id: {value.sourceResultCallId}</p>
    </div>;
}
