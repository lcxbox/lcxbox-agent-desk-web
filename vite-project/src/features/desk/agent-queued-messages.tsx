import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { AiAgentQueuedMessage } from './use-ai-agent-chat';

const FOLDED_LINES = 4;
const FOLDED_CHARACTERS = 280;

/** Owner inputs waiting behind running work, in the order the server accepted them. */
export function AgentQueuedMessages({ items, onCancel }: {
    items: AiAgentQueuedMessage[];
    onCancel: (requestId: string) => Promise<void>;
}) {
    if (!items.length) return null;
    return <ol data-testid="agent-queued-messages" className="space-y-3">
        {items.map(item => <QueuedMessage key={item.id} item={item} onCancel={onCancel} />)}
    </ol>;
}

function QueuedMessage({ item, onCancel }: { item: AiAgentQueuedMessage; onCancel: (requestId: string) => Promise<void> }) {
    const { t } = useTranslation();
    const [expanded, setExpanded] = useState(false);
    const [cancelling, setCancelling] = useState(false);
    const long = item.text.length > FOLDED_CHARACTERS || item.text.split('\n').length > FOLDED_LINES;
    const cancel = (requestId: string) => {
        setCancelling(true);
        void onCancel(requestId).finally(() => setCancelling(false));
    };
    return <li data-agent-queued-message={item.id} className="ml-auto flex max-w-[90%] flex-col items-end gap-1">
        <div className="w-full rounded-lg bg-muted px-3 py-2 text-sm">
            <p className={`whitespace-pre-wrap ${long && !expanded ? 'line-clamp-4' : ''}`}>{item.text}</p>
            {long && <Button type="button" variant="link" size="sm" className="h-auto p-0 text-xs"
                aria-expanded={expanded} onClick={() => setExpanded(current => !current)}>
                {t(expanded ? 'pages.aiAgent.queuedMessage.collapse' : 'pages.aiAgent.queuedMessage.expand')}
            </Button>}
        </div>
        {item.state && <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <span>{t(`pages.aiAgent.queuedMessage.${item.state}`)}</span>
            {item.state === 'queued' && item.requestId && <Button type="button" variant="ghost" size="sm"
                className="h-6 px-2 text-xs" disabled={cancelling} onClick={() => cancel(item.requestId!)}>
                {t('pages.aiAgent.cancelQueuedRequest')}
            </Button>}
        </div>}
    </li>;
}
