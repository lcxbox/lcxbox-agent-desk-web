import { useId, useState } from 'react';
import { ChevronDown, ChevronUp, Target, WifiOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { AiAgentGoal, AiAgentGoalOwnerAction } from './use-ai-agent-chat';

export function goalPlanningAction(goal: Pick<AiAgentGoal, 'state' | 'pauseReason'>): AiAgentGoalOwnerAction | null {
    if (['completed', 'failed', 'cancelled', 'waiting_user'].includes(goal.state)) return null;
    if (goal.state === 'paused') return goal.pauseReason === 'stalled' ? 'retry_stalled' : 'resume';
    return ['queued', 'running', 'waiting_approval', 'waiting_work', 'waiting_device', 'waiting_model', 'blocked'].includes(goal.state) ? 'pause' : null;
}

export function goalProgressMessage(goal: Pick<AiAgentGoal, 'state' | 'statusReason' | 'checkpointSummary'>): string | null {
    if (goal.state === 'waiting_user') return goal.statusReason?.trim() || null;
    if (goal.state === 'blocked') return goal.statusReason?.trim() || null;
    const summary = goal.checkpointSummary?.trim();
    return summary && !summary.startsWith('Waiting for ') ? summary : null;
}

export function AgentGoalPanel({ goal, connected, main, enabled, busy, onDetails, onAction, onReply, onContinue }: {
    goal: AiAgentGoal;
    connected: boolean;
    main: boolean;
    enabled: boolean;
    busy: boolean;
    onDetails: () => void;
    onAction: (action: AiAgentGoalOwnerAction) => void;
    onReply: () => void;
    onContinue: () => void;
}) {
    const { t } = useTranslation();
    const [expanded, setExpanded] = useState(false);
    const contentId = useId();
    const title = t(main ? 'pages.aiAgent.goalPanelTitle' : 'pages.aiAgent.goalMainTitle');
    const action = goalPlanningAction(goal);
    const message = goalProgressMessage(goal);
    const terminal = ['completed', 'failed', 'cancelled'].includes(goal.state);
    return <section aria-label={title} data-testid="agent-goal-panel"
        className="mx-auto w-full min-w-0 max-w-[840px] shrink-0 rounded-lg border bg-muted/30 text-sm">
        <button type="button" aria-expanded={expanded} aria-controls={contentId}
            aria-label={t(expanded ? 'pages.aiAgent.goalCollapse' : 'pages.aiAgent.goalExpand')}
            aria-describedby={`${contentId}-summary ${contentId}-state${connected ? '' : ` ${contentId}-offline`}`}
            className="flex min-h-11 w-full min-w-0 items-center gap-2 rounded-lg px-3 py-2 text-left hover:bg-muted/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            onClick={() => setExpanded(value => !value)}>
            <Target aria-hidden="true" className="h-4 w-4 shrink-0" />
            <span className="sr-only sm:not-sr-only sm:shrink-0 sm:font-medium">{title}</span>
            <span id={`${contentId}-summary`} className="min-w-0 flex-1 truncate" title={goal.goalText}>{goal.goalText.replace(/\s+/g, ' ').trim()}</span>
            <span id={`${contentId}-state`} role="status" aria-live="polite" className="max-w-[45%] shrink-0 truncate rounded-md border px-2 py-0.5 text-xs font-medium">{t(`pages.aiAgent.goalStates.${goal.state}`)}</span>
            {!connected && <span title={t('pages.aiAgent.goalLastKnown')} className="shrink-0">
                <WifiOff aria-hidden="true" className="h-4 w-4 text-muted-foreground" />
                <span id={`${contentId}-offline`} className="sr-only">{t('pages.aiAgent.goalLastKnown')}</span>
            </span>}
            {expanded ? <ChevronUp aria-hidden="true" className="h-4 w-4 shrink-0" /> : <ChevronDown aria-hidden="true" className="h-4 w-4 shrink-0" />}
        </button>
        <div id={contentId} hidden={!expanded} className="max-h-[min(16rem,30dvh)] overflow-y-auto overscroll-contain px-3 pb-3 [overflow-wrap:anywhere]">
            {expanded && <>
                <p className="whitespace-pre-wrap break-words">{goal.goalText}</p>
                {message && <p role="status" aria-live="polite" className="mt-2 whitespace-pre-wrap break-words text-xs text-muted-foreground">
                    {/^[a-z][a-z0-9_]*$/.test(message)
                        ? t(`pages.aiAgent.goalReasons.${message}`, { defaultValue: message }) : message}
                </p>}
                <div className="mt-2 flex flex-wrap gap-2">
                    <Button type="button" size="sm" variant="ghost" onClick={onDetails}>{t('pages.aiAgent.goalDetails')}</Button>
                    {main && <>
                        {goal.state === 'waiting_user' && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={onReply}>
                            {t('pages.aiAgent.goalReply')}
                        </Button>}
                        {action && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={() => onAction(action)}>
                            {t(action === 'retry_stalled' ? 'pages.aiAgent.goalRetry' : action === 'resume' ? 'pages.aiAgent.goalResume' : 'pages.aiAgent.goalPause')}
                        </Button>}
                        {!terminal && <Button type="button" size="sm" variant="ghost" disabled={!enabled || busy} onClick={() => onAction('cancel')}>
                            {t('pages.aiAgent.goalCancel')}
                        </Button>}
                        {goal.state === 'completed' && <Button type="button" size="sm" variant="outline" disabled={!enabled || busy} onClick={onContinue}>
                            {t('pages.aiAgent.goalStillIncomplete')}
                        </Button>}
                    </>}
                </div>
            </>}
        </div>
    </section>;
}
