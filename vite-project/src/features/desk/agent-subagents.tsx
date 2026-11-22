import { ApprovalReviews } from '@/features/approval/reviews';
import { useFollowLatest } from '@/hooks/use-follow-latest';
import { AgentTranscript } from './agent-transcript';
import { AgentContextMeter } from './agent-context-meter';
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot, ClipboardList, ArrowDown, Paperclip, FolderKey, Layers, ListTodo, ListTree, ShieldCheck, ChevronLeft, ChevronRight, ListFilter, Plus } from 'lucide-react';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Disclosure } from '@/components/ui/disclosure';
import { AgentComposerInput } from './agent-composer-input';
import { MarkdownContent } from '@/components/markdown-content';
import { formatLocalTime } from '@/lib/local-time';
import { AgentPermissionRequest } from './agent-permission-request';
import { AgentDirectoryApproval } from './agent-directory-approval';
import { ownerPendingDirectories } from './agent-decision-receipt';
import { AgentAttachments } from './agent-attachments';
import { AgentPermissionRecords, permissionIsAutomaticallyReviewed } from './agent-permission-records';
import { AgentFileScope } from './agent-file-scope';
import { AgentBackgroundTasks } from './agent-background-tasks';
import { AgentDetailsSheet } from './agent-details-sheet';
import { AgentComposerTools } from './agent-composer-tools';
import { AgentComposerMenu } from './agent-composer-menu';
import { AgentComposerStatus } from './agent-composer-status';
import { permissionToolLabel, permissionOperationLabel } from './agent-permission-labels';
import { subagentIsTerminal, useAiAgentSubagents } from './use-ai-agent-subagents';
import type { InteractiveCommandContext } from './agent-interactive-command';

export function AgentSubagentApprovalNotice({ agents }: {
    agents: Pick<ReturnType<typeof useAiAgentSubagents>, 'tasks' | 'openDetail'>;
}) {
    const { t } = useTranslation();
    const waiting = agents.tasks.filter(task => task.state === 'waiting_approval');
    if (waiting.length === 0) return null;
    return <div role="status" className="flex min-w-0 shrink-0 flex-wrap items-center gap-2 rounded-md border border-amber-500/50 bg-amber-500/5 p-2 text-sm">
        <span className="flex items-center gap-2"><ClipboardList className="h-4 w-4 shrink-0" aria-hidden="true" />
            {t('pages.aiAgent.subagents.approvalCount', { count: waiting.length })}</span>
        {waiting.map(task => <Button key={task.task_id} type="button" size="sm" variant="outline"
            className="h-auto max-w-full whitespace-normal break-words text-left"
            onClick={() => agents.openDetail(task)}>
            {t('pages.aiAgent.subagents.viewApproval', { name: task.name })}
        </Button>)}
    </div>;
}

export type AgentSubagentPanel = 'details' | 'capabilities' | 'context' | 'attachments' | 'directories' | 'permissions' | 'tasks';

export function AgentSubagentMenu({ onSelect, onDeviceSettings, onApprovalSettings }: {
    onSelect: (panel: AgentSubagentPanel) => void; onDeviceSettings?: () => void; onApprovalSettings?: () => void;
}) {
    const { t } = useTranslation();
    return <AgentComposerMenu sections={[
        { label: t('pages.aiAgent.workspace.resources'), actions: [
            { label: t('pages.aiAgent.attachments.title'), icon: Paperclip, onSelect: () => onSelect('attachments') },
            { label: t('pages.aiAgent.contextTitle'), icon: Layers, onSelect: () => onSelect('context') },
            { label: t('pages.aiAgent.directories.title'), icon: FolderKey, onSelect: () => onSelect('directories') },
        ] },
        { label: t('pages.aiAgent.workspace.manage'), actions: [
            { label: t('pages.aiAgent.tasks.title'), icon: ListTodo, onSelect: () => onSelect('tasks') },
            { label: t('pages.aiAgent.permissionHistory'), icon: ShieldCheck, onSelect: () => onSelect('permissions') },
            ...(onApprovalSettings ? [{ label: t('pages.aiAgent.autoApprovalTitle'), icon: Bot, onSelect: onApprovalSettings }] : []),
            ...(onDeviceSettings ? [{ label: t('pages.aiAgent.workspace.deviceSettings'), onSelect: onDeviceSettings }] : []),
        ] },
        { label: t('pages.aiAgent.workspace.troubleshoot'), actions: [
            { label: t('pages.aiAgent.workspace.details'), icon: ListTree, onSelect: () => onSelect('details') },
            { label: t('pages.aiAgent.workspace.capabilities'), onSelect: () => onSelect('capabilities') },
        ] },
    ]} />;
}

export function AgentSubagents({ agents, connected, canDecide, interactive, exportBackup, panel: externalPanel,
    onPanelChange, deskId, sessionTargetId, capabilityList, adjustment: externalAdjustment, onAdjustmentChange,
    onDeviceSettings, onApprovalSettings, focusPermissionId }: {
    agents: ReturnType<typeof useAiAgentSubagents>; connected: boolean; canDecide: boolean;
    interactive?: Omit<InteractiveCommandContext, 'runId'>;
    mainStopped: boolean; exportBackup?: (session: string, id: string) => Promise<void>;
    panel?: AgentSubagentPanel | null; onPanelChange?: (panel: AgentSubagentPanel | null) => void;
    deskId?: string; sessionTargetId?: string | null;
    capabilityList?: ReactNode;
    adjustment?: string; onAdjustmentChange?: (text: string) => void;
    onDeviceSettings?: () => void; onApprovalSettings?: () => void;
    focusPermissionId?: string;
}) {
    const { t } = useTranslation();
    const [localAdjustment, setLocalAdjustment] = useState('');
    const adjustment = externalAdjustment ?? localAdjustment;
    const question = agents.detail?.result.open_question;
    const [replyDraft, setReplyDraft] = useState<{ questionId: string; text: string }>();
    const replyText = question && replyDraft?.questionId === question.question_id ? replyDraft.text : '';
    const draft = question ? replyText : adjustment;
    const draftValid = !!draft.trim() && (!question || new TextEncoder().encode(draft.trim()).length <= 4096);
    const sendDisabled = !draftValid || !canDecide || !connected || agents.updating;
    const setAdjustment = onAdjustmentChange ?? setLocalAdjustment;
    const [localPanel, setLocalPanel] = useState<AgentSubagentPanel | null>(null);
    const panel = externalPanel === undefined ? localPanel : externalPanel;
    const setPanel = onPanelChange ?? setLocalPanel;
    const task = agents.detail?.result.task ?? agents.selected;
    useEffect(() => { setLocalAdjustment(''); setLocalPanel(null); setReplyDraft(undefined); }, [task?.task_id]);
    const child = agents.detail?.session;
    const report = agents.detail?.result.report;
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } = useFollowLatest(true, child?.sessionId ?? task?.child_session_id ?? '');
    const focusedPermission = useRef<string | undefined>(undefined);
    useEffect(() => {
        if (!focusPermissionId || focusedPermission.current === focusPermissionId) return;
        const card = [...(contentRef.current?.querySelectorAll<HTMLElement>('[data-permission-id]') ?? [])]
            .find(element => element.dataset.permissionId === focusPermissionId);
        if (card) {
            focusedPermission.current = focusPermissionId;
            card.scrollIntoView?.({ block: 'center' });
            card.focus({ preventScroll: true });
        }
    }, [child?.sessionId, child?.permissionRequests, contentRef, focusPermissionId]);
    if (!task) return null;
    const automaticApproval = child?.approvalDelegation?.status === 'active';
    const pendingDirectories = ownerPendingDirectories(child?.fileScope?.directories ?? [], child?.decisionPause, automaticApproval);
    const pendingPermissions = child?.permissionRequests?.filter(request => ['pending', 'needs_revalidation'].includes(request.state)
        && !permissionIsAutomaticallyReviewed(request, automaticApproval)) ?? [];
    const runningTaskCount = [...(child?.commandTasks ?? []), ...(child?.backgroundTasks ?? [])]
        .filter(work => ['running', 'cancel_requested', 'outcome_unknown'].includes(work.state)).length;
    const jumpToPending = (id?: string) => {
        const target = id ? contentRef.current?.querySelector<HTMLElement>(`[id="agent-directory-${id}"]`)
            : contentRef.current?.querySelector<HTMLElement>('[data-agent-pending]');
        target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true });
    };
    const detailsContent = <div className="space-y-4">
                    {task && <>
                        <Badge variant="outline">{t(question && task.state === 'waiting_input'
                            ? 'pages.aiAgentAttention.reasons.worker_question' : `pages.aiAgent.subagents.states.${task.state}`)}</Badge>
                        <p className="text-xs text-muted-foreground">{t(`pages.aiAgent.subagents.sources.${task.source.kind}`)} · {formatLocalTime(task.updated_at)}</p>
                        {task.source.kind === 'user_input' && <p className="text-xs text-muted-foreground">
                            {t('pages.aiAgent.subagents.sourceInput', { revision: task.source.input_revision })}
                        </p>}
                        {task.source.kind === 'goal' && <p className="break-all text-xs text-muted-foreground">{task.source.goal_id}</p>}
                        {task.source.kind === 'scheduled_occurrence' && <p className="break-all text-xs text-muted-foreground">{task.source.schedule_id} · {task.source.occurrence_id}</p>}
                        {task.wait_reason && <p className="text-sm">{t(`pages.aiAgent.subagents.waitReasons.${task.wait_reason}`)}</p>}
                        <Disclosure title={t('pages.aiAgent.subagents.identity')} summaryClassName="text-xs text-muted-foreground">
                            <p className="mt-2 break-all text-xs">{task.task_id} · {task.group_id}</p>
                        </Disclosure>
                    </>}
                    {agents.detail && <Disclosure title={t('pages.aiAgent.subagents.objective')} defaultOpen summaryClassName="text-sm font-medium">
                        <p className="mt-2 whitespace-pre-wrap break-words text-sm">{agents.detail.result.objective}</p>
                        <ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-muted-foreground">
                            {agents.detail.result.acceptance_criteria.map((criterion, index) => <li key={index}>{criterion}</li>)}
                        </ul>
                    </Disclosure>}
                    {report && <section className="space-y-2">
                        <p className="text-sm font-medium">{t('pages.aiAgent.subagents.result')}</p>
                        <MarkdownContent disableLinks>{report.summary}</MarkdownContent>
                        {(['findings', 'delivered', 'remaining'] as const).map(section => report[section].length > 0 && <div key={section}>
                            <p className="text-xs font-medium">{t(`pages.aiAgent.subagents.${section}`)}</p>
                            <ul className="list-disc space-y-1 pl-5 text-sm">{report[section].map((item, index) => <li key={index} className="whitespace-pre-wrap break-words">{item}</li>)}</ul>
                        </div>)}
                        {report.reason && <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{report.reason}</p>}
                        {(report.receipt_refs.length > 0 || report.evidence_refs.length > 0) && <Disclosure title={t('pages.aiAgent.subagents.references')} summaryClassName="text-xs">
                            <p className="mt-2 break-all text-xs text-muted-foreground">{[...report.receipt_refs, ...report.evidence_refs].join(' · ')}</p>
                        </Disclosure>}
                    </section>}
                    {agents.reports && (agents.reports.items.length > 0 || agents.viewingOlderReports) &&
                        <Disclosure title={t('pages.aiAgent.subagents.reports')} defaultOpen summaryClassName="text-sm font-medium">
                            <ol className="mt-2 space-y-3">
                                {agents.reports.items.map(item => <li key={item.report_id} className="space-y-1 border-l pl-3">
                                    <p className="text-xs text-muted-foreground">{item.seq} · {t(`pages.aiAgent.subagents.reportKinds.${item.kind}`)}</p>
                                    <MarkdownContent disableLinks>{item.summary}</MarkdownContent>
                                    {item.question && <p className="whitespace-pre-wrap text-sm">{item.question}</p>}
                                </li>)}
                            </ol>
                            <div className="mt-2 flex flex-wrap gap-2">
                                {agents.reports.next_before_seq != null && <Button type="button" size="sm" variant="outline"
                                    disabled={agents.reportsLoading} onClick={() => void agents.loadOlderReports()}>
                                    {t('pages.aiAgent.subagents.earlierReports')}
                                </Button>}
                                {agents.viewingOlderReports && <Button type="button" size="sm" variant="ghost"
                                    onClick={agents.showLatestReports}>{t('pages.aiAgent.subagents.latestReports')}</Button>}
                            </div>
                        </Disclosure>}
                    {agents.detail?.result.failure_reason && <p className="whitespace-pre-wrap text-sm text-destructive">{agents.detail.result.failure_reason}</p>}
                        {!!child?.taskStatusProjection?.items.length && <Disclosure title={t('pages.aiAgent.subagents.progress')} summaryClassName="text-sm">
                            <ul className="mt-2 list-disc space-y-2 pl-5 text-sm">
                                {child.taskStatusProjection.items.map(item => <li key={item.itemId} className="break-words">
                                    {item.description} · {t(`pages.aiAgent.taskStatus.${item.status}`)}{item.note && <p className="text-xs text-muted-foreground">{item.note}</p>}
                                </li>)}
                            </ul>
                        </Disclosure>}
                        {!!child?.capabilityGrants?.length && <Disclosure title={t('pages.aiAgent.grantTitle')} summaryClassName="text-sm">
                            {child.capabilityGrants.map(grant => <div key={grant.grantId} className="mt-2 space-y-2 rounded-md border p-2">
                                <p className="break-all text-xs">{grant.grantId}</p>
                                <p className="break-words text-xs">{permissionToolLabel(t, grant.toolName)} · {grant.operationScope.map(value => permissionOperationLabel(t, value, grant.toolName)).join(', ')}</p>
                                {grant.revokedAtUnixMs == null && grant.expiresAtUnixMs > Date.now() && <Button type="button" size="sm" variant="outline"
                                    disabled={agents.updating || !canDecide || !connected} onClick={() => void agents.revokeGrant(grant.grantId)}>{t('pages.aiAgent.grantRevoke')}</Button>}
                            </div>)}
                        </Disclosure>}

        <Button type="button" variant="outline" disabled={agents.detailLoading} onClick={() => agents.openDetail(task)}>
            {t('common.refresh')}
        </Button>
    </div>;
    return <div className="flex min-h-0 flex-1 flex-col gap-2">
        {(pendingDirectories.length + pendingPermissions.length > 0 || runningTaskCount > 0) && <div className="flex shrink-0 flex-wrap gap-2 text-xs">
            {pendingDirectories.length + pendingPermissions.length > 0 && <Button type="button" size="sm" variant="outline" className="border-amber-500/50"
                onClick={() => jumpToPending()}>{t('pages.aiAgent.workspace.pendingCount', { count: pendingDirectories.length + pendingPermissions.length })}</Button>}
            {runningTaskCount > 0 && <Button type="button" size="sm" variant="ghost" onClick={() => setPanel('tasks')}>
                {t('pages.aiAgent.tasks.title')} ({runningTaskCount})</Button>}
        </div>}
        <div className="relative min-h-0 flex-1">
            <div ref={scrollRef} onScroll={onScroll} className="agent-scrollbar h-full overflow-y-auto overscroll-contain [overflow-wrap:anywhere]">
                <div ref={contentRef} className="mx-auto w-full max-w-[840px] space-y-4 pb-4">
                    {agents.error && <p role="alert" className="text-sm text-destructive">{agents.error}</p>}
                    {agents.detailLoading && <p role="status" className="text-sm text-muted-foreground">{t('pages.aiAgent.subagents.loading')}</p>}
                    {child && <>
                        {agents.hasMoreMessages && <Button type="button" size="sm" variant="ghost" disabled={agents.historyLoading}
                            onClick={() => void agents.loadOlderMessages()}>{t('pages.aiAgent.loadEarlierMessages')}</Button>}
                        <AgentTranscript sessionId={child.sessionId} messages={agents.childMessages} tools={agents.childTools}
                            running={child.active} evidence={child.visualEvidence ?? []} notices={child.contextNotices ?? []}
                            exportBackup={exportBackup && child.sessionId ? id => exportBackup(child.sessionId!, id) : undefined} />
                        {pendingDirectories.map(directory => <div key={`${child.sessionId}:${directory.requestId}`} data-agent-pending tabIndex={-1}>
                            <AgentDirectoryApproval directory={directory} revision={child.fileScope!.revision}
                                disabled={!canDecide || !connected || child.active || subagentIsTerminal(task)}
                                busy={agents.updating} onUpdate={agents.updateDirectory} />
                        </div>)}
                        <div data-agent-pending={pendingPermissions.length > 0 ? '' : undefined} tabIndex={-1}>
                        <AgentPermissionRecords approvalRecords={<ApprovalReviews session={child.sessionId} open={panel === 'permissions'} includeChildren={false}/>} key={child.sessionId} requests={child.permissionRequests ?? []} automaticApproval={automaticApproval}
                            open={panel === 'permissions'} onOpenChange={open => setPanel(open ? 'permissions' : null)}
                            decisions={{
                                canDecide: permission => canDecide && !subagentIsTerminal(task) && permission.inputRevision === child.inputRevision,
                                disabled: !connected || child.active || agents.updating,
                                onDecide: agents.decidePermission,
                            }}>
                            {permission => <div key={`${child.sessionId}:${permission.requestId}:${permission.inputRevision}`}
                                data-permission-id={permission.requestId} tabIndex={-1}><AgentPermissionRequest
                                request={permission} canDecide={canDecide && !subagentIsTerminal(task) && permission.inputRevision === child.inputRevision}
                                disabled={!connected || child.active || agents.updating} busy={agents.updating} waitingForTurn={child.active}
                                interactive={interactive ? { ...interactive, runId: child.sessionId } : undefined} onDecide={agents.decidePermission} /></div>}
                        </AgentPermissionRecords>
                        </div>
                        {child.terminalError && <p role="alert" className="text-sm text-destructive">{child.terminalError.message}</p>}
                    </>}
                </div>
            </div>
            {showJumpToLatest && <Button type="button" variant="outline" size="icon" onClick={jumpToLatest}
                className="agent-jump-action absolute bottom-3 right-3 rounded-full bg-background shadow-md"
                aria-label={t('pages.aiAgent.scrollToLatest')}><ArrowDown className="h-4 w-4" /></Button>}
        </div>
        <form className="agent-composer mx-auto w-full max-w-[840px] shrink-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm" onSubmit={event => {
            event.preventDefault();
            if (subagentIsTerminal(task) || sendDisabled) return;
            if (question) {
                void agents.control(task, { kind: 'reply', question_id: question.question_id, message: replyText.trim() }).then(ok => {
                    if (ok) setReplyDraft(current => current?.questionId === question.question_id ? undefined : current);
                });
            } else {
                void agents.control(task, { kind: 'adjust', message: adjustment.trim() }).then(ok => {
                    if (ok) setAdjustment('');
                });
            }
        }}>
            <AgentSubagentApprovalNotice agents={agents} />
            {question && <p className="whitespace-pre-wrap text-sm">{question.text}</p>}
            {!subagentIsTerminal(task) && <AgentComposerInput sendDisabled={sendDisabled}
                aria-label={t(question ? 'pages.aiAgent.subagents.replyMessage' : 'pages.aiAgent.subagents.adjustMessage')}
                placeholder={t(question ? 'pages.aiAgent.subagents.replyMessage' : 'pages.aiAgent.subagents.adjustMessage')}
                value={draft} disabled={!canDecide || !connected || agents.updating}
                onChange={event => question
                    ? setReplyDraft({ questionId: question.question_id, text: event.target.value })
                    : setAdjustment(event.target.value)} className="min-h-16 max-h-40 resize-y" />}
            <div className="flex flex-wrap items-center justify-between gap-2">
                <div className="flex min-w-0 flex-wrap items-center gap-1">
                    <AgentSubagentMenu onSelect={setPanel} onDeviceSettings={onDeviceSettings} onApprovalSettings={onApprovalSettings} />
                    {automaticApproval && <AgentComposerStatus automaticApproval onApprovalSettings={onApprovalSettings} />}
                    <AgentComposerTools meter={<AgentContextMeter usage={child?.contextUsage ?? null} draft={draft} />}
                        onDetails={() => setPanel('details')} onPermissionHistory={() => setPanel('permissions')}
                        onDirectories={() => setPanel('directories')} onTasks={() => setPanel('tasks')} runningTaskCount={runningTaskCount} />
                </div>
                {!subagentIsTerminal(task) && <div className="flex items-center gap-2">
                    <Button type="button" variant="outline" disabled={!canDecide || !connected || agents.updating}
                        onClick={() => void agents.control(task, { kind: 'cancel' })}>{t('pages.aiAgent.subagents.cancel')}</Button>
                    <Button type="submit" disabled={sendDisabled}>
                        {t(question ? 'pages.aiAgent.subagents.sendReply' : 'pages.aiAgent.subagents.applyAdjustment')}
                    </Button>
                </div>}
            </div>
        </form>
        <AgentDetailsSheet panel={panel === 'details' || panel === 'context' || panel === 'capabilities' ? panel : null}
            onPanelChange={next => setPanel(next === 'details' || next === 'context' || next === 'capabilities' ? next : null)} sections={{ details: detailsContent, ...(capabilityList ? { capabilities: capabilityList } : {}), context: child ? <div className="space-y-3">
                        {!!child.contextAttachments?.length && <Disclosure title={t('pages.aiAgent.attachmentTitle')} defaultOpen summaryClassName="text-sm">
                            {child.contextAttachments.map(attachment => <div key={`${child.sessionId}:${attachment.id}`} className="mt-2 space-y-1 rounded-md border p-2">
                                <p className="break-words text-xs">{attachment.displaySummary}</p>
                                <p className="text-xs text-muted-foreground">{attachment.providerId} · {attachment.kind}</p>
                                <Badge variant={attachment.state === 'active' ? 'secondary' : 'outline'}>
                                    {attachment.state === 'active' ? t('pages.aiAgent.attachmentActive')
                                        : t('pages.aiAgent.attachmentStale', { reason: attachment.staleReason ?? attachment.state })}
                                </Badge>
                            </div>)}
                        </Disclosure>}

            {!child.contextAttachments?.length && <p className="text-sm text-muted-foreground">{t('pages.aiAgent.contextEmpty')}</p>}
        </div> : null }} />
        {child && <>
            <AgentAttachments key={`attachments:${child.sessionId}`} sessionId={child.sessionId} open={panel === 'attachments'}
                onOpenChange={open => setPanel(open ? 'attachments' : null)} showTrigger={false} />
            <AgentFileScope key={`directories:${child.sessionId}`} scope={child.fileScope ?? { revision: 0, directories: [] }} automaticApproval={automaticApproval} decisionPause={child.decisionPause}
                deskId={deskId} sessionTargetId={sessionTargetId} open={panel === 'directories'}
                onOpenChange={open => setPanel(open ? 'directories' : null)}
                disabled={!canDecide || !connected || agents.updating} selectionDisabled={child.active || subagentIsTerminal(task)}
                onUpdate={agents.updateDirectory} showPendingActions={false} onPendingJump={jumpToPending} />
            <AgentBackgroundTasks key={`tasks:${child.sessionId}`} open={panel === 'tasks'}
                onOpenChange={open => setPanel(open ? 'tasks' : null)} commands={child.commandTasks ?? []}
                providers={child.backgroundTasks ?? []} tools={agents.childTools} connected={canDecide && connected}
                canCancelProvider={canDecide && connected} cancelling={agents.updating ? 'busy' : null}
                onCancel={async (kind, id) => {
                    if (!await (kind === 'command' ? agents.cancelCommandTask(id) : agents.cancelProviderTask(id))) throw new Error('Cancellation failed');
                }} />
        </>}
    </div>;
}

export function AgentConversationTabs({ agents, attentionTasks = [], mainNeedsApproval = false }: {
    agents: Pick<ReturnType<typeof useAiAgentSubagents>, 'tasks' | 'selected' | 'openDetail' | 'closeDetail' | 'hasMore' | 'loading' | 'loadMore'> & Partial<Pick<ReturnType<typeof useAiAgentSubagents>, 'detail'>>;
    attentionTasks?: { task_id: string }[];
    mainNeedsApproval?: boolean;
}) {
    const { t } = useTranslation();
    const unread = new Set(attentionTasks.map(task => task.task_id));
    const needsApproval = (task: typeof agents.tasks[number]) => !subagentIsTerminal(task) && (task.state === 'waiting_approval'
        || (agents.detail?.result.task.task_id === task.task_id && (agents.detail.session.permissionRequests?.some(request => ['pending', 'needs_revalidation'].includes(request.state))
            || agents.detail.session.fileScope?.directories.some(directory => directory.state === 'pending'))));
    const priority = (task: typeof agents.tasks[number]) => needsApproval(task) ? 0 : subagentIsTerminal(task) ? 2 : 1;
    const tasks = [...agents.tasks].sort((a, b) => priority(a) - priority(b)
        || a.created_at.localeCompare(b.created_at) || a.task_id.localeCompare(b.task_id));
    const viewport = useRef<HTMLDivElement>(null);
    const [scroll, setScroll] = useState({ previous: false, next: false });
    const revealSelected = useCallback(() => {
        const element = viewport.current;
        const selected = element?.querySelector<HTMLElement>('[aria-selected="true"]');
        if (!element || !selected) return;
        selected.scrollIntoView?.({ block: 'nearest', inline: selected.offsetWidth > element.clientWidth ? 'start' : 'nearest' });
    }, []);
    const measure = useCallback(() => {
        const element = viewport.current;
        if (!element) return;
        const previous = element.scrollLeft > 1;
        const next = element.scrollLeft + element.clientWidth < element.scrollWidth - 1;
        setScroll(current => current.previous === previous && current.next === next ? current : { previous, next });
    }, []);
    const hasTabs = tasks.length > 0 || agents.hasMore;
    const order = tasks.map(task => task.task_id).join('|');
    const layout = tasks.map(task => `${task.task_id}:${task.name}:${task.state}:${needsApproval(task)}`).join('|');
    const selectedId = agents.selected?.task_id;
    useEffect(() => {
        const element = viewport.current;
        if (!element) return;
        measure();
        const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => { revealSelected(); measure(); });
        observer?.observe(element);
        element.querySelectorAll('[role="tab"]').forEach(tab => observer?.observe(tab));
        window.addEventListener('resize', measure);
        const wheel = (event: WheelEvent) => {
            if (Math.abs(event.deltaY) <= Math.abs(event.deltaX) || event.shiftKey) return;
            const next = Math.max(0, Math.min(element.scrollWidth - element.clientWidth, element.scrollLeft + event.deltaY));
            if (next === element.scrollLeft) return;
            event.preventDefault(); element.scrollLeft = next; measure();
        };
        element.addEventListener('wheel', wheel, { passive: false });
        return () => { observer?.disconnect(); window.removeEventListener('resize', measure); element.removeEventListener('wheel', wheel); };
    }, [hasTabs, layout, measure, revealSelected]);
    useEffect(() => {
        revealSelected(); measure();
    }, [selectedId, order, measure, revealSelected]);
    const move = (direction: number) => {
        const element = viewport.current;
        if (!element) return;
        const reducedMotion = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
        element.scrollBy({ left: direction * Math.max(80, element.clientWidth * 0.8), behavior: reducedMotion ? 'auto' : 'smooth' });
    };
    if (!hasTabs) return null;
    return <div role="tablist" onKeyDown={event => {
        if (!(event.target instanceof HTMLElement) || event.target.getAttribute('role') !== 'tab'
            || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
        const tabs = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]'));
        const index = tabs.indexOf(document.activeElement as HTMLButtonElement);
        const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1
            : (Math.max(index, 0) + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
        event.preventDefault(); tabs[next]?.focus(); tabs[next]?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' }); tabs[next]?.click();
    }} aria-label={t('pages.aiAgent.subagents.conversationTabs')}
        className="agent-conversation-tabs flex w-full max-w-full min-w-0 shrink-0 items-center gap-1 border-t pt-2">
        <Button type="button" role="tab" id="agent-tab-main" aria-controls="agent-main-panel"
            aria-selected={!agents.selected} tabIndex={agents.selected ? -1 : 0} variant={!agents.selected ? 'secondary' : 'ghost'}
            className="max-w-28 shrink-0" onClick={agents.closeDetail}>
            <span className={`min-w-0 truncate${mainNeedsApproval ? ' agent-approval-tab-title' : ''}`}
                data-approval-pending={mainNeedsApproval || undefined}>{t('pages.aiAgent.subagents.mainConversation')}</span>
            {mainNeedsApproval && <span className="sr-only">{t('pages.aiAgent.subagents.states.waiting_approval')}</span>}
        </Button>
        {(scroll.previous || scroll.next) && <Button type="button" variant="ghost" size="icon" className="agent-tab-scroll-button h-8 w-8 shrink-0"
            aria-label={t('pages.aiAgent.subagents.scrollPrevious')} disabled={!scroll.previous} onClick={() => move(-1)}>
            <ChevronLeft className="h-4 w-4" aria-hidden="true" />
        </Button>}
        <div ref={viewport} data-testid="agent-tab-viewport" onScroll={measure}
            className="agent-scrollbar flex min-w-0 flex-1 items-center gap-1 overflow-x-auto pb-1">
        {tasks.map(task => <Button key={task.task_id} type="button" role="tab" id={`agent-tab-${task.task_id}`}
            aria-controls="agent-child-panel" aria-selected={agents.selected?.task_id === task.task_id}
            tabIndex={agents.selected?.task_id === task.task_id ? 0 : -1}
            variant={agents.selected?.task_id === task.task_id ? 'secondary' : 'ghost'} className="h-auto max-w-64 shrink-0 gap-2"
            onClick={() => { if (agents.selected?.task_id !== task.task_id) agents.openDetail(task); }}>
            <ClipboardList className="h-4 w-4 shrink-0" aria-hidden="true" />
            <span className={`min-w-0 truncate${needsApproval(task) ? ' agent-approval-tab-title' : ''}`}
                data-approval-pending={needsApproval(task) || undefined}>{task.name}</span>
            {needsApproval(task) && task.state !== 'waiting_approval' && <span className="sr-only">{t('pages.aiAgent.subagents.states.waiting_approval')}</span>}
            <Badge variant={task.state === 'waiting_approval' ? 'secondary' : 'outline'}>{t(`pages.aiAgent.subagents.states.${task.state}`)}</Badge>
            {unread.has(task.task_id) && <span aria-label={t('pages.aiAgent.subagents.unread')} className="h-2 w-2 shrink-0 rounded-full bg-primary" />}
        </Button>)}
        </div>
        {(scroll.previous || scroll.next) && <Button type="button" variant="ghost" size="icon" className="agent-tab-scroll-button h-8 w-8 shrink-0"
            aria-label={t('pages.aiAgent.subagents.scrollNext')} disabled={!scroll.next} onClick={() => move(1)}>
            <ChevronRight className="h-4 w-4" aria-hidden="true" />
        </Button>}
        {tasks.length > 0 && <DropdownMenu><DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="icon" className="h-9 w-9 shrink-0" aria-label={t('pages.aiAgent.subagents.switchConversation')}>
                <ListFilter className="h-4 w-4" aria-hidden="true" />
            </Button>
        </DropdownMenuTrigger><DropdownMenuContent align="end" className="max-h-80 max-w-[calc(100vw-2rem)] overflow-y-auto">
            <DropdownMenuItem onSelect={agents.closeDetail}>{t('pages.aiAgent.subagents.mainConversation')}</DropdownMenuItem>
            {tasks.map(task => <DropdownMenuItem key={task.task_id} onSelect={() => { if (selectedId !== task.task_id) agents.openDetail(task); }}>
                <span className={needsApproval(task) ? 'agent-approval-tab-title' : undefined}>{task.name}</span>
                <span className="ml-auto pl-2 text-xs text-muted-foreground">{t(`pages.aiAgent.subagents.states.${task.state}`)}</span>
            </DropdownMenuItem>)}
        </DropdownMenuContent></DropdownMenu>}
        {agents.hasMore && <Button type="button" variant="ghost" size="icon" className="h-9 w-9 shrink-0" disabled={agents.loading}
            aria-label={t('pages.aiAgent.subagents.more')} title={t('pages.aiAgent.subagents.more')}
            onClick={() => void agents.loadMore()}><Plus className="h-4 w-4" aria-hidden="true" /></Button>}
    </div>;
}
