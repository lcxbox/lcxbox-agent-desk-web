import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { AgentApprovalModePicker } from './agent-approval-mode';
import type { useAiAgentChat } from './use-ai-agent-chat';

type ApprovalChat = Pick<ReturnType<typeof useAiAgentChat>,
    'sessionId' | 'hydrating' | 'contextUpdating' | 'approvalDelegation' | 'error'
    | 'approvalInitializing' | 'approvalInitializationFailed' | 'initializeApprovalConversation'>;

export function AgentAutomaticApproval({ chat, enabled, connected, initializeEmpty = true }: {
    chat: ApprovalChat;
    enabled: boolean;
    connected: boolean;
    initializeEmpty?: boolean;
}) {
    const { t } = useTranslation();
    useEffect(() => {
        if (initializeEmpty && enabled && connected && !chat.sessionId && !chat.hydrating && !chat.contextUpdating
            && !chat.approvalInitializing && !chat.approvalInitializationFailed && !chat.error) {
            void chat.initializeApprovalConversation();
        }
    }, [initializeEmpty, enabled, connected, chat.sessionId, chat.hydrating, chat.contextUpdating,
        chat.approvalInitializing, chat.approvalInitializationFailed, chat.error, chat.initializeApprovalConversation]);
    if (chat.sessionId) return <div className="space-y-2">
        <AgentApprovalModePicker key={chat.sessionId} session={chat.sessionId} disabled={!enabled} />
        {chat.approvalDelegation?.status === 'active' && <p className="text-xs text-muted-foreground">
            {t('pages.aiAgent.autoApprovalUsage', {
                reviews: chat.approvalDelegation.reviewsUsed, tokens: chat.approvalDelegation.tokensUsed,
            })}
        </p>}
    </div>;
    if (!initializeEmpty) return <p className="text-sm text-muted-foreground">{t('pages.aiAgent.approvalMode.openSession')}</p>;
    if (chat.approvalInitializationFailed || chat.error) return <div role="alert" className="text-sm text-destructive">
        <p>{chat.approvalInitializationFailed ? t('pages.aiAgent.approvalInitializationFailed')
            : chat.error === 'history_restore_failed' ? t('pages.aiAgent.history.restoreError') : chat.error}</p>
        <Button size="sm" variant="outline" className="mt-2"
            disabled={!enabled || !connected || chat.hydrating || chat.contextUpdating || chat.approvalInitializing}
            onClick={() => void chat.initializeApprovalConversation(true)}>{t('pages.aiAgent.history.retry')}</Button>
    </div>;
    return <p role="status" className="text-sm text-muted-foreground">{t('common.loading')}</p>;
}
