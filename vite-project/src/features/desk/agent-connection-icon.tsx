import { AiAgentIcon } from '@/components/ai-agent-icon';
import { useTranslation } from 'react-i18next';

export function AgentConnectionIcon({ connected, enabled }: { connected: boolean; enabled: boolean }) {
    const { t } = useTranslation();
    const label = t(!enabled ? 'pages.aiAgent.disabledTitle'
        : connected ? 'pages.aiAgent.signalConnected' : 'pages.aiAgent.signalConnecting');
    const color = !enabled ? 'text-muted-foreground' : connected ? 'text-green-500' : 'text-amber-500';
    return <span role="status" title={label} tabIndex={0} data-testid="agent-connection-icon"
        className="inline-flex shrink-0 rounded focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
        <AiAgentIcon className={`h-4 w-4 ${color}`} />
        <span className="sr-only">{label}</span>
    </span>;
}
