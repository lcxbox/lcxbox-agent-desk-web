import { useTranslation } from 'react-i18next';
import type { AiAgentVisualEvidence } from './ai-agent-event';

export function AgentFrameTiming({ frame }: { frame: AiAgentVisualEvidence['frame_observation'] }) {
    const { t } = useTranslation();
    if (!frame) return null;
    const valid = Number.isSafeInteger(frame.received_at_unix_ms) && frame.received_at_unix_ms > 0
        && Number.isSafeInteger(frame.receipt_age_ms) && frame.receipt_age_ms >= 0
        && (frame.source_timestamp_ns == null || (Number.isInteger(frame.source_timestamp_ns) && frame.source_timestamp_ns >= 0))
        && ['latest_observed', 'fresh', 'unchanged_verified'].includes(frame.freshness);
    const status = !valid ? 'unavailable' : frame.freshness === 'latest_observed' ? 'latestObserved'
        : frame.freshness === 'fresh' ? 'freshFrame'
            : frame.freshness === 'unchanged_verified' ? 'unchangedFrame' : 'unavailable';
    return <div className="space-y-1 px-2 py-1 text-xs text-muted-foreground">
        <p>{t(`pages.aiAgent.observation.${status}`)}</p>
        {valid && <p>
            {t('pages.aiAgent.observation.frameAge')}: {t('pages.aiAgent.observation.frameAgeValue', { age: frame.receipt_age_ms })}
        </p>}
        {frame.source_timestamp_ns == null && <p>{t('pages.aiAgent.observation.sourceTimeUnavailable')}</p>}
        <p>{t('pages.aiAgent.observation.historicalFrame')}</p>
    </div>;
}
