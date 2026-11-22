import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle } from 'lucide-react';
import { Disclosure } from '@/components/ui/disclosure';
import type { AiAgentMessage, AiAgentToolActivity } from './use-ai-agent-chat';
import { hasUnknownOutcome } from './action-result-status';
import { isConfirmedNotSent } from './ai-agent-external-send';
import { useDecisionLabel } from './agent-tool-call';
import { outcomeUnknownReason, toolResultSummary } from './agent-result-summary';

export function AgentToolGroup({ messages, tools, renderMessage, displayNameForTool, focused = false }: {
    messages: AiAgentMessage[];
    tools: AiAgentToolActivity[];
    renderMessage: (message: AiAgentMessage) => ReactNode;
    displayNameForTool?: (name: string) => string; focused?: boolean;
}) {
    const { t } = useTranslation();
    const decisionLabel = useDecisionLabel();
    const callIds = [...new Set(messages.map(message => message.toolCallId).filter((id): id is string => Boolean(id)))];
    const entries = callIds.map(id => tools.find(tool => tool.callId === id)).filter((tool): tool is AiAgentToolActivity => Boolean(tool));
    const uncertain = entries.filter(tool => hasUnknownOutcome(tool.output, tool.callId));
    const known = entries.filter(tool => !uncertain.includes(tool) && !tool.permissionReview && !tool.decision);
    const reviews = [...new Set(entries.flatMap(tool => [
        ...tool.permissionReview ? [t(`pages.aiAgent.permissionState.${tool.permissionReview.state}`)] : [],
        ...tool.decision ? [decisionLabel(tool.decision)] : [],
    ]))];
    const failed = known.filter(tool => tool.status === 'failed');
    const toolLabel = (name: string) => displayNameForTool?.(name) ?? name;
    const firstFailureMessage = toolResultSummary(t, failed[0]?.output ?? null, toolLabel);
    // Native unknown receipts have their own result view; only the fixed texts are explained here.
    const firstUncertainMessage = outcomeUnknownReason(t, uncertain[0]?.output ?? null);
    const succeeded = known.filter(tool => tool.status === 'ok' && !isConfirmedNotSent(tool.name, tool.output)).length;
    const returned = known.filter(tool => tool.status === 'returned' || (tool.status === 'ok' && isConfirmedNotSent(tool.name, tool.output))).length;
    return <Disclosure defaultOpen={focused} className="w-full max-w-full rounded-md border bg-muted/30 px-3 py-2 sm:max-w-[90%]"
        summaryClassName="cursor-pointer text-sm" title={<span className="space-y-1">
            <span className="block font-medium">{t('pages.aiAgent.workspace.toolGroup', { count: callIds.length })}</span>
            {reviews.length > 0 && <span className="block text-xs text-muted-foreground">
                {reviews.join(' · ')}
            </span>}
            {(succeeded > 0 || returned > 0) && <span className="block text-xs text-muted-foreground">
                {[
                    succeeded > 0 && t('pages.aiAgent.workspace.toolGroupSucceeded', { count: succeeded }),
                    returned > 0 && t('pages.aiAgent.workspace.toolGroupReturned', { count: returned }),
                ].filter(Boolean).join(' · ')}
            </span>}
            {failed.length > 0 && <span className="flex items-start gap-1 break-words text-xs text-destructive">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                {t('pages.aiAgent.workspace.toolGroupFailed', { count: failed.length, name: displayNameForTool?.(failed[0].name) ?? failed[0].name })}
                {firstFailureMessage && <span>{firstFailureMessage}</span>}
            </span>}
            {uncertain.length > 0 && <span className="flex items-start gap-1 text-xs text-amber-700 dark:text-amber-300">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                {t('pages.aiAgent.workspace.toolGroupOutcomeUnknown', { count: uncertain.length })}
                {firstUncertainMessage && <span>{firstUncertainMessage}</span>}
            </span>}
        </span>}>
        <div className="mt-2 space-y-2 border-t pt-2">
            {messages.map(message => <div key={message.id}>{renderMessage(message)}</div>)}
        </div>
    </Disclosure>;
}
