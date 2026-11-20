import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { MainRequestReceipt } from '@/services/types';
import { agentErrorMessage } from '@/lib/agent-error-i18n';

export interface AssistantFailedRequestsProps {
    /** Failed receipts of the current conversation. */
    failed: MainRequestReceipt[];
    /** Requests of the current conversation that are still queued or activated. */
    waitingIds: string[];
}

/**
 * Requests that could not start. The list opens by itself when a request that
 * this page saw waiting fails; failures loaded with the page stay collapsed.
 */
export function AssistantFailedRequests({ failed, waitingIds }: AssistantFailedRequestsProps) {
    const { t } = useTranslation();
    const seenWaiting = useRef(new Set<string>());
    const [open, setOpen] = useState(false);
    const waitingKey = waitingIds.join('\n');
    const failedKey = failed.map(request => request.requestId).join('\n');
    useEffect(() => {
        if (failedKey.split('\n').some(id => seenWaiting.current.has(id))) setOpen(true);
        for (const id of waitingKey.split('\n')) if (id) seenWaiting.current.add(id);
    }, [failedKey, waitingKey]);
    if (failed.length === 0) return null;
    const reason = (request: MainRequestReceipt) => {
        const generic = t(`pages.aiAssistant.queueFailure.${request.failure ?? 'unknown'}`);
        return request.errorCode == null ? generic : agentErrorMessage(t, request.errorCode, null, generic);
    };
    return (
        <details className="text-sm text-muted-foreground" open={open}
            onToggle={event => setOpen(event.currentTarget.open)}>
            <summary className="cursor-pointer">{t('pages.aiAssistant.failedQueuedRequests', { count: failed.length })}</summary>
            <ul className="space-y-1 px-2">
                {failed.map(request => (
                    <li key={request.requestId} className="flex gap-2">
                        <span>{new Date(request.acceptedAtMs).toLocaleString()}</span>
                        <span>{reason(request)}</span>
                    </li>
                ))}
            </ul>
        </details>
    );
}
