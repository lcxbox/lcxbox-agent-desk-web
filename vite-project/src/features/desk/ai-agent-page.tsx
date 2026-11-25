import { AgentOwnerQuestion } from './agent-owner-question';
import { useOptionalDeviceTarget } from './device-target-context';
import { readAgentDraft, writeAgentDraft } from './agent-draft';
import { agentModelMissing } from './agent-model-readiness';
import { devicePath } from './device-target-paths';
import { AgentGoalProposalControls } from './agent-goal-proposal-controls';
import { ApprovalReviews, useOwnApprovalAccess } from '@/features/approval/reviews';
import { AgentGoalPanel, goalPlanningAction } from './agent-goal-panel';
import { AgentConversationMessages } from './agent-conversation-messages';
import { AgentConversationHistory } from './agent-conversation-history';
import { AgentQueuedMessages } from './agent-queued-messages';
import { permissionToolLabel, permissionResourceLabel, permissionOperationLabel, isReusableWebHost, distinctOperationScopes } from './agent-permission-labels';
import { BindingFingerprintNote, splitBindingFingerprints } from './agent-permission-fingerprints';
import { AgentAttachments } from './agent-attachments';
import { requireRecoveryZip } from '@/lib/file-recovery-error';
import { AgentComposerInput } from './agent-composer-input';
import { AgentObservationResult } from './agent-observation-result';
import { useFollowLatest } from '@/hooks/use-follow-latest';
import './agent-responsive.css';
import { AgentSchedules } from './agent-schedules';
import { AgentDocumentPreviews } from './agent-document-preview';
import { AgentBackgroundTasks } from './agent-background-tasks';
import { AgentSubagentApprovalNotice, AgentSubagents, AgentConversationTabs, type AgentSubagentPanel } from './agent-subagents';
import { AgentStopConfirmation } from './agent-stop-confirmation';
import { AgentComposerActions } from './agent-composer-actions';
import { AgentRequestDevices, useConversationRequestDevices } from './agent-request-devices';
import { AgentIssueSources } from './agent-issue-sources';
import type { IssueSelection } from '@/services/types';
import { AgentAutomaticApproval } from './agent-automatic-approval';
import { useAgentApprovalMode } from './agent-approval-mode';
import { useAiAgentSubagents } from './use-ai-agent-subagents';
import { ScheduleProposalCards } from '@/features/schedules/proposal-card';
import { pendingSkillProposalCount, SkillProposalCards } from './agent-skill-proposal-card';
import { ownerPendingDirectories } from './agent-decision-receipt';
import { AgentContextMeter } from './agent-context-meter';
import { AgentDirectoryApproval } from './agent-directory-approval';
import { AgentFileScope, type AgentDirectoryOperation } from './agent-file-scope';
import { AgentConnectionIcon } from './agent-connection-icon';
import { exportDeviceFileRecovery } from '@/services/clients';
import { FileRecoverySettings } from '@/features/settings/file-recovery-settings';
import { AgentPermissionRequest } from './agent-permission-request';
import { AgentPermissionRecords, permissionIsAutomaticallyReviewed } from './agent-permission-records';
import { AgentHistory } from './agent-history';
import { AgentComposerMenu, type AgentMenuSection } from './agent-composer-menu';
import { AgentComposerStatus } from './agent-composer-status';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { capabilityDescriptionKey } from './agent-capability-copy';
import { type FormEvent, useEffect, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { BookOpen, Bot, Check, AlertTriangle, ArrowDown, ArrowLeft, CalendarClock, Eye, FolderKey, ListTodo, ListTree, LoaderCircle, Monitor, Paperclip, RefreshCw, Settings2, ShieldCheck, SlidersHorizontal, X } from 'lucide-react';

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { MarkdownContent } from '@/components/markdown-content';
import { useGetModelProvider } from '@/services/hooks/modelProviderController/useGetModelProvider';
import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import { AgentBrowserPairing } from './agent-browser-pairing';
import { canPairLocalDevice } from './local-device-pairing';
import { useQueryServerInfo } from '@/services/hooks/systemController/useQueryServerInfo';
import { useRestrictedSession } from './restricted-session';
import { useDeskSignaling } from './use-desk-signaling';
import { isAiAgentEnabled } from './ai-agent-switch';
import {
    type ObservationEntry,
    type OwnerSelectableWindow,
    type SelectableApplication,
    type ObservationRoot,
    ownerSelectableWindows,
    useAiAgentObservation,
} from './use-ai-agent-observation';
import { useAiAgentChat } from './use-ai-agent-chat';
import { fetchGoalBudgetPolicy, type GoalBudgetPolicy } from '../settings/goal-budget-policy-settings';
import { SessionTargetDialog } from './session-target-selection';
import { useAiAgentCapabilities } from './use-ai-agent-capabilities';
import { AgentCapabilityList } from './agent-capability-list';
import { AgentDetailsSheet, type AgentPanelId } from './agent-details-sheet';
import { requiresBrowserRemoteTakeover } from './ai-agent-browser-takeover';
import { useConfirmExec } from '../exec/use-confirm-exec';
import { ExecLifecycle } from '../exec/exec-lifecycle';
import {
    type AiAgentFeatureProfile,
    hasAiAgentBrowserEntry,
} from './ai-agent-features';
import {
    isExactExternalSendTool,
    parseExternalSendReceipt,
} from './ai-agent-external-send';
import { AgentFailedRequests } from './agent-failed-requests';

const CURRENT_SCREEN_CAPABILITY_ID = 'screen.capture.current';

function GoalLimitsSummary({
    policy,
}: {
    policy: GoalBudgetPolicy;
}) {
    const { t } = useTranslation();
    const items = [
        ['goalBudgetActiveHours', policy.limits.activeTimeMs, 3_600_000],
        ['goalBudgetDeadlineDays', policy.limits.deadlineMs, 86_400_000],
        ['goalBudgetTokens', policy.limits.modelTokens, 1],
        ['goalBudgetModelCalls', policy.limits.modelCalls, 1],
        ['goalBudgetToolCalls', policy.limits.toolCalls, 1],
        ['goalBudgetSlices', policy.limits.slices, 1],
        ['goalBudgetStalledSlices', policy.limits.stalledSlices, 1],
    ] as const;
    return <p className="mt-1 text-xs text-muted-foreground">{items.map(([label, value, scale]) =>
        `${t(`pages.aiAgent.${label}`)}: ${value === null ? t('pages.aiAgent.goalBudgetDisabled') : (value / scale).toLocaleString()}`,
    ).join(' · ')}</p>;
}

function ObservationCard({
    title,
    description,
    entry,
    onRefresh,
    disabled = false,
    windowCandidates = [],
    onAttachWindow,
    onDelayedRefresh,
    onCancelDelayed,
    remainingSeconds = 0,
    applications = [],
    onListApplications,
    onInspectApplication,
}: {
    title: string;
    description: string;
    entry: ObservationEntry;
    onRefresh: () => void;
    disabled?: boolean;
    windowCandidates?: OwnerSelectableWindow[];
    onAttachWindow?: (candidate: OwnerSelectableWindow) => void;
    onDelayedRefresh?: () => void;
    onCancelDelayed?: () => void;
    remainingSeconds?: number;
    applications?: SelectableApplication[];
    onListApplications?: () => void;
    onInspectApplication?: (root: ObservationRoot) => void;
}) {
    const { t } = useTranslation();
    const isPending = entry.phase === 'pending';
    const isScheduled = entry.phase === 'scheduled';
    const error = entry.outcome?.status === 'err' ? entry.outcome.data : null;

    return (
        <Card>
            <CardHeader>
                <div className="flex items-start justify-between gap-4">
                    <div>
                        <CardTitle className="flex items-center gap-2 text-base">
                            <Eye className="h-4 w-4" />
                            {title}
                        </CardTitle>
                        <CardDescription className="mt-1">{description}</CardDescription>
                    </div>
                    <Badge variant={entry.phase === 'ready' ? 'default' : 'outline'}>
                        {t(`pages.aiAgent.phase.${entry.phase}`)}
                    </Badge>
                </div>
            </CardHeader>
            <CardContent className="space-y-3">
                <Button variant="outline" size="sm" onClick={onRefresh} disabled={disabled || isPending || isScheduled}>
                    {isPending
                        ? <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                        : <RefreshCw className="mr-2 h-4 w-4" />}
                    {t('pages.aiAgent.refresh')}
                </Button>
                {onDelayedRefresh && !isScheduled && (
                    <Button className="ml-2" variant="outline" size="sm" onClick={onDelayedRefresh} disabled={disabled || isPending}>
                        {t('pages.aiAgent.delayedObserve')}
                    </Button>
                )}
                {isScheduled && (
                    <div role="status" className="space-y-2 text-sm">
                        <p>{t('pages.aiAgent.observationCountdown', { count: remainingSeconds })}</p>
                        <Button variant="outline" size="sm" onClick={onCancelDelayed}>
                            {t('pages.aiAgent.cancelObservation')}
                        </Button>
                    </div>
                )}
                {onListApplications && (
                    <div className="space-y-2 rounded-md border p-3">
                        <Button variant="outline" size="sm" onClick={onListApplications} disabled={disabled || isPending || isScheduled}>
                            {t('pages.aiAgent.listApplications')}
                        </Button>
                        <p className="text-xs text-muted-foreground">{t('pages.aiAgent.applicationSelectionHint')}</p>
                        <div className="flex flex-wrap gap-2">
                            {applications.map((app) => <Button key={app.objectRef.token} variant="outline" size="sm"
                                disabled={disabled || isPending || isScheduled || Date.parse(app.objectRef.expires_at) <= Date.now()}
                                onClick={() => onInspectApplication?.(app.objectRef)}>
                                {app.name || t('pages.aiAgent.applicationUnnamed')}
                            </Button>)}
                        </div>
                    </div>
                )}
                {error && (
                    <Alert variant="destructive">
                        <AlertTitle>{error.kind}</AlertTitle>
                        <AlertDescription>{error.message}</AlertDescription>
                        {error.kind === 'permission_denied' && error.message.includes('allowlist') && (
                            <AlertDescription>{t('pages.aiAgent.applicationRestrictionHint')}</AlertDescription>
                        )}
                    </Alert>
                )}
                {windowCandidates.length > 0 && onAttachWindow && (
                    <div className="space-y-2 rounded-md border p-3">
                        <div>
                            <p className="text-sm font-medium">
                                {t('pages.aiAgent.windowSelectorTitle')}
                            </p>
                            <p className="text-xs text-muted-foreground">
                                {t('pages.aiAgent.windowSelectorDescription')}
                            </p>
                        </div>
                        <div className="flex flex-wrap gap-2">
                            {windowCandidates.map((candidate) => {
                                const label = candidate.title
                                    ?? t('pages.aiAgent.windowSelectorUntitled');
                                return (
                                    <Button
                                        key={candidate.objectRef.token}
                                        type="button"
                                        size="sm"
                                        variant="secondary"
                                        disabled={disabled}
                                        onClick={() => onAttachWindow(candidate)}
                                    >
                                        {label}
                                    </Button>
                                );
                            })}
                        </div>
                    </div>
                )}
                {entry.outcome?.status === 'ok' && (
                    <AgentObservationResult data={entry.outcome.data} />
                )}
            </CardContent>
        </Card>
    );
}

export function AiAgentWorkspace({
    initialConversationId,
    deskId,
    stableDeviceId,
    personalScope,
    localPairingAvailable,
    featureProfile,
    agentEnabled,
    backTo,
}: {
    initialConversationId?: string | null;
    deskId: string;
    stableDeviceId: string;
    personalScope?: string;
    localPairingAvailable: boolean;
    featureProfile: AiAgentFeatureProfile;
    agentEnabled: boolean;
    backTo?: string;
}) {
    const { t } = useTranslation();
    const { i18n } = useTranslation();
    const hasDevice = Boolean(deskId);
    const { isConnected, subscribe, sendMessage } = useDeskSignaling({ usePrepared: hasDevice });
    const { entries, inspectSession, inspectUi, scheduleUi, cancelDelayedUi, remainingSeconds, applications, listApplications, applicationSelectionAvailable } = useAiAgentObservation({
        deskId,
        enabled: hasDevice && agentEnabled && isConnected,
        subscribe,
        sendMessage,
    });
    const navigate = useNavigate();
    const setupOrigin = useLocation();
    const chat = useAiAgentChat({
        initialConversationId,
        deskId,
        connected: isConnected,
        conversationStorageScope: personalScope ?? stableDeviceId,
        subscribe,
        sendMessage,
    });
    const requestDevices = useConversationRequestDevices(personalScope, chat.conversationId, chat.ensureConversation);
    const subagents = useAiAgentSubagents({ connection: deskId, conversation: chat.conversationId,
        session: chat.sessionId, snapshot: chat.subagents, connected: isConnected, onChanged: chat.refreshSnapshot });
    const [contextDeviceChoice, setContextDeviceChoice] = useState<string>();
    const contextDevice = requestDevices.selected.find(device => device.id === contextDeviceChoice)
        ?? (requestDevices.selected.length === 1 ? requestDevices.selected[0] : undefined);
    const contextConnection = deskId || (contextDevice && requestDevices.connectionFor(contextDevice.id));
    const updateDirectory = (operation: AgentDirectoryOperation, timeoutMessage: string, deviceRef?: string) =>
        chat.updateDirectory(operation, timeoutMessage, deviceRef, requestDevices.connectionFor);
    const capabilities = useAiAgentCapabilities({
        deskId: contextConnection ?? null, subscribe, sendMessage,
        enabled: !!contextConnection && agentEnabled && isConnected,
    });
    const target = useOptionalDeviceTarget();
    const recoveryConnection = target?.connection;
    const exportSessionBackup = async (conversation: string | undefined, id: string) => {
        // Recovery records use the durable server session key, not the client
        // conversation UUID used when submitting new agent turns.
        const connection = recoveryConnection;
        if (!conversation || !connection) throw new Error('Backup target unavailable');
        const data = await exportDeviceFileRecovery({ connection: deskId, device_id: connection.device_id,
            conversation_id: conversation, recovery_id: id }, { responseType: 'blob' });
        const zip = await requireRecoveryZip(data);
        const url = URL.createObjectURL(zip);
        const anchor = document.createElement('a');
        anchor.href = url; anchor.download = 'file-recovery.zip'; anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 60000);
    };
    const exportBackup = (id: string) => exportSessionBackup(chat.sessionId, id);
    const exec = useConfirmExec({
        deskId,
        deviceId: stableDeviceId,
        subscribe,
        sendMessage,
        acceptUnsolicitedPreviews: hasDevice,
    });
    const provider = useGetModelProvider();
    const providerConfig = provider.data?.data;
    const modelMissing = agentModelMissing(provider);
    const draftScope = personalScope ?? stableDeviceId;
    const [question, setQuestion] = useState(() => readAgentDraft(draftScope));
    useEffect(() => writeAgentDraft(draftScope, question), [draftScope, question]);
    const [schedulesOpen, setSchedulesOpen] = useState(false);
    const [taskPanelSession, setTaskPanelSession] = useState<string | null>(null);
    const [attachmentsOpen, setAttachmentsOpen] = useState(false);
    const [goalDetailsOpen, setGoalDetailsOpen] = useState(false);
    const goalReplyRef = useRef<HTMLTextAreaElement>(null);
    const goalComposerRequested = useRef(false);
    const focusGoalComposer = () => {
        if (goalDetailsOpen) {
            goalComposerRequested.current = true;
            setGoalDetailsOpen(false);
        } else goalReplyRef.current?.focus();
    };
    const continueCompletedGoal = () => {
        setStartGoal(true);
        setPreviousCompletedGoalId(chat.goal?.goalId ?? null);
        const draft = t('pages.aiAgent.goalStillIncompletePrompt', { goal: chat.goal?.goalText ?? '' });
        setQuestion(draft);
        focusGoalComposer();
    };
    const [approvalSettingsOpen, setApprovalSettingsOpen] = useState(false);
    const [pendingScheduleCount, setPendingScheduleCount] = useState(0);
    const [deviceSettingsOpen, setDeviceSettingsOpen] = useState(false);
    const [panel, setPanel] = useState<AgentPanelId | null>(null);
    const [childPanel, setChildPanel] = useState<{ taskId: string; panel: AgentSubagentPanel | null } | null>(null);
    const setSelectedChildPanel = (panel: AgentSubagentPanel | null) => {
        if (subagents.selected) setChildPanel({ taskId: subagents.selected.task_id, panel });
    };
    useEffect(() => {
        setChildPanel(null); setPanel(null); setAttachmentsOpen(false); setTaskPanelSession(null);
        setPermissionHistorySession(null); setDirectorySession(null);
        setApprovalSettingsOpen(false); setDeviceSettingsOpen(false); setGoalDetailsOpen(false); setSchedulesOpen(false);
    }, [subagents.selected?.task_id]);
    const approvalAccess = useOwnApprovalAccess();
    const [permissionHistorySession, setPermissionHistorySession] = useState<string | null>(null);
    const [directorySession, setDirectorySession] = useState<string | null>(null);
    const permissionHistoryKey = `${deskId}:${chat.conversationId}`;
    const [childDrafts, setChildDrafts] = useState<Record<string, { revision: number; text: string }>>({});
    useEffect(() => { setChildDrafts({}); }, [permissionHistoryKey]);
    const selectedChildTask = subagents.detail?.result.task ?? subagents.selected;
    const selectedChildDraft = selectedChildTask && childDrafts[selectedChildTask.task_id];
    const { scrollRef, contentRef, onScroll, showJumpToLatest, jumpToLatest } = useFollowLatest(!subagents.selected, permissionHistoryKey);
    const approvalSettingsAvailable = target?.orgId == null && !!(hasDevice || personalScope) && featureProfile.approval_delegation;
    const approvalMode = useAgentApprovalMode(chat.sessionId, approvalSettingsAvailable);
    const noReview = approvalMode.data?.mode === 'no_review';
    const automaticApproval = approvalMode.data?.mode !== 'manual' && chat.approvalDelegation?.status === 'active';
    const pause = chat.decisionPause;
    const pendingDirectories = ownerPendingDirectories(chat.fileScope.directories, pause, automaticApproval);
    const pendingPermissionCount = chat.permissionRequests.filter(request => ['pending', 'needs_revalidation'].includes(request.state)
        && !permissionIsAutomaticallyReviewed(request, automaticApproval)).length;
    const pendingCount = pendingDirectories.length + pendingPermissionCount + pendingScheduleCount + Number(Boolean(chat.pendingGoalOpenRequest))
        + pendingSkillProposalCount(chat.skillProposals);
    const runningTaskCount = [...chat.commandTasks, ...chat.backgroundTasks]
        .filter(task => ['running', 'cancel_requested'].includes(task.state)).length;
    const jumpToPending = (id?: string) => {
        const target = id ? document.getElementById(id) : document.querySelector<HTMLElement>('[data-agent-pending]');
        target?.scrollIntoView({ block: 'center', behavior: 'smooth' });
        target?.focus({ preventScroll: true });
    };
    useEffect(() => { setPermissionHistorySession(null); }, [permissionHistoryKey]);
    useEffect(() => { setPendingScheduleCount(0); }, [permissionHistoryKey]);
    const [selectedCapabilityIds, setSelectedCapabilityIds] = useState<string[]>([]);
    const [selectedIssues, setSelectedIssues] = useState<IssueSelection[]>([]);
    const selectedContextCount = (hasDevice
        ? new Set([...selectedCapabilityIds,
            ...chat.attachments.filter(item => item.state === 'active').map(item => item.capabilityId)]).size
        : chat.attachments.filter(item => item.state === 'active'
            && requestDevices.selected.some(device => device.id === item.deviceRef)).length) + selectedIssues.length;
    const [startGoal, setStartGoal] = useState(false);
    const [goalTargetRef, setGoalTargetRef] = useState<string | null>(null);
    const goalDevice = requestDevices.selected.length === 1 ? requestDevices.selected[0]
        : requestDevices.selected.find(device => device.id === goalTargetRef);
    const [goalBudgetPolicy, setGoalBudgetPolicy] = useState<GoalBudgetPolicy | null>(null);
    useEffect(() => {
        if (!startGoal && !chat.pendingGoalOpenRequest && !chat.goal) return;
        let current = true;
        setGoalBudgetPolicy(null);
        const reload = () => { void fetchGoalBudgetPolicy()
            .then(value => { if (current) setGoalBudgetPolicy(value); })
            .catch(() => { if (current) setGoalBudgetPolicy(null); }); };
        reload();
        const timer = window.setInterval(reload, 15_000);
        return () => { current = false; window.clearInterval(timer); };
    }, [startGoal, chat.pendingGoalOpenRequest?.requestId, chat.goal?.goalId]);
    const [previousCompletedGoalId, setPreviousCompletedGoalId] = useState<string | null>(null);
    const [offPageReminderAvailable, setOffPageReminderAvailable] = useState(() => {
        try { return sessionStorage.getItem('lcxbox.tauriShell') === '1'; }
        catch { return false; }
    });
    useEffect(() => {
        const abort = new AbortController();
        void fetch('/api/my/ai-agent-attention?limit=1', {
            credentials: 'include', headers: { Accept: 'application/json' }, signal: abort.signal,
        }).then(response => response.ok ? response.json() : null)
            .then(body => {
                if (!abort.signal.aborted && body?.data?.offPageReminderAvailable === true) {
                    setOffPageReminderAvailable(true);
                }
            }).catch(() => {});
        return () => abort.abort();
    }, []);

    const contextCapabilities = contextConnection && featureProfile.object_context
        ? (capabilities.snapshot?.entries ?? []).filter((entry) =>
            hasDevice ? entry.context_selectable : entry.selectable_device_context)
        : [];

    const browserTakeoverRequired = hasDevice && requiresBrowserRemoteTakeover(
        capabilities.snapshot?.entries,
    );
    const externalSendReceipts = chat.tools.flatMap((tool) => {
        if (!isExactExternalSendTool(tool.name)) return [];
        const receipt = parseExternalSendReceipt(tool.output);
        return receipt ? [{ tool, receipt }] : [];
    });

    useEffect(() => {
        if (!hasDevice) return;
        const ready = new Set(
            contextCapabilities
                .filter((entry) => entry.ready)
                .map((entry) => entry.capability.capability_id),
        );
        setSelectedCapabilityIds((current) => current.filter((id) => ready.has(id)));
    }, [capabilities.snapshot, hasDevice]);

    useEffect(() => {
        const restored = featureProfile.object_context
            ? chat.attachments
            .filter((attachment) =>
                attachment.state === 'active' && attachment.kind === 'interactive_session'
                    && (hasDevice || attachment.deviceRef === contextDevice?.id),
            )
            .map((attachment) => attachment.capabilityId)
            : [];
        setSelectedCapabilityIds([...new Set(restored)]);
    }, [chat.attachments, contextDevice?.id, featureProfile.object_context, hasDevice]);

    const toggleContext = (capabilityId: string) => {
        if (!contextConnection || !agentEnabled || !featureProfile.object_context) return;
        const next = selectedCapabilityIds.includes(capabilityId)
            ? selectedCapabilityIds.filter((id) => id !== capabilityId)
            : [...selectedCapabilityIds, capabilityId];
        // CurrentScreen is deliberately one-shot: selecting it never writes a
        // durable context update, and the UI clears it immediately after the
        // turn is accepted so every screenshot requires a fresh user gesture.
        if (capabilityId === CURRENT_SCREEN_CAPABILITY_ID) {
            setSelectedCapabilityIds(next);
            return;
        }
        if (chat.updateContext(next.filter(id => id !== CURRENT_SCREEN_CAPABILITY_ID), contextConnection)) {
            setSelectedCapabilityIds(next);
        }
    };

    useEffect(() => {
        const accepted = chat.acceptedInput;
        if (!accepted) return;
        setQuestion(current => current.trim() === accepted.question ? '' : current);
        setStartGoal(false);
        setGoalTargetRef(null);
        setPreviousCompletedGoalId(null);
        setSelectedCapabilityIds(current => current.filter(id => id !== CURRENT_SCREEN_CAPABILITY_ID));
    }, [chat.acceptedInput]);

    const canQueueInput = !startGoal;
    const sendDisabled = (!canQueueInput && chat.turnRunning) || chat.stopping || !!chat.deliveryState
        || !agentEnabled || !question.trim() || !isConnected || chat.hydrating || chat.historyRestoreFailed || !chat.sessionTargetReady
        || chat.sessionTargetResolving || chat.contextUpdating || modelMissing
        || !requestDevices.ready || (startGoal && (!goalBudgetPolicy || (!hasDevice && !goalDevice)));
    const submit = (event: FormEvent) => {
        event.preventDefault();
        if (sendDisabled) return;
        const selectedContext = featureProfile.object_context ? selectedCapabilityIds : [];
        chat.start(question, i18n.language, selectedContext, startGoal, previousCompletedGoalId, selectedIssues,
            startGoal && goalDevice ? [goalDevice.id] : requestDevices.selected.map(device => device.id));
    };

    const resetConversation = (temporary = false) => {
        chat.reset(temporary);
        setSelectedCapabilityIds([]);
        setSelectedIssues([]);
        setStartGoal(false);
        setPreviousCompletedGoalId(null);
    };

    const displayNameForTool = (name: string) => {
        if (name === 'call_external_tool') return t('pages.aiAgent.issueSources.readTool');
        const key = capabilities.snapshot?.entries.find(entry => entry.capability.tool_name === name)?.capability.display_name_key;
        return key ? t(key, { defaultValue: name }) : name;
    };

    const detailsContent = (
        <div className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAgent.providerBoundary', {
                provider: providerConfig?.wire_protocol ?? t('pages.aiAgent.providerUnknown'),
                model: providerConfig?.model ?? t('pages.aiAgent.providerUnknown'),
            })}</p>
                                {chat.taskStatusProjection && (
                        <div data-testid="ai-agent-task-status" className="space-y-2 rounded-md border p-3">
                            <div className="flex flex-wrap items-center justify-between gap-2">
                                <div>
                                    <p className="text-sm font-medium">{t('pages.aiAgent.taskStatusTitle')}</p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.aiAgent.taskStatusDescription')}
                                    </p>
                                </div>
                                <div className="flex gap-2">
                                    {chat.pendingInputCount > 0 && (
                                        <Badge variant="secondary">
                                            {t('pages.aiAgent.pendingInputs', { count: chat.pendingInputCount })}
                                        </Badge>
                                    )}
                                    <Badge variant="outline">rev {chat.taskStatusProjection.revision}</Badge>
                                </div>
                            </div>
                            <div className="space-y-2">
                                {chat.taskStatusProjection.items.map((item) => (
                                    <div key={item.itemId} className="flex items-start justify-between gap-3 rounded bg-muted/50 px-3 py-2">
                                        <div>
                                            <p className="text-sm">{item.description}</p>
                                            {item.note && <p className="text-xs text-muted-foreground">{item.note}</p>}
                                        </div>
                                        <Badge variant="outline">
                                            {t(`pages.aiAgent.taskStatus.${item.status}`)}
                                        </Badge>
                                    </div>
                                ))}
                            </div>
                        </div>
                    )}

                                {chat.capabilityGrants.length > 0 && (
                        <div data-testid="ai-agent-capability-grants" className="space-y-3 rounded-md border border-emerald-500/40 p-3">
                            <div>
                                <p className="flex items-center gap-2 text-sm font-medium">
                                    <ShieldCheck className="h-4 w-4" />
                                    {t('pages.aiAgent.grantTitle')}
                                </p>
                                <p className="text-xs text-muted-foreground">
                                    {t('pages.aiAgent.grantDescription')}
                                </p>
                            </div>
                            {chat.capabilityGrants.map((grant) => {
                                const expired = grant.expiresAtUnixMs <= Date.now();
                                const state = grant.revokedAtUnixMs != null
                                    ? 'revoked'
                                    : expired
                                        ? 'expired'
                                        : grant.remainingUses === 0
                                            ? 'exhausted'
                                            : 'active';
                                const grantScopes = splitBindingFingerprints(grant.resourceScope);
                                return (
                                    <div key={grant.grantId} className="space-y-2 rounded-md bg-muted/50 p-3">
                                        <div className="flex flex-wrap items-start justify-between gap-2">
                                            <div>
                                                <p className="break-all text-sm font-medium">
                                                    {permissionToolLabel(t, grant.toolName)}
                                                </p>
                                                <p className="break-all text-xs text-muted-foreground">
                                                    {grant.providerId} · {grant.capabilityId} · {grant.riskTier}
                                                </p>
                                            </div>
                                            <Badge variant={state === 'active' ? 'default' : 'outline'}>
                                                {t(`pages.aiAgent.grantState.${state}`)}
                                            </Badge>
                                        </div>
                                        <div className="space-y-1 text-xs text-muted-foreground">
                                            <p>{isReusableWebHost(grant)
                                                ? t('pages.aiAgent.webHostReusableUntilExpiry')
                                                : t('pages.aiAgent.grantRemainingUses', { count: grant.remainingUses })}</p>
                                            <p>{t('pages.aiAgent.grantExpiresAt', {
                                                time: new Date(grant.expiresAtUnixMs).toLocaleString(),
                                            })}</p>
                                            {[...grantScopes.shown, ...distinctOperationScopes(t, grant)].length > 0 && (
                                                <p className="break-all">
                                                    {[...grantScopes.shown.map((scope) => permissionResourceLabel(t, scope)), ...distinctOperationScopes(t, grant).map((scope) => permissionOperationLabel(t, scope, grant.toolName))].join(' · ')}
                                                </p>
                                            )}
                                            <BindingFingerprintNote fingerprints={grantScopes.fingerprints} grant />
                                            {grant.revokedReason && <p className="break-all">{grant.revokedReason}</p>}
                                        </div>
                                        {featureProfile.grant_revoke && state === 'active' && (
                                            <Button
                                                type="button"
                                                size="sm"
                                                variant="outline"
                                                disabled={chat.grantRevoking !== null}
                                                onClick={() => void chat.revokeCapabilityGrant(grant.grantId)}
                                            >
                                                {chat.grantRevoking === grant.grantId
                                                    ? <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />
                                                    : <X className="mr-2 h-4 w-4" />}
                                                {t('pages.aiAgent.grantRevoke')}
                                            </Button>
                                        )}
                                    </div>
                                );
                            })}
                        </div>
                    )}

                                {chat.draft && (
                        <Card data-testid="computer-action-draft-preview" className="border-violet-500/40">
                            <CardHeader>
                                <CardTitle className="text-base">{t('pages.aiAgent.draftTitle')}</CardTitle>
                                <CardDescription>
                                    {t('pages.aiAgent.draftDescription', {
                                        count: chat.draft.actions.length,
                                        risk: chat.draft.risk,
                                    })}
                                </CardDescription>
                            </CardHeader>
                            <CardContent className="space-y-3">
                                <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted p-3 text-xs">
                                    {JSON.stringify(chat.draft, null, 2)}
                                </pre>
                                <Button disabled>{t('pages.aiAgent.executionDisabled')}</Button>
                            </CardContent>
                        </Card>
                    )}

        </div>
    );

    const menuSections: AgentMenuSection[] = [
        { label: t('pages.aiAgent.workspace.resources'), actions: [
            { label: t('pages.aiAgent.attachments.title'), icon: Paperclip,
                disabled: !chat.sessionId, onSelect: () => setAttachmentsOpen(true) },
            ...(hasDevice || personalScope ? [{ label: t('pages.aiAgent.directories.title'), icon: FolderKey,
                onSelect: () => setDirectorySession(permissionHistoryKey) }] : []),
            ...((hasDevice || personalScope) ? [
                { label: t('pages.aiAgent.schedules.title'), icon: CalendarClock,
                    onSelect: () => setSchedulesOpen(true) },
            ] : []),
            ...((hasDevice || personalScope) ? [{ label: t('schedules.createResume'), icon: CalendarClock,
                disabled: !agentEnabled || chat.running || chat.hydrating || !chat.sessionId || !chat.inputRevision || (!hasDevice && !requestDevices.selected.length),
                onSelect: () => { if (chat.sessionId && chat.inputRevision) navigate(`/schedules?${new URLSearchParams({
                    resume_conversation: chat.sessionId,
                    ...(hasDevice ? {resume_device: stableDeviceId} : {resume_devices: requestDevices.selected.map(device => device.id).join(',')}),
                    resume_revision: String(chat.inputRevision),
                })}`); } }] : []),
        ] },
        { label: t('pages.aiAgent.workspace.manage'), actions: [
            { label: `${t('pages.aiAgent.tasks.title')}${runningTaskCount > 0 ? ` (${runningTaskCount})` : ''}`,
                icon: ListTodo, onSelect: () => setTaskPanelSession(permissionHistoryKey) },
            { label: t('pages.aiAgent.permissionHistory'), icon: ShieldCheck,
                onSelect: () => setPermissionHistorySession(permissionHistoryKey) },
            ...(approvalSettingsAvailable ? [{ label: t('pages.aiAgent.approvalMode.title'), icon: Bot,
                onSelect: () => setApprovalSettingsOpen(true) }] : []),
            { label: t('pages.aiAgent.personalization.title'), icon: SlidersHorizontal,
                onSelect: () => navigate('/ai-agent/personalization') },
            { label: t('pages.agentSkills.personal.title'), icon: BookOpen,
                onSelect: () => navigate('/ai-agent/skills') },
            ...(hasDevice ? [{ label: t('pages.aiAgent.workspace.deviceSettings'), icon: Settings2,
                onSelect: () => setDeviceSettingsOpen(true) }] : []),
        ] },
        { label: t('pages.aiAgent.workspace.troubleshoot'), actions: [
            { label: t('pages.aiAgent.workspace.details'), icon: ListTree, onSelect: () => setPanel('details') },
        ] },
    ];

    const issueSources = <AgentIssueSources value={selectedIssues} onChange={setSelectedIssues}
        disabled={!agentEnabled || chat.hydrating || !!chat.deliveryState || chat.historyRestoreFailed} />;

    const contextSection = <>
        {featureProfile.object_context && (hasDevice || personalScope) && (
            <Card data-testid="ai-agent-context-selector">
                <CardHeader>
                    <CardTitle className="text-base">{t('pages.aiAgent.contextTitle')}</CardTitle>
                    <CardDescription>{t('pages.aiAgent.contextDescription')}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-2">
                    {!hasDevice && requestDevices.selected.length > 1 && <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                            <Button type="button" variant="outline" disabled={chat.contextUpdating}>
                                {contextDevice?.name ?? t('pages.aiAgent.requestDevices')}
                            </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent>
                            {requestDevices.selected.map(device => <DropdownMenuItem key={device.id}
                                onSelect={() => setContextDeviceChoice(device.id)}>{device.name}</DropdownMenuItem>)}
                        </DropdownMenuContent>
                    </DropdownMenu>}
                    {contextCapabilities.length === 0 && (
                        <p className="text-sm text-muted-foreground">
                            {t('pages.aiAgent.contextEmpty')}
                        </p>
                    )}
                    {contextCapabilities.map((entry) => {
                        const id = entry.capability.capability_id;
                        const selected = selectedCapabilityIds.includes(id);
                        return (
                            <Button variant="unstyled"
                                key={id}
                                type="button"
                                disabled={!agentEnabled || (!entry.ready && (hasDevice || !selected))
                                    || chat.running || chat.contextUpdating}
                                onClick={() => toggleContext(id)}
                                className="flex w-full items-center justify-between gap-3 rounded-md border px-3 py-2 text-left disabled:cursor-not-allowed disabled:opacity-50"
                            >
                                <span className="min-w-0 flex-1">
                                    <span className="block text-sm font-medium">{t(entry.capability.display_name_key, { defaultValue: id })}</span>
                                    <code className="block break-all text-xs text-muted-foreground">{id}</code>
                                    <span className="block text-xs text-muted-foreground">
                                        {t(capabilityDescriptionKey(entry.capability.display_name_key), {
                                            defaultValue: t('pages.aiAgent.workspace.descriptionUnavailable'),
                                        })}
                                    </span>
                                    <span className="block text-xs text-muted-foreground">
                                        {entry.ready
                                            ? t('pages.aiAgent.contextWillSend')
                                            : entry.reason ?? t('pages.aiAgent.contextUnavailable')}
                                    </span>
                                </span>
                                <Badge variant={selected ? 'default' : 'outline'} className="shrink-0 whitespace-nowrap">
                                    {selected && <Check className="mr-1 h-3 w-3" />}
                                    {selected
                                        ? t('pages.aiAgent.contextSelected')
                                        : t('pages.aiAgent.contextNotSelected')}
                                </Badge>
                            </Button>
                        );
                    })}
                    {chat.attachments.length > 0 && (
                        <div className="space-y-2 border-t pt-3" data-testid="ai-agent-attachments">
                            <p className="text-xs font-medium text-muted-foreground">
                                {t('pages.aiAgent.attachmentTitle')}
                            </p>
                            {chat.attachments.map((attachment) => (
                                <div
                                    key={attachment.id}
                                    className="flex items-center justify-between gap-3 rounded-md bg-muted px-3 py-2"
                                >
                                    <span className="min-w-0">
                                        <span className="block truncate text-xs font-medium">
                                            {attachment.displaySummary}
                                        </span>
                                        <span className="block text-xs text-muted-foreground">
                                            {attachment.providerId} · {attachment.kind}
                                            {!hasDevice && attachment.deviceRef
                                                ? ` · ${requestDevices.selected.find(device => device.id === attachment.deviceRef)?.name ?? attachment.deviceRef}`
                                                : ''}
                                        </span>
                                    </span>
                                    <span className="flex shrink-0 items-center gap-1">
                                        <Badge variant={attachment.state === 'active' ? 'secondary' : 'outline'}>
                                            {attachment.state === 'active'
                                                ? t('pages.aiAgent.attachmentActive')
                                                : t('pages.aiAgent.attachmentStale', {
                                                    reason: attachment.staleReason ?? 'unknown',
                                                })}
                                        </Badge>
                                        {attachment.state === 'active' &&
                                            attachment.kind !== 'interactive_session' && (
                                            <Button
                                                type="button"
                                                variant="ghost"
                                                size="icon"
                                                className="h-7 w-7"
                                                disabled={chat.running}
                                                title={t('pages.aiAgent.attachmentDetach')}
                                                onClick={() => chat.detachAttachment(attachment.id)}
                                            >
                                                <X className="h-3.5 w-3.5" />
                                            </Button>
                                        )}
                                    </span>
                                </div>
                            ))}
                        </div>
                    )}
                </CardContent>
            </Card>
        )}
        {issueSources}
    </>;

    return (
        <>
            <AgentDetailsSheet panel={panel} onPanelChange={setPanel} sections={{
                details: detailsContent,
                context: contextSection,
                ...(hasDevice ? {
                capabilities: <AgentCapabilityList entries={capabilities.snapshot?.entries ?? []}
                    loading={capabilities.loading} error={Boolean(capabilities.error)}
                    refreshDisabled={!agentEnabled || !isConnected} onRefresh={capabilities.refresh} />,
                connection: <>
            {(recoveryConnection?.connection_id === deskId) && <FileRecoverySettings
                key={deskId} target={{ connection: deskId, device_id: recoveryConnection?.device_id }} />}
            {localPairingAvailable && <AgentBrowserPairing agentEnabled={agentEnabled} />}
</>,
                observation: <>            <div className="grid gap-4 lg:grid-cols-2">
                <ObservationCard
                    title={t('pages.aiAgent.sessionTitle')}
                    description={t('pages.aiAgent.sessionDescription')}
                    entry={entries.desktop_session_inspect}
                    onRefresh={() => inspectSession()}
                    disabled={!agentEnabled || !isConnected}
                />
                <ObservationCard
                    title={t('pages.aiAgent.uiTitle')}
                    description={t('pages.aiAgent.uiDescription')}
                    entry={entries.desktop_ui_inspect}
                    onRefresh={() => inspectUi()}
                    applications={applications}
                    onListApplications={applicationSelectionAvailable ? listApplications : undefined}
                    onInspectApplication={inspectUi}
                    onDelayedRefresh={scheduleUi}
                    onCancelDelayed={cancelDelayedUi}
                    remainingSeconds={remainingSeconds}
                    disabled={!agentEnabled || !isConnected}
                    windowCandidates={ownerSelectableWindows(entries.desktop_ui_inspect)}
                    onAttachWindow={(candidate) => chat.attachWindow(
                        candidate.objectRef,
                        candidate.title ?? t('pages.aiAgent.windowSelectorUntitled'),
                    )}
                />
            </div>
</>,
                } : {}),
            }} />
            {hasDevice && <SessionTargetDialog
                targets={chat.sessionTargets}
                onSelect={(targetId) => chat.selectSessionTarget(targetId)}
            />}
            {!agentEnabled && (
                <Alert data-testid="ai-agent-disabled">
                    <AlertTitle>{t('pages.aiAgent.disabledTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.aiAgent.disabledDescription')}
                        <Button type="button" variant="outline" size="sm" className="mt-2" onClick={() => setDeviceSettingsOpen(true)}>
                            {t('pages.aiAgent.workspace.deviceSettings')}
                        </Button>
                    </AlertDescription>
                </Alert>
            )}
            {hasDevice && [
                featureProfile.permission_decision,
                featureProfile.approval_delegation,
                featureProfile.grant_revoke,
                featureProfile.background_task_cancel,
                featureProfile.object_context,
            ].some((enabled) => !enabled) && (
                <Alert data-testid="ai-agent-partial-support">
                    <AlertTitle>{t('pages.aiAgent.partialSupportTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.aiAgent.partialSupportDescription')}
                    </AlertDescription>
                </Alert>
            )}
            {browserTakeoverRequired && (
                <Link to={devicePath(stableDeviceId, 'browser-setup', target?.orgId)}
                    state={{ returnTo: setupOrigin.pathname + setupOrigin.search }} data-testid="browser-remote-takeover"
                    className="flex w-full shrink-0 items-center gap-2 border bg-muted/40 px-3 py-2 text-sm hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
                    <Monitor className="h-4 w-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                    <span className="min-w-0 flex-1">{t('pages.aiAgent.browserTakeoverTitle')}</span>
                    <span className="shrink-0 text-xs text-primary">{t('pages.aiAgent.browserSetup.open')}</span>
                </Link>
            )}
            <Card className="flex min-h-0 w-full flex-1 flex-col rounded-none border-0 shadow-none">
                <CardHeader className="agent-header shrink-0 px-3 py-2">
                    <div data-testid="agent-title-row" className="flex items-center justify-between gap-2">
                        <div className="flex min-w-0 flex-1 items-center gap-2">
                            {backTo && (
                                <Button asChild variant="ghost" size="icon" className="agent-header-back h-8 w-8 shrink-0">
                                    <Link to={backTo} aria-label={t(hasDevice ? 'pages.aiAgent.backToDevice' : 'pages.aiAgent.backToOverview')}
                                        title={t(hasDevice ? 'pages.aiAgent.backToDevice' : 'pages.aiAgent.backToOverview')}>
                                        <ArrowLeft className="h-4 w-4" aria-hidden="true" />
                                    </Link>
                                </Button>
                            )}
                            <CardTitle className="flex min-w-0 items-center gap-2 text-base">
                                <AgentConnectionIcon connected={isConnected} enabled={agentEnabled} />
                                <span title={recoveryConnection?.peer.display_name ?? deskId}
                                    className="truncate">{recoveryConnection?.peer.display_name
                                        || chat.sessionTarget?.display_name || t('pages.aiAgent.chatTitle')}</span>
                            </CardTitle>
                        </div>
                        <div className="agent-header-actions flex shrink-0 items-center gap-1">
                            <AgentHistory deskId={deskId} deviceId={recoveryConnection?.device_id} disabled={chat.hydrating || chat.contextUpdating || chat.permissionUpdating || !!chat.grantRevoking}
                                onDeleted={id => { if (chat.forgetConversation(id)) { setSelectedCapabilityIds([]); setSelectedIssues([]); } }}
                                onSelect={(id) => {
                                    if (!chat.selectConversation(id)) return false;
                                    setQuestion('');
                                    setSelectedCapabilityIds([]);
                                    setSelectedIssues([]);
                                    return true;
                                }} onNew={resetConversation} />
                        </div>
                    </div>
                </CardHeader>
                <CardContent className="flex min-h-0 flex-1 flex-col gap-2 px-3 pb-3 pt-0">
                    {chat.temporaryConversation && <p role="status" className="shrink-0 rounded-md bg-muted px-3 py-2 text-xs text-muted-foreground">
                        <strong>{t('pages.aiAgent.temporary.title')}</strong> {t('pages.aiAgent.temporary.description')}
                    </p>}
                    {chat.goal && <AgentGoalPanel key={`${permissionHistoryKey}:${chat.goal.goalId}:${subagents.selected?.task_id ?? 'main'}`} goal={chat.goal} connected={isConnected}
                        main={!subagents.selected} enabled={agentEnabled && isConnected && !chat.pendingGoalOpenRequest}
                        busy={chat.goalUpdating || chat.turnRunning || !!chat.deliveryState}
                        onDetails={() => setGoalDetailsOpen(true)} onAction={action => void chat.controlGoal(action)}
                        onReply={focusGoalComposer} onContinue={continueCompletedGoal} />}
                    <div role="tabpanel" id="agent-main-panel" aria-labelledby="agent-tab-main" hidden={!!subagents.selected} className={subagents.selected ? 'hidden' : 'contents'}>
                    {(pendingCount > 0 || runningTaskCount > 0) && (
                        <div className="flex w-full min-w-0 shrink-0 items-center gap-2 overflow-x-auto px-1 text-xs">
                            {pendingCount > 0 && <Button type="button" size="sm" variant="outline" className="shrink-0 border-amber-500/50"
                                onClick={() => jumpToPending()}>{t('pages.aiAgent.workspace.pendingCount', { count: pendingCount })}</Button>}
                            {runningTaskCount > 0 && <Button type="button" size="sm" variant="ghost" className="shrink-0" onClick={() => setTaskPanelSession(permissionHistoryKey)}>
                                {t('pages.aiAgent.workspace.runningTasks', { count: runningTaskCount })}</Button>}
                        </div>
                    )}
                    <div className="relative min-h-0 flex-1">
                    <div ref={scrollRef} onScroll={onScroll} data-testid="agent-scroll-area"
                        className="agent-scrollbar h-full overflow-y-auto overscroll-contain [overflow-wrap:anywhere]">
                    <div ref={contentRef} className="mx-auto w-full max-w-[840px] space-y-4 pb-4">
                    <AgentConversationMessages sessionId={chat.sessionId} messages={chat.messages} tools={chat.tools}
                        running={chat.running} evidence={chat.visualEvidence} notices={chat.contextNotices}
                        revealSource={chat.revealMessage}
                        displayNameForTool={displayNameForTool} exportBackup={exportBackup}
                        loading={chat.hydrating} hasMore={chat.hasMoreMessages} loadingOlder={chat.loadingOlderMessages}
                        onOlder={() => void chat.loadOlderMessages()}
                        before={<AgentDocumentPreviews previews={chat.documentPreviews} requestPage={chat.requestDocumentPreviewPage} />}>
                        <ScheduleProposalCards key={permissionHistoryKey} tools={chat.tools} running={chat.running}
                            awaitingOwnerId={pause?.kind === 'schedule' && pause.awaitingOwner ? pause.requestId : null}
                            deviceId={stableDeviceId} connectionId={deskId} onPendingCountChange={setPendingScheduleCount} />
                        <SkillProposalCards proposals={chat.skillProposals}
                            onRefresh={chat.refreshSnapshot} onDecided={chat.refreshSnapshot} />
                        {chat.contextCompressing && <p role="status" data-testid="agent-context-compressing"
                            className="flex items-center justify-center gap-2 text-xs text-muted-foreground">
                            <LoaderCircle className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                            {t('pages.aiAgent.contextCompressing')}
                        </p>}
                        {chat.partial && (
                            <MarkdownContent disableLinks className="w-full max-w-full rounded-lg bg-muted px-3 py-2 text-sm sm:max-w-[90%]">
                                {chat.partial}
                            </MarkdownContent>
                        )}
                    </AgentConversationMessages>

                    {externalSendReceipts.length > 0 && (
                        <div data-testid="ai-agent-external-send-results" className="space-y-3">
                            {externalSendReceipts.map(({ tool, receipt }) => (
                                <div
                                    key={tool.callId}
                                    className={`space-y-1 rounded-md border p-3 ${
                                        receipt.outcome === 'sent'
                                            ? 'border-emerald-500/50 bg-emerald-500/5'
                                            : receipt.outcome === 'outcome_unknown'
                                                ? 'border-amber-500/50 bg-amber-500/5'
                                                : 'border-slate-500/40 bg-muted/30'
                                    }`}
                                >
                                    <p className="flex items-center gap-2 text-sm font-medium">
                                        {receipt.outcome === 'outcome_unknown' && <AlertTriangle className="h-4 w-4" />}
                                        {t(`pages.aiAgent.externalSendResult.${receipt.outcome}`)}
                                    </p>
                                    <p className="text-xs text-muted-foreground">
                                        {t('pages.aiAgent.externalSendResultDescription.' + receipt.outcome)}
                                    </p>
                                    <p className="break-all text-xs text-muted-foreground">
                                        {tool.name} · {new Date(receipt.observed_at_unix_ms).toLocaleString()}
                                        {receipt.provider_receipt_id ? ` · ${receipt.provider_receipt_id}` : ''}
                                    </p>
                                </div>
                            ))}
                        </div>
                    )}
                    <div data-agent-pending={chat.pendingGoalOpenRequest ? '' : undefined} tabIndex={-1}>
                    {chat.pendingGoalOpenRequest && (
                        <div data-testid="ai-agent-goal-open-request" className="rounded-md border border-amber-500/50 bg-amber-500/5 px-3 py-2 text-xs">
                            <p className="font-medium">{t(chat.pendingGoalOpenRequest.targetGoalId
                                ? 'pages.aiAgent.goalRevisionTitle'
                                : 'pages.aiAgent.goalProposalTitle')}</p>
                            <p className="mt-1 whitespace-pre-wrap break-words">{chat.pendingGoalOpenRequest.goalText}</p>
                            {chat.pendingGoalOpenRequest.previousCompletedGoalId && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalContinuesPrevious', { goalId: chat.pendingGoalOpenRequest.previousCompletedGoalId })}
                            </p>}
                            <p className="mt-1 text-muted-foreground">
                                {t(chat.pendingGoalOpenRequest.targetGoalId
                                    ? 'pages.aiAgent.goalRevisionDetails'
                                    : 'pages.aiAgent.goalProposalDetails', {
                                    device: chat.pendingGoalOpenRequest.deviceId,
                                    revision: chat.pendingGoalOpenRequest.targetGoalRevision,
                                    expiry: new Date(chat.pendingGoalOpenRequest.expiresAtUnixMs).toLocaleString(),
                                })}
                            </p>
                            {!chat.pendingGoalOpenRequest.targetGoalId && goalBudgetPolicy && <GoalLimitsSummary policy={goalBudgetPolicy} />}
                            <p className="mt-1 text-muted-foreground">{t(chat.pendingGoalOpenRequest.targetGoalId
                                ? 'pages.aiAgent.goalRevisionBoundary'
                                : 'pages.aiAgent.goalProposalBoundary')}</p>
                            {!offPageReminderAvailable && <p className="mt-1 text-amber-700 dark:text-amber-300">
                                {t('pages.aiAgent.goalNoOffPageReminder')}
                            </p>}
                            <AgentGoalProposalControls automaticApproval={automaticApproval}
                                revision={Boolean(chat.pendingGoalOpenRequest.targetGoalId)}
                                busy={chat.goalOpenUpdating || chat.turnRunning || chat.pendingGoalOpenRequest.expiresAtUnixMs <= Date.now()}
                                canApprove={Boolean(chat.pendingGoalOpenRequest.targetGoalId || goalBudgetPolicy)}
                                onDecide={chat.decideGoalOpen} />
                        </div>
                    )}
                    </div>
                    {pendingDirectories.map(directory => <div key={directory.requestId} data-agent-pending tabIndex={-1}>
                        <AgentDirectoryApproval directory={directory} revision={chat.fileScope.revision}
                            deviceLabel={hasDevice ? undefined : requestDevices.selected.find(device => device.id === directory.deviceRef)?.name || directory.deviceRef || undefined}
                            disabled={!agentEnabled || !isConnected || chat.hydrating}
                            busy={chat.contextUpdating} onUpdate={updateDirectory} />
                    </div>)}
                    <AgentBackgroundTasks key={`tasks:${permissionHistoryKey}`}
                        open={taskPanelSession === permissionHistoryKey}
                        onOpenChange={open => setTaskPanelSession(open ? permissionHistoryKey : null)}
                        commands={chat.commandTasks} providers={chat.backgroundTasks} tools={chat.tools}
                        connected={isConnected} canCancelProvider={featureProfile.background_task_cancel}
                        cancelling={chat.taskCancelling} onCancel={chat.cancelTask} />
                    {(hasDevice || personalScope) && <AgentFileScope key={`directories:${permissionHistoryKey}`} scope={chat.fileScope} automaticApproval={automaticApproval} decisionPause={pause}
                        deskId={deskId} sessionTargetId={hasDevice && chat.sessionTargetReady ? (chat.sessionTarget?.target_id ?? null) : undefined}
                        devices={hasDevice ? undefined : requestDevices.selected}
                        connectionFor={requestDevices.connectionFor}
                        open={directorySession === permissionHistoryKey} onOpenChange={open => setDirectorySession(open ? permissionHistoryKey : null)}
                        disabled={!agentEnabled || !isConnected || chat.hydrating || chat.contextUpdating} onUpdate={updateDirectory}
                        showPendingActions={false} onPendingJump={id => jumpToPending(`agent-directory-${id}`)} />}
                    <div data-agent-pending={pendingPermissionCount > 0 ? '' : undefined} tabIndex={-1}>
                    <AgentPermissionRecords approvalRecords={featureProfile.approval_delegation ? <ApprovalReviews onLocate={record => {
                            if (record.task) {
                                subagents.openDetail(record.task);
                                if (record.source_kind === 'directory_proposal') setChildPanel({ taskId: record.task.task_id, panel: 'directories' });
                            } else {
                                subagents.closeDetail();
                                if (record.source_kind === 'directory_proposal') setDirectorySession(permissionHistoryKey);
                            }
                            setPermissionHistorySession(null);
                        }} session={chat.sessionId ?? null} open={permissionHistorySession === permissionHistoryKey}/> : undefined} key={permissionHistoryKey} requests={chat.permissionRequests} automaticApproval={automaticApproval}
                        open={permissionHistorySession === permissionHistoryKey}
                        onOpenChange={(open) => setPermissionHistorySession(open ? permissionHistoryKey : null)}
                        decisions={{
                            canDecide: request => featureProfile.permission_decision && request.inputRevision === chat.inputRevision,
                            disabled: !agentEnabled || !isConnected || chat.hydrating || chat.turnRunning || chat.permissionUpdating,
                            onDecide: chat.decidePermissionItems,
                        }}>
                            {(request) => (
                                <AgentPermissionRequest key={`${permissionHistoryKey}:${request.requestId}:${request.inputRevision}`}
                                    request={request} canDecide={featureProfile.permission_decision && request.inputRevision === chat.inputRevision}
                                    disabled={!agentEnabled || !isConnected || chat.hydrating || chat.turnRunning}
                                    busy={chat.permissionUpdating} waitingForTurn={chat.turnRunning}
                                    interactive={featureProfile.exec_pty ? {
                                        runId: chat.runId,
                                        browserConnectionId: chat.browserConnectionId,
                                        deviceId: stableDeviceId,
                                    } : undefined}
                                    onDecide={chat.decidePermissionItems} />
                            )}
                    </AgentPermissionRecords>
                    </div>
                    {hasDevice && featureProfile.exec_pty && Object.entries(exec.entries).map(([row, entry]) => {
                        const rowIndex = Number(row);
                        return (
                            <div key={row} data-testid="ai-agent-exec">
                                <ExecLifecycle
                                    entry={entry}
                                    onApprove={() => exec.approve(rowIndex)}
                                    onReject={() => exec.reject(rowIndex)}
                                    onCancel={() => exec.cancel(rowIndex)}
                                    onDismiss={() => exec.dismiss(rowIndex)}
                                    ptyClient={exec.ptyClient(rowIndex)}
                                    approvalDisabled={!agentEnabled}
                                />
                            </div>
                        );
                    })}
                    <AgentQueuedMessages items={chat.queuedMessages} onCancel={chat.cancelQueuedRequest} />
                    {chat.error && (
                        <Alert variant="destructive">
                            <AlertTitle>{t(chat.error === 'history_restore_failed'
                                ? 'pages.aiAgent.history.restoreError' : 'pages.aiAgent.chatErrorTitle')}</AlertTitle>
                            <AlertDescription>{chat.error === 'history_restore_failed' ? (
                                <div className="flex gap-2">
                                    <Button type="button" variant="outline" size="sm" onClick={chat.refreshSnapshot}>
                                        {t('pages.aiAgent.history.retry')}
                                    </Button>
                                    <Button type="button" variant="outline" size="sm" onClick={() => resetConversation()}>
                                        {t('pages.aiAgent.newConversation')}
                                    </Button>
                                </div>
                            ) : chat.error === 'selected_context_expired' ? t('pages.aiAgent.selectedContextExpired') : chat.error}</AlertDescription>
                        </Alert>
                    )}
                    {chat.sessionId && pause?.ownerQuestion && <AgentOwnerQuestion key={pause.ownerQuestion.waitId} session={chat.sessionId} wait={pause.ownerQuestion} refresh={chat.refreshSnapshot} />}
                    </div>
                    </div>
                    {showJumpToLatest && (
                        <Button type="button" variant="outline" size="icon" onClick={jumpToLatest}
                            className="agent-jump-action absolute bottom-3 right-3 rounded-full bg-background shadow-md"
                            aria-label={t('pages.aiAgent.scrollToLatest')}
                            title={t('pages.aiAgent.scrollToLatest')}>
                            <ArrowDown className="h-4 w-4" />
                        </Button>
                    )}
                    </div>
                    <form onSubmit={submit} className="agent-composer mx-auto w-full max-w-[840px] shrink-0 space-y-2 rounded-xl border bg-background p-3 shadow-sm">


                        {chat.deliveryState && <div role="status" className="flex items-center justify-between gap-2 text-sm">
                            <span>{t(chat.deliveryState === 'sending' ? 'pages.aiAgent.deliverySending' : 'pages.aiAgent.deliveryUnconfirmed')}</span>
                            {chat.deliveryState === 'unconfirmed' && <Button type="button" size="sm" variant="outline"
                                disabled={!isConnected || !chat.sessionTargetReady} onClick={() => void chat.retryDelivery()}>
                                {t('pages.aiAgent.deliveryRetry')}
                            </Button>}
                        </div>}
                        {startGoal && !offPageReminderAvailable && (
                            <p className="text-xs text-amber-700 dark:text-amber-300">{t('pages.aiAgent.goalNoOffPageReminder')}</p>
                        )}
                        {startGoal && previousCompletedGoalId && (
                            <p className="text-xs text-muted-foreground">
                                {t('pages.aiAgent.goalContinuesPrevious', { goalId: previousCompletedGoalId })}
                            </p>
                        )}
                        <AgentSubagentApprovalNotice agents={subagents} />
                        {chat.mainStopped && <Button type="button" variant="outline" size="sm"
                            disabled={chat.resumingQueue} onClick={() => void chat.resumeQueue()}>
                            {t('pages.aiAgent.resumeQueue')}
                        </Button>}
                        <AgentFailedRequests failed={chat.failedRequests} waitingIds={chat.waitingRequestIds} />
                        {provider.isSuccess && modelMissing && (
                            <p role="status" className="text-sm text-muted-foreground">
                                {t('pages.aiAgent.modelNotConfigured')}{' '}
                                <Link to="/system/ai-model" className="text-primary underline-offset-4 hover:underline">
                                    {t('pages.aiAgent.configureModel')}
                                </Link>
                            </p>
                        )}
                        <AgentComposerInput
                            ref={goalReplyRef}
                            sendDisabled={sendDisabled}
                            value={question}
                            onChange={(event) => setQuestion(event.target.value)}
                            placeholder={t('pages.aiAgent.questionPlaceholder')}
                            disabled={!agentEnabled || !isConnected || chat.hydrating || chat.contextUpdating || modelMissing}
                            className="min-h-16 max-h-40 w-full resize-y rounded-md border-0 bg-background px-3 py-2 shadow-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50"
                        />
                        <div className="flex min-w-0 items-center gap-1">
                            <AgentComposerMenu key={`${permissionHistoryKey}:${subagents.selected?.task_id ?? 'main'}`} sections={menuSections} onContext={hasDevice || issueSources ? () => setPanel('context') : undefined}
                                goal={(hasDevice || personalScope) ? {
                                    checked: startGoal, disabled: !agentEnabled || chat.turnRunning || !!chat.deliveryState,
                                    onCheckedChange: checked => { setStartGoal(checked); setPreviousCompletedGoalId(null); },
                                    summary: startGoal && goalBudgetPolicy ? <GoalLimitsSummary policy={goalBudgetPolicy} /> : undefined,
                                } : undefined} />
                            {startGoal && personalScope && <DropdownMenu>
                                <DropdownMenuTrigger asChild><Button type="button" variant="outline" size="sm">
                                    {goalDevice?.name || t('pages.aiAgent.goalSelectDevice')}
                                </Button></DropdownMenuTrigger>
                                <DropdownMenuContent>
                                    {requestDevices.selected.map(device => <DropdownMenuItem key={device.id}
                                        onSelect={() => setGoalTargetRef(device.id)}>{device.name}</DropdownMenuItem>)}
                                </DropdownMenuContent>
                            </DropdownMenu>}
                            {personalScope && <AgentRequestDevices selected={requestDevices.selected}
                                disabled={!requestDevices.ready || chat.hydrating} onChange={requestDevices.update} />}
                            {personalScope && requestDevices.error && <Button type="button" variant="ghost" size="sm"
                                aria-label={t(requestDevices.error)} onClick={requestDevices.reload}>
                                <RefreshCw className="h-4 w-4" aria-hidden="true" />
                            </Button>}
                            {!hasDevice && selectedIssues.length > 0 && <Button type="button" variant="ghost" size="sm" onClick={() => setPanel('context')}>
                                {t('pages.aiAgent.issueSources.selected', { count: selectedIssues.length })}
                            </Button>}
                            {(hasDevice || noReview || automaticApproval) && <AgentComposerStatus automaticApproval={automaticApproval} noReview={noReview}
                                onApprovalSettings={() => setApprovalSettingsOpen(true)}
                                contextCount={selectedContextCount} onContext={() => setPanel('context')} startGoal={startGoal}
                                onClearGoal={() => { setStartGoal(false); setPreviousCompletedGoalId(null); }} />}
                            <div className="ml-auto flex shrink-0 items-center gap-1">
                                <AgentContextMeter usage={chat.contextUsage} draft={question} />
                                <AgentComposerActions turnRunning={chat.turnRunning} canStop={chat.canStop}
                                    stopping={chat.stopping} onStop={chat.stop}
                                    allowQueue={canQueueInput}
                                    sendDisabled={sendDisabled} />
                            </div>
                        </div>
                    </form>
                    </div>
                    <div role="tabpanel" id="agent-child-panel" hidden={!subagents.selected} aria-labelledby={subagents.selected ? `agent-tab-${subagents.selected.task_id}` : undefined}
                        className={subagents.selected ? 'flex min-h-0 flex-1 flex-col' : 'hidden'}>
                    <AgentSubagents key={subagents.selected?.task_id} agents={subagents} connected={isConnected}
                        adjustment={selectedChildDraft && selectedChildDraft.revision === selectedChildTask?.input_revision ? selectedChildDraft.text : ''}
                        onAdjustmentChange={text => { if (selectedChildTask) setChildDrafts(previous => ({ ...previous,
                            [selectedChildTask.task_id]: { revision: selectedChildTask.input_revision, text } })); }}
                        panel={childPanel?.taskId === subagents.selected?.task_id ? childPanel?.panel ?? null : null}
                        onPanelChange={setSelectedChildPanel} deskId={deskId}
                        onDeviceSettings={() => setDeviceSettingsOpen(true)}
                        onApprovalSettings={featureProfile.approval_delegation ? () => setApprovalSettingsOpen(true) : undefined}
                        sessionTargetId={chat.sessionTargetReady ? (chat.sessionTarget?.target_id ?? null) : undefined}
                        capabilityList={<AgentCapabilityList entries={capabilities.snapshot?.entries ?? []}
                            loading={capabilities.loading} error={Boolean(capabilities.error)}
                            refreshDisabled={!agentEnabled || !isConnected} onRefresh={capabilities.refresh} />}
                        mainStopped={chat.mainStopped}
                        exportBackup={exportSessionBackup}
                        canDecide={agentEnabled && featureProfile.permission_decision}
                        interactive={featureProfile.exec_pty ? { browserConnectionId: chat.browserConnectionId, deviceId: stableDeviceId } : undefined} />
                    </div>
                    <AgentConversationTabs agents={subagents} attentionTasks={chat.subagents.attention_tasks}
                        mainNeedsApproval={pendingCount > 0 || Object.values(exec.entries).some(entry => entry.phase === 'awaiting')} />
                    <AgentStopConfirmation open={chat.stopConfirmation !== null} busy={chat.stopping}
                        onDismiss={chat.dismissStopConfirmation} onConfirm={chat.confirmStop} />
                    <AgentAttachments sessionId={chat.sessionId} open={attachmentsOpen}
                        onOpenChange={setAttachmentsOpen} showTrigger={false} />
                    <Sheet open={goalDetailsOpen} onOpenChange={setGoalDetailsOpen}>
                        <SheetContent className="w-full overflow-y-auto sm:max-w-xl" onCloseAutoFocus={event => {
                            if (goalComposerRequested.current) {
                                event.preventDefault();
                                goalComposerRequested.current = false;
                                goalReplyRef.current?.focus();
                            }
                        }}>
                            <SheetHeader><SheetTitle>{t('pages.aiAgent.goalStart')}</SheetTitle></SheetHeader>
                    {chat.goal && (
                        <div data-testid="ai-agent-goal" className="rounded-md border bg-muted/30 px-3 py-2 text-xs">
                            <div className="flex items-center justify-between gap-2">
                                <span className="min-w-0 truncate font-medium" title={chat.goal.goalText}>{chat.goal.goalText}</span>
                                <Badge variant="outline">{t(`pages.aiAgent.goalStates.${chat.goal.state}`)}</Badge>
                            </div>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalUsage', {
                                    slices: chat.goal.usedSlices,
                                    sliceLimit: chat.goal.limits.slices === null ? t('pages.aiAgent.goalBudgetDisabled') : chat.goal.limits.slices,
                                    tokens: chat.goal.usedModelTokens,
                                    tokenLimit: chat.goal.limits.modelTokens === null ? t('pages.aiAgent.goalBudgetDisabled') : chat.goal.limits.modelTokens,
                                })}
                            </p>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalBudgetUsage', {
                                    modelCalls: chat.goal.usedModelCalls,
                                    modelCallLimit: chat.goal.limits.modelCalls === null ? t('pages.aiAgent.goalBudgetDisabled') : chat.goal.limits.modelCalls,
                                    toolCalls: chat.goal.usedToolCalls,
                                    toolCallLimit: chat.goal.limits.toolCalls === null ? t('pages.aiAgent.goalBudgetDisabled') : chat.goal.limits.toolCalls,
                                    activeMinutes: Math.floor(chat.goal.usedActiveTimeMs / 60_000),
                                    activeMinuteLimit: chat.goal.limits.activeTimeMs === null ? t('pages.aiAgent.goalBudgetDisabled') : Math.floor(chat.goal.limits.activeTimeMs / 60_000),
                                })}
                            </p>
                            <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalIdentity', {
                                    revision: chat.goal.goalRevision,
                                    device: chat.goal.deviceId,
                                    deadline: chat.goal.deadlineUnixMs === null ? t('pages.aiAgent.goalBudgetDisabled')
                                        : new Date(chat.goal.deadlineUnixMs).toLocaleString(),
                                })}
                            </p>
                            {chat.goal.checkpointSummary && <p className="mt-1 line-clamp-2">{t('pages.aiAgent.goalCheckpoint', { summary: chat.goal.checkpointSummary })}</p>}
                            {chat.goal.previousCompletedGoalId && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalContinuesPrevious', { goalId: chat.goal.previousCompletedGoalId })}
                                {chat.goal.previousCompletionSummary && ` · ${chat.goal.previousCompletionSummary}`}
                            </p>}
                            {chat.goal.statusReason && <p className="mt-1 text-muted-foreground">{t(`pages.aiAgent.goalReasons.${chat.goal.statusReason}`, { defaultValue: chat.goal.statusReason })}</p>}
                            {chat.goal.nextAttemptUnixMs && <p className="mt-1 text-muted-foreground">
                                {t('pages.aiAgent.goalNextAttempt', { time: new Date(chat.goal.nextAttemptUnixMs).toLocaleString() })}
                            </p>}
                            {!subagents.selected && !['completed', 'failed', 'cancelled'].includes(chat.goal.state) && (
                                <div className="mt-2 flex flex-wrap gap-2">
                                    {chat.goal.state === 'waiting_user' && <Button size="sm" variant="outline"
                                        disabled={chat.goalUpdating || chat.turnRunning || !isConnected}
                                        onClick={focusGoalComposer}>
                                        {t('pages.aiAgent.goalReply')}</Button>}
                                    {goalPlanningAction(chat.goal) && <Button size="sm" variant="outline"
                                        disabled={chat.goalUpdating || chat.turnRunning || Boolean(chat.pendingGoalOpenRequest) || !isConnected}
                                        onClick={() => void chat.controlGoal(goalPlanningAction(chat.goal!)!)}>
                                        {t(chat.goal.state === 'paused' ? chat.goal.pauseReason === 'stalled' ? 'pages.aiAgent.goalRetry' : 'pages.aiAgent.goalResume' : 'pages.aiAgent.goalPause')}
                                    </Button>}
                                    <Button size="sm" variant="destructive" disabled={chat.goalUpdating || chat.turnRunning || !isConnected}
                                        onClick={() => void chat.controlGoal('cancel')}>{t('pages.aiAgent.goalCancel')}</Button>
                                </div>
                            )}
                            {!subagents.selected && chat.goal.state === 'completed' && !chat.pendingGoalOpenRequest && <Button type="button" size="sm" variant="outline" className="mt-2"
                                disabled={chat.turnRunning || !!chat.deliveryState}
                                onClick={() => { setGoalDetailsOpen(false); continueCompletedGoal(); }}>
                                {t('pages.aiAgent.goalStillIncomplete')}
                            </Button>}
                        </div>
                    )}
                        </SheetContent>
                    </Sheet>
                    <Sheet open={approvalSettingsOpen} onOpenChange={setApprovalSettingsOpen}>
                        <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
                            <SheetHeader><SheetTitle>{t('pages.aiAgent.approvalMode.title')}</SheetTitle></SheetHeader>
                            {approvalSettingsOpen && approvalSettingsAvailable && (
                                <><AgentAutomaticApproval chat={chat} enabled={agentEnabled} connected={isConnected} initializeEmpty={!personalScope} />
                                {approvalAccess.allowed && <Button type="button" variant="outline" className="mt-3" onClick={() => { setApprovalSettingsOpen(false); if (subagents.selected) setSelectedChildPanel('permissions'); else setPermissionHistorySession(permissionHistoryKey); }}><ListTree className="mr-2 size-4"/>{t('approval.records')}</Button>}</>
                            )}
                        </SheetContent>
                    </Sheet>
                    <Sheet open={deviceSettingsOpen} onOpenChange={setDeviceSettingsOpen}>
                        <SheetContent className="w-full sm:max-w-md">
                            <SheetHeader><SheetTitle>{t('pages.aiAgent.workspace.deviceSettings')}</SheetTitle></SheetHeader>
                            <p className="mt-4 text-sm">{t(agentEnabled
                                ? 'pages.aiAgent.workspace.agentOnDeviceEnabled'
                                : 'pages.aiAgent.workspace.agentOnDeviceDisabled')}</p>
                            <p className="mt-2 text-sm text-muted-foreground">{t('pages.aiAgent.workspace.deviceSettingsOwnerOnly')}</p>
                        </SheetContent>
                    </Sheet>
                    <AgentSchedules key={permissionHistoryKey} sessionId={chat.sessionId ?? null} open={schedulesOpen} onOpenChange={setSchedulesOpen} deviceId={stableDeviceId} />
                </CardContent>
            </Card>
        </>
    );
}

function AiAgentLivePage({
    featureProfile: suppliedFeatureProfile,
    personalScope,
}: {
    featureProfile?: AiAgentFeatureProfile | null;
    personalScope?: string;
}) {
    const target = useOptionalDeviceTarget();
    const deskId = target?.connectionId;
    const [searchParams] = useSearchParams();
    const { t } = useTranslation();
    const serverInfo = useQueryServerInfo({ query: { enabled: suppliedFeatureProfile === undefined } });
    const featureProfile = suppliedFeatureProfile === undefined
        ? serverInfo.data?.data?.ai_agent ?? null
        : suppliedFeatureProfile;
    const restricted = useRestrictedSession(deskId);
    const connection = target?.connection;
    const localPairingAvailable = canPairLocalDevice(connection?.device_id, serverInfo);

    if (suppliedFeatureProfile === undefined && serverInfo.isLoading) {
        return <div className="p-6"><Skeleton className="h-64 w-full" /></div>;
    }

    if (!hasAiAgentBrowserEntry(featureProfile)) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert>
                    <AlertTitle>{t('pages.aiAgent.unavailableTitle')}</AlertTitle>
                    <AlertDescription>
                        {t('pages.aiAgent.unavailableDescription')}
                    </AlertDescription>
                </Alert>
            </div>
        );
    }

    if (restricted.isRestricted) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert variant="destructive">
                    <AlertTitle>{t('pages.aiAgent.ownerOnlyTitle')}</AlertTitle>
                    <AlertDescription>{t('pages.aiAgent.ownerOnly')}</AlertDescription>
                </Alert>
            </div>
        );
    }

    if (personalScope) {
        return <div className="absolute inset-0 flex min-w-0 flex-col gap-2 overflow-hidden">
            <AiAgentWorkspace key={personalScope} personalScope={personalScope}
                initialConversationId={searchParams.get('conversation')} backTo="/ai-agent/conversations"
                deskId="" stableDeviceId="" localPairingAvailable={false}
                featureProfile={featureProfile} agentEnabled />
        </div>;
    }

    if (!deskId || !connection) {
        return (
            <div className="mx-auto max-w-3xl p-6">
                <Alert variant="destructive">
                    <AlertTitle>{t('pages.deskDashboard.notFound')}</AlertTitle>
                    <AlertDescription>{t('pages.deskDashboard.notFoundDesc')}</AlertDescription>
                </Alert>
            </div>
        );
    }

    return (
        <div className="absolute inset-0 flex min-w-0 flex-col gap-2 overflow-hidden">
            <AiAgentWorkspace
                backTo={target!.basePath}
                initialConversationId={searchParams.get('conversation')}
                deskId={deskId}
                stableDeviceId={connection.device_id ?? ''}
                localPairingAvailable={localPairingAvailable}
                featureProfile={featureProfile}
                agentEnabled={isAiAgentEnabled(connection.ai_agent_settings)}
            />
        </div>
    );
}

/** History is selected before mounting any live connection or approval hook. */
export default function AiAgentPage(props: { featureProfile?: AiAgentFeatureProfile | null; personalScope?: string }) {
    const [query] = useSearchParams();
    return query.has('source') ? <AgentConversationHistory sourceId={query.get('source') ?? ''}
        expectedRevision={query.has('revision') ? Number(query.get('revision')) : undefined} /> : <AiAgentLivePage {...props} />;
}

export function PersonalAiAgentPage({ enabled = true }: { enabled?: boolean }) {
    const user = useGetCurrentUser();
    const [query] = useSearchParams();
    if (query.has('source')) return <AiAgentPage />;
    if (enabled && user.isLoading) return <div className="p-6"><Skeleton className="h-64 w-full" /></div>;
    const account = user.data?.data;
    const personalScope = enabled && account && account.target_connection_id == null
        ? `personal:${account.user_id ?? 'owner'}` : undefined;
    return <AiAgentPage personalScope={personalScope} />;
}
