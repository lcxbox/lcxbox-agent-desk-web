import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { ArrowLeft, Bot } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import type { AgentSourceResolution } from '@/services/types';
import { agentSourceIdPattern } from '@/components/markdown-source-links';
import { AgentConversationMessages } from './agent-conversation-messages';
import { agentDataErrorKey, agentSourceAnchor, readAgentSource, readAgentSourceHistory, readAgentRetainedHistory,
    type AgentSourceHistory, type AgentRetainedHistory, type SourceHistorySelection } from './agent-data-api';
import { mergeAgentMessages } from './agent-message-history';
import { sourceTextParts } from './agent-source-text';
import { projectPersistedSnapshot, type AiAgentMessage } from './use-ai-agent-chat';

type Page = { kind: 'native'; history: AgentSourceHistory }
    | { kind: 'retained'; history: AgentRetainedHistory; view: 'context' | 'latest' };
type Selection = SourceHistorySelection | { view: 'retained'; cursor: string; originalView: 'context' | 'latest' };
const prefix = 'pages.aiAgent.memory.';

/** The agent page's history mode never constructs live connection or approval hooks. */
export function AgentConversationHistory({ sourceId, expectedRevision }: { sourceId: string; expectedRevision?: number }) {
    const { t, i18n } = useTranslation();
    const [source, setSource] = useState<AgentSourceResolution | null>(null);
    const [page, setPage] = useState<Page | null>(null);
    const currentPage = useRef<Page | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState('');
    const [locating, setLocating] = useState(true);
    const scroller = useRef<HTMLDivElement | null>(null);
    const scrollTarget = useRef<'latest' | { height: number; top: number } | null>(null);
    const epoch = useRef(0);
    const load = useCallback(async (selection: Selection) => {
        const stamp = ++epoch.current;
        const previous = currentPage.current;
        const accept = (next: Page | null) => { currentPage.current = next; setPage(next); };
        setLoading(true); setError('');
        scrollTarget.current = selection.view === 'latest' ? 'latest' : selection.view === 'older' && scroller.current
            ? { height: scroller.current.scrollHeight, top: scroller.current.scrollTop } : null;
        setLocating(selection.view === 'around');
        setSource(null); accept(null);
        try {
            if (!agentSourceIdPattern.test(sourceId)) throw new Error('Invalid source');
            const resolved = await readAgentSource(sourceId, expectedRevision);
            if (epoch.current !== stamp) return;
            setSource(resolved);
            const anchor = agentSourceAnchor(resolved);
            if (selection.view !== 'retained' && anchor) {
                try {
                    const history = await readAgentSourceHistory(resolved, selection);
                    if (epoch.current !== stamp) return;
                    accept({ kind: 'native', history: selection.view === 'older' && previous?.kind === 'native'
                        ? { ...history, messages: mergeAgentMessages(history.messages, previous.history.messages) } : history });
                    return;
                } catch { /* A retained window has its own owner and source checks. */ }
            }
            accept(null);
            if (resolved.locator && ['available', 'partial', 'redacted'].includes(resolved.availability)) {
                const view = selection.view === 'retained' ? selection.originalView : selection.view === 'latest' ? 'latest' : 'context';
                const history = await readAgentRetainedHistory(resolved, view, selection.view === 'retained' ? selection.cursor : undefined);
                if (epoch.current === stamp) accept({ kind: 'retained', history, view });
            }
        } catch (cause) {
            if (epoch.current === stamp) { scrollTarget.current = null; accept(null); setError(agentDataErrorKey(cause)); }
        } finally { if (epoch.current === stamp) setLoading(false); }
    }, [sourceId, expectedRevision]);
    useEffect(() => { void load({ view: 'around' }); return () => { epoch.current++; }; }, [load]);
    useLayoutEffect(() => {
        const target = scrollTarget.current;
        if (page && target && scroller.current) {
            if (target === 'latest') scroller.current.scrollTop = scroller.current.scrollHeight;
            else if (page.kind === 'native') scroller.current.scrollTop = target.top + scroller.current.scrollHeight - target.height;
            scrollTarget.current = null;
        }
    }, [page]);
    const anchor = source && agentSourceAnchor(source);
    const projection = useMemo(() => page?.kind === 'native'
        ? projectPersistedSnapshot({ messages: page.history.messages, actionPermissionReasons: page.history.actionPermissionReasons }) : null, [page]);
    const retainedMessages = useMemo((): AiAgentMessage[] => page?.kind === 'retained'
        ? page.history.fragments.map(fragment => ({ id: fragment.source.source_id,
            role: fragment.role === 'user' ? 'user' : fragment.role === 'assistant' ? 'assistant' : 'tool_result',
            text: fragment.source.content ?? '' })) : [], [page]);
    const focus = page?.kind === 'native' && anchor ? { messageId: anchor.message, scroll: locating,
        parts: sourceTextParts(source!, page.history.messages.find(message => message.id === anchor.message) ?? { id: '', text: '' }) }
        : page?.kind === 'retained' ? { messageId: page.history.anchor_source_id, scroll: locating } : undefined;
    const older = page?.kind === 'native' ? page.history.messagePage.hasMore : Boolean(page?.history.previous_cursor);
    const loadOlder = () => {
        if (page?.kind === 'native' && page.history.messagePage.nextBeforeMessageId)
            void load({ view: 'older', cursor: page.history.messagePage.nextBeforeMessageId });
        if (page?.kind === 'retained' && page.history.previous_cursor)
            void load({ view: 'retained', cursor: page.history.previous_cursor, originalView: page.view });
    };
    return <div className="absolute inset-0 flex min-w-0 flex-col overflow-hidden" data-testid="agent-conversation-history">
        <Card className="flex min-h-0 w-full flex-1 flex-col rounded-none border-0 shadow-none">
            <CardHeader className="agent-header shrink-0 px-3 py-2">
                <div className="flex items-center justify-between gap-2">
                    <div className="flex min-w-0 items-center gap-2">
                        <Button asChild variant="ghost" size="icon"><Link to="/ai-agent" aria-label={t('pages.aiAgent.backToOverview')}><ArrowLeft className="size-4" /></Link></Button>
                        <CardTitle className="flex min-w-0 items-center gap-2 text-base"><Bot className="size-4 shrink-0" />
                            <span className="truncate">{source?.source_device?.current_name || source?.source_device?.recorded_name || t('pages.aiAgent.chatTitle')}</span>
                        </CardTitle>
                    </div>
                    <span className="text-xs text-muted-foreground">{t(`${prefix}${page?.kind === 'retained' ? 'retainedHistory' : 'readOnlyHistory'}`)}</span>
                </div>
                {source?.recorded_at_unix_ms && <p className="text-xs text-muted-foreground">
                    {new Date(source.recorded_at_unix_ms).toLocaleString(i18n.language, { timeZone: source.statement_timezone ?? undefined })}
                    {source.statement_timezone && ` · ${source.statement_timezone}`}
                </p>}
                <div className="flex flex-wrap gap-2">
                    <Button variant="ghost" size="sm" disabled={loading} onClick={() => void load({ view: 'around' })}>{t(`${prefix}locateOriginal`)}</Button>
                    {source?.locator && <Button variant="ghost" size="sm" disabled={loading} onClick={() => void load({ view: 'latest' })}>{t(`${prefix}latestHistory`)}</Button>}
                    {page?.kind === 'retained' && page.history.next_cursor && <Button variant="ghost" size="sm" disabled={loading}
                        onClick={() => void load({ view: 'retained', cursor: page.history.next_cursor!, originalView: page.view })}>{t(`${prefix}newerHistory`)}</Button>}
                </div>
            </CardHeader>
            <CardContent ref={scroller} className="min-h-0 flex-1 overflow-y-auto px-3" data-testid="agent-scroll-area">
                <div className="mx-auto w-full max-w-[840px] space-y-4 pb-4">
                    {error && <p role="alert" className="text-destructive">{t(error)}</p>}
                    {source?.notice === 'memory_forgotten_original_kept' && <p className="text-sm">{t(`${prefix}memoryForgottenOriginalKept`)}</p>}
                    {page ? <AgentConversationMessages sessionId={page.kind === 'native' ? anchor?.session : undefined}
                        messages={projection?.messages ?? retainedMessages} tools={projection?.tools ?? []} running={false}
                        notices={page.kind === 'native' ? page.history.contextNotices : undefined} readOnly focus={focus}
                        loadingOlder={loading} hasMore={older} onOlder={loadOlder} /> : <>
                        {loading && <p role="status">{t(`${prefix}loading`)}</p>}
                        {source?.availability === 'unavailable' && <p>{t(`${prefix}sourceUnavailable`)}</p>}
                        {source?.availability === 'evidence_only' && <p>{t(`${prefix}evidenceOnly`)}</p>}
                        {error && source && <p className="text-sm text-muted-foreground">{t(`${prefix}originalUnavailable`)}</p>}
                        {source?.content && <pre className="whitespace-pre-wrap break-words font-sans text-sm">{source.content}</pre>}
                    </>}
                    {source?.availability === 'partial' && <p className="text-sm">{t(`${prefix}sourcePartial`)}</p>}
                    {source?.availability === 'redacted' && <p className="text-sm">{t(`${prefix}sourceRedacted`)}</p>}
                </div>
            </CardContent>
        </Card>
    </div>;
}
