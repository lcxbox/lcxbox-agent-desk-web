import type { AiAgentToolActivity } from './use-ai-agent-chat';
import { Button } from '@/components/ui/button';
import { AgentCodeBlock } from './agent-code-block';
import { Disclosure } from '@/components/ui/disclosure';
import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { AgentFileResult, parseFileReceipt, parseTextFileChange } from './agent-file-result';
import { hasUnknownOutcome } from './action-result-status';
import { AgentLaunchResult, parseLaunchReceipt } from './agent-launch-result';
import { CommandFileChangesView, CommandRestoreReceiptView, parseCommandRestoreReceipt, parseFileChanges, type FileChanges } from './agent-command-changes';
import { CommandSandboxDiagnostic, sandboxDiagnostic, type SandboxDiagnostic } from './agent-command-sandbox';

type CommandReceipt = {
    exit_code: number | null;
    failure?: { message: string; kind: string } | null;
    termination_signal?: number | null;
    duration_ms: number;
    streams: { type: 'split'; stdout: string; stderr: string; stdout_truncated: boolean; stderr_truncated: boolean }
        | { type: 'pty_combined'; terminal: string; truncated: boolean };
    redactions?: string[];
    /** Files changed under the snapshot roots of a mutating command. */
    fileChanges: FileChanges | null;
    /** Command-sandbox note the device attached to the result. */
    sandboxNote: SandboxDiagnostic | null;
};

function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function parseCommandReceipt(text: string): CommandReceipt | null {
    try {
        const envelope: unknown = JSON.parse(text);
        if (!isRecord(envelope) || !isRecord(envelope.Exec)) return null;
        const value = envelope.Exec;
        const streams = value.streams;
        if ((value.exit_code !== null && !Number.isSafeInteger(value.exit_code)) || !Number.isSafeInteger(value.duration_ms)
            || (value.duration_ms as number) < 0 || !isRecord(streams)) return null;
        if (value.failure != null && (!isRecord(value.failure) || typeof value.failure.message !== 'string' || typeof value.failure.kind !== 'string')) return null;
        if (value.termination_signal != null && !Number.isSafeInteger(value.termination_signal)) return null;
        if (value.redactions !== undefined && (!Array.isArray(value.redactions)
            || !value.redactions.every(item => typeof item === 'string'))) return null;
        if (streams.type === 'split') {
            if (typeof streams.stdout !== 'string' || typeof streams.stderr !== 'string'
                || typeof streams.stdout_truncated !== 'boolean' || typeof streams.stderr_truncated !== 'boolean') return null;
        } else if (streams.type === 'pty_combined') {
            if (typeof streams.terminal !== 'string' || typeof streams.truncated !== 'boolean') return null;
        } else return null;
        const fileChanges = value.file_changes == null ? null : parseFileChanges(value.file_changes);
        if (value.file_changes != null && !fileChanges) return null;
        return { ...value, fileChanges, sandboxNote: sandboxDiagnostic(value.diagnostics, value.failure) } as CommandReceipt;
    } catch {
        return null;
    }
}

type ReadWindow = { fileName: string; totalLines: number; startLine: number; endLine: number; truncated: boolean; matches: number | null };

/** Window facts of a `read_text_file` result: the selected lines or the search matches. */
export function parseReadWindow(text: string): ReadWindow | null {
    try {
        const envelope: unknown = JSON.parse(text);
        const value = isRecord(envelope) && isRecord(envelope.ReadContext) ? envelope.ReadContext.FileContentRead : null;
        if (!isRecord(value) || typeof value.display_name !== 'string' || typeof value.content_utf8 !== 'string'
            || typeof value.truncated !== 'boolean' || !Array.isArray(value.matches)
            || ![value.total_lines, value.start_line, value.end_line].every(n => Number.isSafeInteger(n) && (n as number) >= 0)) return null;
        // A search returns no window; its matches carry the text.
        const search = value.start_line === 0 && value.content_utf8 === '' && (value.matches.length > 0 || (value.total_lines as number) > 0);
        return { fileName: value.display_name, totalLines: value.total_lines as number, startLine: value.start_line as number,
            endLine: value.end_line as number, truncated: value.truncated, matches: search ? value.matches.length : null };
    } catch {
        return null;
    }
}

export function AgentCommandResult({ text, tool, onLocateCall, onExportBackup }: {
    text: string;
    tool?: AiAgentToolActivity;
    onLocateCall?: () => void;
    onExportBackup?: (id: string) => Promise<void>;
}) {
    const { t, i18n } = useTranslation();
    const command = useMemo(() => {
        if (tool?.name !== 'exec_command') return null;
        try {
            const args: unknown = JSON.parse(tool.argumentsJson);
            return isRecord(args) && typeof args.command === 'string' && typeof args.shell === 'string'
                ? { text: args.command, shell: args.shell } : null;
        } catch { return null; }
    }, [tool]);
    const receipt = useMemo(() => parseCommandReceipt(text), [text]);
    const fileReceipt = useMemo(() => parseFileReceipt(text), [text]);
    const fileChange = useMemo(() => tool?.name === 'update_text_file' ? parseTextFileChange(tool.argumentsJson) : null, [tool]);
    const launchReceipt = useMemo(() => parseLaunchReceipt(text), [text]);
    const outcomeUnknown = useMemo(() => hasUnknownOutcome(text), [text]);
    const readWindow = useMemo(() => parseReadWindow(text), [text]);
    const restoreReceipt = useMemo(() => parseCommandRestoreReceipt(text), [text]);
    if (launchReceipt) return <AgentLaunchResult receipt={launchReceipt} text={text} />;
    if (restoreReceipt) return <Disclosure className="min-w-0" title={<>{t('pages.aiAgent.commandRestore.receiptTitle')} · {t(`pages.aiAgent.fileReceipt.${restoreReceipt.verified ? 'verified' : 'unknown'}`)}</>} summaryClassName="cursor-pointer font-medium">
        <div className="mt-3 space-y-3">
            <CommandRestoreReceiptView receipt={restoreReceipt} />
            <Disclosure title={<>{t('pages.aiAgent.commandReceipt.raw')}</>} summaryClassName="cursor-pointer text-xs"><AgentCodeBlock text={text} /></Disclosure>
        </div>
    </Disclosure>;
    if (fileReceipt) return <AgentFileResult receipt={fileReceipt} text={text} change={fileChange} onExportBackup={onExportBackup} />;
    const output = (label: string, content: string, truncated: boolean) => (
        <div className="min-w-0 space-y-1">
            <AgentCodeBlock label={label} text={content || t('pages.aiAgent.commandReceipt.empty')} format="text" />
            {truncated && <p className="text-xs text-muted-foreground">{t('pages.aiAgent.commandReceipt.truncated')}</p>}
        </div>
    );
    const duration = receipt ? t(`pages.aiAgent.commandReceipt.${receipt.duration_ms >= 60000 ? 'minutes' : receipt.duration_ms >= 1000 ? 'seconds' : 'milliseconds'}`, {
        value: (receipt.duration_ms / (receipt.duration_ms >= 60000 ? 60000 : receipt.duration_ms >= 1000 ? 1000 : 1)).toLocaleString(i18n.language, { maximumFractionDigits: 2 }),
    }) : '';
    return (
        <div className="min-w-0 space-y-2">
            {command && <div className="min-w-0 space-y-1">
                <p className="text-xs text-muted-foreground">{command.shell}</p>
                <p className="truncate font-mono text-xs" title={command.text}>{command.text.split(/\r?\n/).find(line => line.trim())?.slice(0, 160)}</p>
                <Disclosure title={t('pages.aiAgent.commandReceipt.command')}>
                    <AgentCodeBlock text={command.text} format="text" />
                </Disclosure>
                {onLocateCall && <Button variant="link" size="sm" className="h-auto p-0" onClick={onLocateCall}>{t('pages.aiAgent.commandReceipt.locate')}</Button>}
            </div>}
            <Disclosure className="min-w-0" title={<>
                    {t('pages.aiAgent.commandResultTitle')}
                    {readWindow && <span data-testid="read-window" className="font-normal text-muted-foreground"> · {readWindow.fileName} · {readWindow.matches !== null
                        ? t('pages.aiAgent.readWindow.matches', { count: readWindow.matches })
                        : t('pages.aiAgent.readWindow.lines', { start: readWindow.startLine, end: readWindow.endLine, total: readWindow.totalLines })}
                        {readWindow.truncated && <> · {t('pages.aiAgent.readWindow.truncated')}</>}</span>}
                    {outcomeUnknown && <> · {t('pages.aiAgent.toolCall.inspectBeforeRetry')}</>}
                </>} summaryClassName="cursor-pointer rounded font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">

                <div className="mt-3 space-y-3">
                    {receipt ? <>
                        <dl className="flex flex-wrap gap-x-6 gap-y-2">
                            <div><dt className="text-muted-foreground">{t('pages.aiAgent.commandReceipt.exitCode')}</dt><dd>{receipt.exit_code ?? '—'}</dd></div>
                            <div><dt className="text-muted-foreground">{t('pages.aiAgent.commandReceipt.duration')}</dt><dd>{duration}</dd></div>
                        </dl>
                        {receipt.failure && <p role="alert" className="whitespace-pre-wrap break-words text-destructive">{receipt.failure.message}</p>}
                        {receipt.sandboxNote && <CommandSandboxDiagnostic value={receipt.sandboxNote} />}
                        {receipt.fileChanges && <CommandFileChangesView value={receipt.fileChanges} />}
                        {receipt.streams.type === 'split' ? <>
                            {output(t('pages.aiAgent.commandReceipt.stdout'), receipt.streams.stdout, receipt.streams.stdout_truncated)}
                            {output(t('pages.aiAgent.commandReceipt.stderr'), receipt.streams.stderr, receipt.streams.stderr_truncated)}
                        </> : output(t('pages.aiAgent.commandReceipt.terminal'), receipt.streams.terminal, receipt.streams.truncated)}
                        {!!receipt.redactions?.length && output(t('pages.aiAgent.commandReceipt.redactions'), receipt.redactions.join('\n'), false)}
                        <Disclosure title={<>{t('pages.aiAgent.commandReceipt.raw')}</>} summaryClassName="cursor-pointer text-xs text-muted-foreground">

                            <AgentCodeBlock text={text} />
                        </Disclosure>
                    </> : <AgentCodeBlock text={text} />}
                    <p className="text-xs text-muted-foreground">{t('pages.aiAgent.commandResultHint')}</p>
                </div>
            </Disclosure>
        </div>
    );
}
