import { agentSourceHref } from '@/components/markdown-source-links';
import { useEffect, useRef, useState } from 'react';
import { Link, useLocation, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { SelectItem } from '@/components/ui/select';
import { SelectField } from '@/components/ui/select-field';
import type { AgentHistorySearchRequest, AgentHistorySearchResult, AgentSourceKind } from '@/services/types';
import { AgentRetainedConversations } from './agent-retained-conversations';
import { AiAgentHubLayout } from './ai-agent-hub-layout';
import { agentDataErrorKey, getAgentDataSettings, searchAgentHistory } from './agent-data-api';

const prefix = 'pages.aiAgent.historySearch.';
type SearchDraft = { text: string; days: string; kind: string; query: AgentHistorySearchRequest };
export default function AgentHistorySearch() {
    const { t, i18n } = useTranslation();
    const location = useLocation(), navigate = useNavigate();
    const restored = useRef<SearchDraft | undefined>(location.state?.agentHistorySearch);
    const [policyAttempt, setPolicyAttempt] = useState(0);
    const [limits, setLimits] = useState<{ queryChars: number; searchWindowDays: number } | null>(null);
    const [text, setText] = useState(restored.current?.text ?? '');
    const [days, setDays] = useState(restored.current?.days ?? '90');
    const [kind, setKind] = useState(restored.current?.kind ?? 'all');
    const [page, setPage] = useState<AgentHistorySearchResult | null>(null);
    const [input, setInput] = useState<AgentHistorySearchRequest | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const epoch = useRef(0);
    useEffect(() => () => { epoch.current++; }, []);
    const load = async (query: AgentHistorySearchRequest, more = false) => {
        const stamp = ++epoch.current;
        setLoading(true); setError('');
        if (!more) setPage(null);
        try {
            const result = await searchAgentHistory(query);
            if (stamp !== epoch.current) return;
            setInput(query);
            setPage(previous => more && previous ? { ...result, fragments: [...previous.fragments, ...result.fragments],
                partial: previous.partial || result.partial } : result);
        } catch (cause) { if (stamp === epoch.current) { setPage(null); setError(agentDataErrorKey(cause)); } }
        finally { if (stamp === epoch.current) setLoading(false); }
    };
    useEffect(() => {
        if (restored.current) void load({ ...restored.current.query, cursor: null });
        // Restore only the original request. The server revalidates each source.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);
    useEffect(() => {
        let active = true;
        setError('');
        void getAgentDataSettings().then(value => {
            const queryChars = value.policy.limits?.retrieval?.query_chars;
            const searchWindowDays = value.policy.limits?.retrieval?.search_window_days;
            if (!active) return;
            if (!queryChars || !searchWindowDays) { setError(agentDataErrorKey(null)); return; }
            setLimits({ queryChars, searchWindowDays });
            setDays(previous => String(Math.min(Number(previous), searchWindowDays)));
        }).catch(cause => { if (active) setError(agentDataErrorKey(cause)); });
        return () => { active = false; };
    }, [policyAttempt]);
    return <AiAgentHubLayout>
        <h1 className="text-2xl font-semibold">{t(`${prefix}title`)}</h1>
        <form className="flex flex-wrap items-end gap-3" onSubmit={event => {
            event.preventDefault(); if (!limits || Array.from(text).length > limits.queryChars) return; const now = Date.now(); const queries = text.trim() ? [text.trim()] : [];
            const query: AgentHistorySearchRequest = { queries, device_ref: null, conversation_id: null, source_kinds: kind === 'all' ? [] : [kind as AgentSourceKind],
                time_range: { from_unix_ms: Math.max(0, now - Math.min(Number(days) * 86400000, limits.searchWindowDays * 86400000)), until_unix_ms: now },
                sort: queries.length ? 'relevance' : 'time', limit: 10, cursor: null };
            navigate(location.pathname, { replace: true, state: { ...location.state, agentHistorySearch: { text, days, kind, query } } });
            void load(query);
        }}>
            <label className="min-w-48 flex-1 space-y-2 text-sm">{t(`${prefix}keywords`)}
                <Input value={text} aria-invalid={limits != null && Array.from(text).length > limits.queryChars} onChange={event => setText(event.target.value)} />
            </label>
            <label className="space-y-2 text-sm">{t(`${prefix}period`)}
                <SelectField aria-label={t(`${prefix}period`)} className="h-9 min-w-36" value={days} onValueChange={value => { if (value) setDays(value); }}>
                    {[...new Set([7, 30, 90].filter(value => value <= (limits?.searchWindowDays ?? 0)).concat(limits?.searchWindowDays ?? []))].sort((a, b) => a - b).map(days => String(days)).map(value => <SelectItem key={value} value={value}>{t(`${prefix}days`, { count: Number(value) })}</SelectItem>)}
                </SelectField>
            </label>
            <label className="space-y-2 text-sm">{t(`${prefix}kind`)}
                <SelectField aria-label={t(`${prefix}kind`)} className="h-9 min-w-44" value={kind} onValueChange={setKind}>
                    {['all', 'input', 'agent_message', 'receipt', 'native_status'].map(value => <SelectItem key={value} value={value}>{t(`${prefix}${value}`)}</SelectItem>)}
                </SelectField>
            </label>
            <Button type="submit" disabled={loading || !limits || Array.from(text).length > limits.queryChars}>{t(`${prefix}search`)}</Button>
        </form>
        {!input && <AgentRetainedConversations />}
        {loading && <p role="status">{t('pages.aiAgent.memory.loading')}</p>}
        {error && <p role="alert" className="text-destructive">{t(error)}</p>}
        {error && !limits && <Button variant="outline" onClick={() => setPolicyAttempt(value => value + 1)}>{t('pages.aiAgent.personalization.reload')}</Button>}
        {page?.partial && <p role="status" className="text-sm">{t(`${prefix}partial`)}</p>}
        {page && !loading && page.fragments.length === 0 && <p>{t(`${prefix}empty`)}</p>}
        {page?.fragments.map(({ source, kind: sourceKind }) => <Card key={source.source_id}><CardContent className="space-y-3 pt-5">
            <p className="text-xs text-muted-foreground">{t(`${prefix}${sourceKind}`)}{' · '}
                {source.source_device?.current_name ?? source.source_device?.recorded_name}{' · '}
                {new Date(source.recorded_at_unix_ms!).toLocaleString(i18n.language, { timeZone: source.statement_timezone ?? undefined })}
                {' · '}{source.statement_timezone}</p>
            <p className="whitespace-pre-wrap break-words text-sm">{source.content}</p>
            <Button variant="link" className="px-0" asChild><Link to={agentSourceHref(source.source_id, source.revision)}>
                {t('pages.aiAgent.memory.openOriginal')}</Link></Button>
        </CardContent></Card>)}
        {page?.next_cursor && input && <Button variant="outline" disabled={loading}
            onClick={() => void load({ ...input, cursor: page.next_cursor }, true)}>{t('pages.aiAgent.memory.more')}</Button>}
    </AiAgentHubLayout>;
}
