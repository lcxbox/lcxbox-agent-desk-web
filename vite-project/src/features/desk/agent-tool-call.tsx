import { AgentCodeBlock } from './agent-code-block';
import { Disclosure } from '@/components/ui/disclosure';
import { useState } from 'react';
import { Ban, CheckCircle2, CircleDot, CircleHelp, Clock, Loader2, TriangleAlert, XCircle } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { decisionCardId, type AgentDecisionReceipt } from './agent-decision-receipt';
import type { AiAgentToolActivity } from './use-ai-agent-chat';
import { hasUnknownOutcome } from './action-result-status';
import { isConfirmedNotSent } from './ai-agent-external-send';

export function isHistoricalPermissionSkip(output: string | null | undefined): boolean {
    return output?.startsWith('not executed: waiting for user permission decision')
        || output?.startsWith('not executed: waiting for the existing user permission decision')
        || false;
}

function batchResult(tool: AiAgentToolActivity) {
    if (!['execute_ui_actions', 'send_background_input'].includes(tool.name) || !tool.output) return null;
    try {
        let value = JSON.parse(tool.output);
        if (typeof value.message === 'string') {
            try { value = JSON.parse(value.message); }
            catch {
                if (value.result === 'definitely_not_started') return { key: 'batchNotStarted', count: 0 };
                if (value.result === 'outcome_unknown') return { key: 'batchOutcomeUnknown', count: 0 };
                return null;
            }
        }
        if (value.status === 'completed' && Number.isInteger(value.completed_steps)) return { key: 'batchCompleted', count: value.completed_steps };
        if (value.status === 'stopped_on_error' && Number.isInteger(value.failed_step_number)) {
            const counted = value.failed_step_number > 0 && value.failed_step_number <= 20
                && value.completed_steps === value.failed_step_number - 1;
            const stage = counted && value.effect === 'may_have_effect' ? 'batchOutcomeUnknown'
                : counted && value.effect === 'no_effect' ? (value.completed_steps === 0 ? 'batchNotStarted' : 'batchPartiallyDispatched') : null;
            return { key: 'batchFailed', count: value.failed_step_number, stage, completed: value.completed_steps };
        }
    } catch { /* Unstructured transport errors use the original payload. */ }
    return null;
}

const decisionIcons = {
    pending: [Clock, 'text-amber-700 dark:text-amber-300'],
    submitted: [CircleDot, 'text-muted-foreground'],
    approved: [CheckCircle2, 'text-green-600 dark:text-green-400'],
    rejected: [XCircle, 'text-destructive'],
    conflicted: [TriangleAlert, 'text-amber-700 dark:text-amber-300'],
    withdrawn: [Ban, 'text-muted-foreground'],
} as const;

/** Label of a submitted request: its state and, once decided, who decided it. */
export function useDecisionLabel() {
    const { t } = useTranslation();
    return (decision: AgentDecisionReceipt) => [
        t(`pages.aiAgent.decision.state.${decision.state}`),
        decision.decidedBy && t(`pages.aiAgent.decision.by.${decision.decidedBy}`),
    ].filter(Boolean).join(' · ');
}

/** A request whose result reaches the agent only after the AI reviewer or the owner decides it. */
function DecisionToolCall({ tool, decision, displayName, initialOpen }: {
    tool: AiAgentToolActivity; decision: AgentDecisionReceipt; displayName?: string; initialOpen: boolean;
}) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(initialOpen);
    const label = useDecisionLabel()(decision);
    const [Icon, iconClass] = decisionIcons[decision.state];
    const locate = () => {
        const card = document.getElementById(decisionCardId(decision.kind, decision.requestId));
        card?.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
        card?.focus({ preventScroll: true });
    };
    return <div className="min-w-0 rounded-md border bg-muted/30 px-3 py-2"><Disclosure open={open} onOpenChange={setOpen} title={<>
            <span role="img" aria-label={label} title={label} className={`mr-2 inline-flex align-middle ${iconClass}`}>
                <Icon aria-hidden="true" className="size-4 shrink-0" />
            </span>
            {t('pages.aiAgent.toolCall.title')} · {displayName ?? tool.name} · {label}
        </>} summaryClassName="cursor-pointer select-none break-words text-sm [overflow-wrap:anywhere]">
        {open && <div className="mt-3 min-w-0 space-y-3 text-xs">
            <AgentCodeBlock label={t('pages.aiAgent.toolCall.input')} text={tool.argumentsJson} />
            {decision.state === 'pending' || decision.state === 'submitted'
                ? <p className="text-muted-foreground">{t('pages.aiAgent.decision.notReturned')}</p>
                : decision.reason && <p className="whitespace-pre-wrap break-words">{decision.reason}</p>}
        </div>}
    </Disclosure>
        {decision.state === 'pending' && decision.awaitingOwner && <Button type="button" size="sm" variant="outline"
            className="mt-2 border-amber-500/50" onClick={locate}>{t('pages.aiAgent.decision.review')}</Button>}
    </div>;
}

export function AgentToolCall({ tool, running, displayName, initialOpen = false }: { tool?: AiAgentToolActivity; running: boolean; displayName?: string; initialOpen?: boolean }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(initialOpen);
    if (!tool) return null;
    if (tool.decision) return <DecisionToolCall tool={tool} decision={tool.decision} displayName={displayName} initialOpen={initialOpen} />;
    const prefix = 'pages.aiAgent.toolCall.';
    const batch = batchResult(tool);
    const permissionSkip = isHistoricalPermissionSkip(tool.output);
    const outcomeUnknown = hasUnknownOutcome(tool.output, tool.callId);
    const status = tool.status === 'running' && !running ? 'missing' : tool.status;
    const displayStatus = tool.permissionReview?.state === 'approved' ? 'ok'
        : tool.permissionReview?.state === 'denied' ? 'failed'
        : status === 'ok' && isConfirmedNotSent(tool.name, tool.output) ? 'returned' : status;
    const StatusIcon = permissionSkip || outcomeUnknown ? CircleHelp : displayStatus === 'ok' ? CheckCircle2 : displayStatus === 'failed' ? XCircle : displayStatus === 'running' ? Loader2 : displayStatus === 'returned' ? CircleDot : CircleHelp;
    const statusLabel = tool.permissionReview
        ? t(`pages.aiAgent.permissionState.${tool.permissionReview.state}`)
        : t(`${prefix}${permissionSkip ? 'skippedForPermission' : outcomeUnknown ? 'outcomeUnknown' : displayStatus === 'ok' ? 'success' : displayStatus === 'failed' ? 'failure' : displayStatus === 'running' ? 'waiting' : displayStatus === 'returned' ? 'returned' : 'missing'}`);
    const statusClass = permissionSkip || outcomeUnknown ? 'text-amber-700 dark:text-amber-300' : displayStatus === 'ok' ? 'text-green-600 dark:text-green-400' : displayStatus === 'failed' ? 'text-destructive' : 'text-muted-foreground';
    return <Disclosure open={open} onOpenChange={setOpen} className="min-w-0 rounded-md border bg-muted/30 px-3 py-2" title={<>
            <span role="img" aria-label={statusLabel} title={statusLabel} className={`mr-2 inline-flex align-middle ${statusClass}`}>
                <StatusIcon aria-hidden="true" className={`size-4 shrink-0${status === 'running' && !permissionSkip && !outcomeUnknown ? ' animate-spin motion-reduce:animate-none' : ''}`} />
            </span>
            {t(`${prefix}title`)} · {tool.name === 'unknown' ? t(`${prefix}unknown`) : displayName ?? tool.name}
            {(permissionSkip || tool.permissionReview) && <> · {statusLabel}</>}
            {outcomeUnknown && !batch && <> · {t(`${prefix}inspectBeforeRetry`)}</>}
            {batch && <> · {t(`${prefix}${batch.key}`, { count: batch.count })}</>}
            {batch?.stage && <> · {t(`${prefix}${batch.stage}`, { count: batch.completed })}</>}
        </>} summaryClassName="cursor-pointer select-none break-words text-sm [overflow-wrap:anywhere]">

        {open && <div className="mt-3 min-w-0 space-y-3 text-xs">
            {permissionSkip && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAgent.historicalPermissionSkip')}</p>}
            {tool.permissionReason && <p className="whitespace-pre-wrap break-words">{t('pages.aiAgent.permissionReasonLabel', { reason: tool.permissionReason })}</p>}
            <AgentCodeBlock label={t(`${prefix}input`)} text={tool.name === 'unknown' ? t(`${prefix}missingInput`) : tool.argumentsJson} />
            {tool.permissionReview?.source && <p className="text-muted-foreground">{t(`pages.aiAgent.permissionDecisionSource.${tool.permissionReview.source}`)}</p>}
            {tool.output === null
                ? <p className="text-muted-foreground">{statusLabel}</p>
                : <AgentCodeBlock label={t(`${prefix}output`)} text={tool.output === '' ? t(`${prefix}empty`) : tool.output} />}
        </div>}
    </Disclosure>;
}
