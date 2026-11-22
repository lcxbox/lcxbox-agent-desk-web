import { Bot } from 'lucide-react';
import { Fragment, useEffect, useRef, useState, type ComponentProps } from 'react';
import { matchesAgentMessage, type AgentMessageFocus } from './agent-message-history';
import { useTranslation } from 'react-i18next';
import { MarkdownContent } from '@/components/markdown-content';
import { agentCitations } from './agent-data-api';
import type { ContextNoticeDto } from '@/services/types';
import type { AiAgentMessage, AiAgentToolActivity } from './use-ai-agent-chat';
import { AgentImages } from './agent-images';
import { AgentReasoning } from './agent-reasoning';
import { AgentToolGroup } from './agent-tool-group';
import { AgentToolCall, isHistoricalPermissionSkip } from './agent-tool-call';
import { AgentCommandResult } from './agent-command-result';
import { AgentResultAttachments } from './agent-attachments';
import { AgentContextNotices, noticeMessageId, type ContextSummarySourceNavigation } from './agent-context-notices';

/** Main and delegated sessions share presentation, never their input authority. */
export function AgentTranscript({ sessionId, messages, tools, running, evidence = [], notices = [], displayNameForTool, exportBackup, focus: requestedFocus, readOnly = false, revealSource }: {
    focus?: AgentMessageFocus; readOnly?: boolean;
    /** Loads earlier history until a summarized source is present. */
    revealSource?: (messageId: string) => Promise<boolean>;
    sessionId?: string; messages: AiAgentMessage[]; tools: AiAgentToolActivity[]; running: boolean;
    evidence?: ComponentProps<typeof AgentImages>['evidence']; notices?: ContextNoticeDto[]; displayNameForTool?: (name: string) => string;
    exportBackup?: (id: string) => Promise<void>;
}) {
    const { t } = useTranslation();
    // A summary source jump temporarily takes over the requested focus.
    const [sourceFocus, setSourceFocus] = useState<AgentMessageFocus>();
    useEffect(() => setSourceFocus(undefined), [requestedFocus?.messageId, sessionId]);
    const focus = sourceFocus ?? requestedFocus;
    const loaded = (id: string) => messages.some(message => matchesAgentMessage(message, id));
    const sources: ContextSummarySourceNavigation = {
        available: id => !!revealSource || loaded(id),
        open: id => {
            if (loaded(id) || !revealSource) setSourceFocus({ messageId: id });
            else void revealSource(id).then(found => { if (found) setSourceFocus({ messageId: id }); });
        },
    };
    const focusedId = messages.find(message => message.id === focus?.messageId)?.id
        ?? messages.find(message => matchesAgentMessage(message, focus?.messageId))?.id;
    const target = useRef<HTMLDivElement | null>(null);
    useEffect(() => {
        if (focus?.scroll === false) return;
        target.current?.scrollIntoView?.({ block: 'center' });
        target.current?.focus({ preventScroll: true });
    }, [focus?.messageId, focus?.scroll, focusedId]);
    const callElementId = (id: string) => `agent-call-${sessionId ?? 'local'}-${id}`;
    const displayNameForCall = (id?: string) => {
        const name = tools.find(tool => tool.callId === id)?.name;
        return name && displayNameForTool ? displayNameForTool(name) : undefined;
    };
    const renderTranscriptMessage = (message: AiAgentMessage) => (
        <Fragment key={message.id}>
        {(message.role !== 'assistant' || message.text || message.reasoning) && <div
            key={message.id}
            id={message.role === 'tool_call' ? callElementId(message.toolCallId!) : undefined}
            tabIndex={message.id === focusedId || message.role === 'tool_call' ? -1 : undefined}
            ref={message.id === focusedId ? node => { target.current = node; } : undefined}
            data-source-anchor={message.id === focusedId || undefined}
            data-agent-message-id={message.id}
            className={`rounded-lg px-3 py-2 text-sm ${message.id === focusedId ? 'ring-2 ring-primary ring-offset-2' : ''} ${
                message.role === 'user'
                    ? 'ml-auto max-w-[90%] bg-muted'
                    : message.role === 'tool_result' ? 'w-full max-w-full border bg-muted/30 sm:max-w-[90%]' : 'w-full max-w-full bg-transparent sm:max-w-[90%]'
            }`}
        >
            {message.role === 'goal_decision' && message.goalDecision ? <div role="status" className="space-y-1 border-l-2 border-muted-foreground/40 pl-3">
                <p className="flex items-center gap-2 font-medium"><Bot className="h-4 w-4" aria-hidden="true" />
                    {t(message.goalDecision.reviewer === 'system' ? 'pages.aiAgent.goalAiReviewFailed'
                        : message.goalDecision.status === 'approved' ? 'pages.aiAgent.goalAiReviewApproved' : 'pages.aiAgent.goalAiReviewDenied')}</p>
                <p className="whitespace-pre-wrap text-muted-foreground">{message.text}</p>
            </div> : message.role === 'tool_call' ? <AgentToolCall tool={tools.find(tool => tool.callId === message.toolCallId)} running={running}
                displayName={displayNameForCall(message.toolCallId)} initialOpen={message.id === focusedId} /> : message.role === 'tool_result' ? <>
                {message.permissionReason && <p className="mb-2 text-sm">{t('pages.aiAgent.permissionReasonLabel', { reason: message.permissionReason })}</p>}
                {isHistoricalPermissionSkip(message.text) && <p className="mb-2 text-sm text-amber-700 dark:text-amber-300">{t('pages.aiAgent.historicalPermissionSkip')}</p>}
                <AgentCommandResult text={message.text} tool={tools.find(tool => tool.callId === message.toolCallId)} onLocateCall={message.toolCallId ? () => { const target = document.getElementById(callElementId(message.toolCallId!)); target?.scrollIntoView({ block: 'center', behavior: 'smooth' }); target?.focus({ preventScroll: true }); } : undefined} onExportBackup={exportBackup} />
                <AgentResultAttachments sessionId={sessionId} text={message.text} />
            </> : message.role === 'assistant'
                ? <><AgentReasoning text={message.reasoning} />{message.text && <MarkdownContent disableLinks agentSources={agentCitations(message)}>{message.text}</MarkdownContent>}
                    {message.citationsIncomplete && <p className="text-xs text-muted-foreground">{t('pages.aiAgent.memory.citationsIncomplete')}</p>}</>
                : <p className="whitespace-pre-wrap">{message.id === focusedId && focus?.parts
                    ? <>{focus.parts.before}<mark className="bg-primary/20 text-foreground">{focus.parts.match}</mark>{focus.parts.after}</> : message.text}</p>}
        </div>}
        <AgentContextNotices sessionId={sessionId} sources={sources}
            notices={notices.filter(notice => noticeMessageId(notice, messages) === message.id)} />
        </Fragment>
                        );

    return <div data-testid="agent-shared-transcript" className="min-h-48 space-y-5 py-4">
        <AgentImages key={sessionId} sessionId={sessionId} evidence={evidence} readOnly={readOnly} focusedMessageId={focusedId}
            messages={messages} renderMessage={renderTranscriptMessage}
            renderReasoning={message => <div className="w-full max-w-full text-sm sm:max-w-[90%]"><AgentReasoning text={message.reasoning} /></div>}
            renderToolGroup={group => <AgentToolGroup messages={group} tools={tools}
                renderMessage={renderTranscriptMessage} displayNameForTool={displayNameForTool}
                focused={group.some(message => message.id === focusedId)} />} />
        <AgentContextNotices historical sessionId={sessionId} sources={sources}
            notices={notices.filter(notice => !noticeMessageId(notice, messages))} />
    </div>;
}
