import { useEffect, useRef, useState } from 'react';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import type { MainRequestReceipt } from '@/services/types';
import { agentErrorMessage } from '@/lib/agent-error-i18n';

export interface AssistantFailedRequestsProps {
    /** Failed receipts of the current conversation. */
    failed: MainRequestReceipt[];
    /** Requests of the current conversation that are still queued or activated. */
    waitingIds: string[];
}

/** The recorded cause of a failed request, or its localized generic reason. */
export function queueFailureReason(t: TFunction, request: MainRequestReceipt): string {
    const generic = t(`pages.aiAssistant.queueFailure.${request.failure ?? 'unknown'}`);
    return request.errorCode == null ? generic : agentErrorMessage(t, request.errorCode, null, generic);
}

/**
 * Requests that could not start. The list opens by itself once for each
 * request that this page saw waiting and that then failed; failures loaded
 * with the page stay collapsed, and a closed list stays closed until another
 * such failure appears.
 */
export function AssistantFailedRequests({ failed, waitingIds }: AssistantFailedRequestsProps) {
    const { t } = useTranslation();
    const seenWaiting = useRef(new Set<string>());
    const announced = useRef(new Set<string>());
    const [open, setOpen] = useState(false);
    const waitingKey = waitingIds.join('\n');
    const failedKey = failed.map(request => request.requestId).join('\n');
    useEffect(() => {
        const fresh = failedKey.split('\n')
            .filter(id => seenWaiting.current.has(id) && !announced.current.has(id));
        for (const id of fresh) announced.current.add(id);
        if (fresh.length > 0) setOpen(true);
        for (const id of waitingKey.split('\n')) if (id) seenWaiting.current.add(id);
    }, [failedKey, waitingKey]);
    if (failed.length === 0) return null;
    return (
        <details className="text-sm text-muted-foreground" open={open}
            onToggle={event => setOpen(event.currentTarget.open)}>
            <summary className="cursor-pointer">{t('pages.aiAssistant.failedQueuedRequests', { count: failed.length })}</summary>
            <ul className="space-y-1 px-2">
                {failed.map(request => (
                    <li key={request.requestId} className="flex gap-2">
                        <span>{new Date(request.acceptedAtMs).toLocaleString()}</span>
                        <span>{queueFailureReason(t, request)}</span>
                    </li>
                ))}
            </ul>
        </details>
    );
}
