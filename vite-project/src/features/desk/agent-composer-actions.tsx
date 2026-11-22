import { useTranslation } from 'react-i18next';
import { LoaderCircle, Send, X } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AgentComposerActions({ turnRunning, canStop, stopping, sendDisabled, allowQueue = false, onStop }: {
    turnRunning: boolean;
    canStop: boolean;
    stopping: boolean;
    sendDisabled: boolean;
    allowQueue?: boolean;
    onStop: () => void;
}) {
    const { t } = useTranslation();
    const stopLabel = t(stopping ? 'pages.aiAgent.stopping' : 'pages.aiAgent.stop');
    const sendLabel = t('pages.aiAgent.send');
    return <>
        {(turnRunning || canStop || stopping) && <Button type="button" className="agent-action agent-primary-action"
            aria-label={stopLabel} onClick={onStop} disabled={!canStop || stopping}>
            {turnRunning || stopping ? <LoaderCircle aria-hidden="true" className="h-4 w-4 shrink-0 animate-spin motion-reduce:animate-none" />
                : <X aria-hidden="true" className="h-4 w-4 shrink-0" />}
            <span className="agent-action-label">{stopLabel}</span>
        </Button>}
        {(!turnRunning || allowQueue) && !stopping && <Button type="submit" className="agent-action agent-primary-action"
            aria-label={sendLabel} disabled={sendDisabled}>
            <Send aria-hidden="true" className="h-4 w-4 shrink-0" />
            <span className="agent-action-label">{sendLabel}</span>
        </Button>}
    </>;
}
