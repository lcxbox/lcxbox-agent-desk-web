import { useCallback, useEffect, useRef, useState } from 'react';
import { useMutation, useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
    AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { cancelAiAgentMainRequest, getAiAgentSession, listAiAgentWorkbenchTasks, stopAiAgentSession } from '@/services/clients';
import type { AiAgentWorkbenchDelivery, AiAgentWorkbenchTask } from '@/services/types';
import { AgentConversationTabs, AgentSubagentApprovalNotice, AgentSubagents } from './agent-subagents';
import { AgentTranscript } from './agent-transcript';
import { queueFailureReason } from './agent-failed-requests';
import { projectPersistedSnapshot, type PersistedSnapshot } from './use-ai-agent-chat';
import { useAiAgentSubagents } from './use-ai-agent-subagents';

export function WorkbenchTasks({ workbench, request, worker, permission, active, subscriptionMode, onOpen, onClose }: {
    workbench: string;
    subscriptionMode?: 'manual' | 'ai_review';
    request?: string;
    worker?: string;
    permission?: string;
    active: boolean;
    onOpen(request: string): void;
    onClose(): void;
}) {
    const { t } = useTranslation();
    const [cursors, setCursors] = useState<string[]>([]);
    const before = cursors.at(-1);
    const tasks = useQuery({
        queryKey: ['ai-agent-workbench-tasks', workbench, request, before],
        queryFn: async ({ signal }) => {
            const result = await listAiAgentWorkbenchTasks(workbench, { request, before }, { signal });
            if (!result.success || !result.data) throw new Error('Workbench task unavailable');
            return result.data;
        },
        enabled: active,
        refetchInterval: active ? 5000 : false,
    });
    const { refetch } = tasks;
    const refresh = useCallback(() => { void refetch(); }, [refetch]);
    return <section className="space-y-3">
        {request && <Button variant="ghost" size="sm" onClick={onClose}>{t('pages.aiAgent.workbench.tasks')}</Button>}
        {tasks.isPending && <p role="status">{t('common.loading')}</p>}
        {tasks.isError && <p role="alert" className="text-destructive">{t('pages.aiAgent.workbench.taskUnavailable')}</p>}
        {tasks.data?.items.length === 0 && <p className="text-sm text-muted-foreground">{t('pages.aiAgent.workbench.noTasks')}</p>}
        {tasks.data?.items.map(task => <Card key={task.receipt.requestId}>
            <CardHeader className="flex flex-row flex-wrap items-center justify-between gap-3 space-y-0">
                <div className="space-y-1">
                    <CardTitle className="text-base">{task.issueNumber == null
                        ? t('pages.aiAgent.workbench.ownerTask')
                        : t('pages.aiAgent.workbench.issueTask', { number: task.issueNumber })}</CardTitle>
                    <p className="text-xs text-muted-foreground">{new Date(task.updatedAtMs).toLocaleString()}</p>
                </div>
                {!request && <Button variant="outline" size="sm" onClick={() => onOpen(task.receipt.requestId)}>
                    {t('pages.aiAgent.workbench.openTask')}
                </Button>}
            </CardHeader>
            <CardContent className="space-y-3">
                <div className="flex flex-wrap gap-2">
                    {task.workerStates.length === 0 && <Badge variant="outline">
                        {t(`pages.aiAgent.workbench.queue.${task.receipt.state}`)}
                    </Badge>}
                    {task.workerStates.map(state => <Badge key={state} variant="outline">
                        {t(state === 'waiting_source' ? 'pages.aiAgent.workbench.waitingSource' : `pages.aiAgent.subagents.states.${state}`)}
                    </Badge>)}
                </div>
                {task.receipt.state === 'failed' && <p className="text-sm text-muted-foreground">
                    {queueFailureReason(t, task.receipt)}
                </p>}
                {request && <>
                    {task.deliveries.length > 0 && <section className="space-y-2" aria-label={t('pages.aiAgent.workbench.deliveries')}>
                        <h3 className="text-sm font-medium">{t('pages.aiAgent.workbench.deliveries')}</h3>
                        {task.issueNumber != null && subscriptionMode && <p className="text-sm text-muted-foreground">
                            {t('pages.aiAgent.approvalMode.subscription', { mode: t(`pages.aiAgent.approvalMode.${subscriptionMode}`) })}
                        </p>}
                        {task.deliveries.map(delivery => <WorkbenchDelivery key={delivery.artifactId} delivery={delivery} />)}
                    </section>}
                    <WorkbenchExecution workbench={workbench} task={task} worker={worker} permission={permission} refresh={refresh} active={active} />
                </>}
            </CardContent>
        </Card>)}
        {!request && <div className="flex gap-2">
            {before && <Button variant="outline" size="sm" onClick={() => setCursors(current => current.slice(0, -1))}>
                {t('pages.aiAgent.workbench.newerTasks')}
            </Button>}
            {tasks.data?.nextBefore && <Button variant="outline" size="sm"
                onClick={() => setCursors(current => [...current, tasks.data!.nextBefore!])}>
                {t('pages.aiAgent.workbench.olderTasks')}
            </Button>}
        </div>}
    </section>;
}

function originalIssueUrl(delivery: AiAgentWorkbenchDelivery): URL | undefined {
    if (!Number.isSafeInteger(delivery.issueNumber) || delivery.issueNumber < 1) return;
    const part = /^[a-zA-Z0-9_.-]+$/;
    if (![delivery.owner, delivery.repository].every(value => part.test(value) && value !== '.' && value !== '..')) return;
    try {
        const base = new URL(delivery.instance);
        if (base.protocol !== 'https:' || base.username || base.password || base.search || base.hash) return;
        if (!base.pathname.endsWith('/')) base.pathname += '/';
        return new URL(`${delivery.owner}/${delivery.repository}/issues/${delivery.issueNumber}`, base);
    } catch { return; }
}

export function deliveredCommentUrl(delivery: AiAgentWorkbenchDelivery): string | undefined {
    if (delivery.state !== 'delivered' || !Number.isSafeInteger(delivery.commentId)
        || !delivery.commentId || delivery.commentId < 1) return;
    const issue = originalIssueUrl(delivery);
    if (!issue) return;
    issue.hash = `issuecomment-${delivery.commentId}`;
    return issue.href;
}

function WorkbenchDelivery({ delivery }: { delivery: AiAgentWorkbenchDelivery }) {
    const { t } = useTranslation();
    const comment = deliveredCommentUrl(delivery);
    const needsAttention = delivery.reason === 'connection_unavailable' || delivery.reason === 'artifact_invalid';
    const href = comment ?? (needsAttention ? originalIssueUrl(delivery)?.href : undefined);
    return <div className="flex flex-wrap items-center gap-2 rounded-md border p-2 text-sm">
        <span>{delivery.owner}/{delivery.repository} #{delivery.issueNumber}</span>
        <Badge variant="outline">{t(`pages.aiAgent.workbench.delivery.${delivery.state}`)}</Badge>
        {needsAttention && <span className="text-muted-foreground">
            {t(`pages.aiAgent.workbench.delivery.${delivery.reason}`)}
        </span>}
        {href && <Button variant="link" size="sm" asChild>
            <a href={href} target="_blank" rel="noreferrer">{t(comment ? 'pages.aiAgent.workbench.openComment'
                : 'pages.aiAgent.workbench.openIssue')}</a>
        </Button>}
    </div>;
}

const emptyDelegation: PersistedSnapshot['subagents'] = {
    task: null, tasks: null, parent_session_id: null, active_tasks: [], attention_tasks: [], attention_count: 0,
};

function WorkbenchExecution({ workbench, task, worker, permission, active, refresh }: {
    workbench: string;
    task: AiAgentWorkbenchTask;
    worker?: string;
    permission?: string;
    active: boolean;
    refresh(): void;
}) {
    const { t } = useTranslation();
    const session = task.executionSessionId ?? undefined;
    const [confirming, setConfirming] = useState(false);
    const stopId = useRef<{ input: number; control: number; id: string } | undefined>(undefined);
    const snapshot = useQuery({
        queryKey: ['ai-agent-workbench-execution', session],
        queryFn: async ({ signal }) => {
            const result = await getAiAgentSession({ session }, { signal });
            if (!result.success || !result.data || result.data.sessionId !== session) throw new Error('Execution unavailable');
            return result.data as PersistedSnapshot;
        },
        enabled: !!session && active,
        refetchInterval: active ? 5000 : false,
        retry: false,
    });
    const { refetch } = snapshot;
    const refreshExecution = useCallback(() => { if (session) void refetch(); refresh(); }, [session, refetch, refresh]);
    const agents = useAiAgentSubagents({ connection: '', conversation: null, session,
        snapshot: snapshot.data?.subagents ?? emptyDelegation, connected: active, onChanged: refreshExecution });
    const openFromLink = useRef<string | undefined>(undefined);
    useEffect(() => {
        if (!active || !agents.ready || !worker || worker === openFromLink.current) return;
        openFromLink.current = worker;
        void agents.openDetailById(worker);
    }, [active, agents.ready, worker, agents.openDetailById]);

    const stop = useMutation({
        mutationFn: async () => {
            if (task.receipt.state === 'queued') {
                const result = await cancelAiAgentMainRequest({ session: workbench, requestId: task.receipt.requestId });
                if (!result.success) throw new Error('Queue cancellation failed');
                return;
            }
            const native = snapshot.data;
            if (!session || !native?.inputRevision || native.controlRevision == null) throw new Error('Execution unavailable');
            if (stopId.current?.input !== native.inputRevision || stopId.current?.control !== native.controlRevision) {
                stopId.current = { input: native.inputRevision, control: native.controlRevision, id: v4() };
            }
            const result = await stopAiAgentSession({ session, control: {
                client_request_id: stopId.current.id,
                expected_input_revision: native.inputRevision,
                expected_control_revision: native.controlRevision,
                subagent_choice: 'include_sub_agents',
            } });
            if (!result.success) throw new Error('Work stop failed');
        },
        onSettled: refreshExecution,
        onSuccess: () => { setConfirming(false); stopId.current = undefined; },
    });
    const native = snapshot.data;
    const projection = native && projectPersistedSnapshot(native);
    const canStop = task.receipt.state === 'queued'
        || (!!native && !native.mainStopped && (native.active || agents.unfinished > 0
            || task.deliveries.some(item => ['ready', 'claimed'].includes(item.state))));
    return <section className="space-y-3">
        {canStop && <Button size="sm" variant="outline" onClick={() => { stop.reset(); setConfirming(true); }}>
            {t('pages.aiAgent.workbench.stopRound')}
        </Button>}
        <AlertDialog open={confirming} onOpenChange={open => { if (!stop.isPending) setConfirming(open); }}>
            <AlertDialogContent>
                <AlertDialogHeader>
                    <AlertDialogTitle>{t('pages.aiAgent.workbench.stopRound')}</AlertDialogTitle>
                    <AlertDialogDescription>{t('pages.aiAgent.workbench.stopDescription')}</AlertDialogDescription>
                </AlertDialogHeader>
                {stop.isError && <p role="alert" className="text-sm text-destructive">{t('pages.aiAgent.workbench.saveFailed')}</p>}
                <AlertDialogFooter>
                    <AlertDialogCancel disabled={stop.isPending}>{t('common.cancel')}</AlertDialogCancel>
                    <AlertDialogAction disabled={stop.isPending || !canStop} onClick={event => { event.preventDefault(); stop.mutate(); }}>
                        {t('pages.aiAgent.workbench.stopRound')}
                    </AlertDialogAction>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
        {session && snapshot.isError && <p className="text-sm text-muted-foreground">{t('pages.aiAgent.workbench.taskUnavailable')}</p>}
        <AgentSubagentApprovalNotice agents={agents} />
        <AgentConversationTabs agents={agents} attentionTasks={native?.subagents.attention_tasks} />
        {agents.selected ? <div className="flex min-h-[32rem] max-h-[75vh] flex-col overflow-hidden rounded-md border p-3">
            <AgentSubagents agents={agents} connected={active} canDecide mainStopped={native?.mainStopped ?? false}
                focusPermissionId={agents.selected.task_id === worker ? permission : undefined} />
        </div> : native && projection && <AgentTranscript sessionId={session} messages={projection.messages}
            tools={projection.tools} running={native.active} evidence={native.visualEvidence ?? []}
            notices={native.contextNotices ?? []} />}
    </section>;
}
