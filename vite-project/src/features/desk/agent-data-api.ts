import type { AgentHistoryWindow, AiAgentSessionSnapshotDto, AgentDataControlReceipt, AgentSourceResolution, MemoryClearRequest,
    MemoryDeleteRequest, MemoryListResult, MemoryRecord, AgentDataSettingsView,
    AgentDataSettingsUpdate, UserInstructions, UserInstructionsUpdate } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { deskErrorKeyOr, type ErrorCodeKeyMap } from '@/lib/desk-error-i18n';
import { agentSourceIdPattern } from '@/components/markdown-source-links';

const errors: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.REVISION_CONFLICT]: 'pages.aiAgent.memory.changed',
    [deskErrorCodeEnum.PERMISSION_ERROR]: 'pages.aiAgent.memory.inaccessible',
};

export class AgentDataApiError extends Error {
    readonly code?: number;
    constructor(code?: number) {
        super('Agent data request failed');
        this.code = code;
    }
}

export function agentDataErrorKey(error: unknown): string {
    return deskErrorKeyOr(errors, error instanceof AgentDataApiError ? error.code : undefined,
        'pages.aiAgent.memory.requestFailed');
}

async function request<T>(path: string, body?: unknown): Promise<T> {
    const response = await fetch(path, {
        method: body === undefined ? 'GET' : 'POST', credentials: 'include', cache: 'no-store',
        headers: body === undefined ? { Accept: 'application/json' }
            : { Accept: 'application/json', 'Content-Type': 'application/json' },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const envelope = response.ok ? await response.json() : null;
    if (!envelope?.success || envelope.code !== deskErrorCodeEnum.SUCCESS || envelope.data == null) throw new AgentDataApiError(envelope?.code);
    return envelope.data as T;
}

function positive(value: unknown): value is number {
    return typeof value === 'number' && Number.isSafeInteger(value) && value > 0;
}

function nonnegative(value: unknown): value is number {
    return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

export function validPersonalization(content: string, maximumBytes = Number.MAX_SAFE_INTEGER): boolean {
    return new TextEncoder().encode(content).byteLength <= maximumBytes
        && !/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(content);
}

export function validAgentTimezone(value: string): boolean {
    if (!value || value.length > 128 || value.trim() !== value) return false;
    try { new Intl.DateTimeFormat('en', { timeZone: value }).format(0); return true; }
    catch { return false; }
}

export function agentSettingsErrorKey(error: unknown): string {
    return deskErrorKeyOr({
        [deskErrorCodeEnum.REVISION_CONFLICT]: 'pages.aiAgent.personalization.changed',
        [deskErrorCodeEnum.PERMISSION_ERROR]: 'pages.aiAgent.personalization.inaccessible',
        [deskErrorCodeEnum.INVALID_PARAMS]: 'pages.aiAgent.personalization.invalid',
    }, error instanceof AgentDataApiError ? error.code : undefined,
    'pages.aiAgent.personalization.requestFailed');
}

function instructions(value: UserInstructions): UserInstructions {
    if (!positive(value?.revision) || typeof value.content !== 'string' || !validPersonalization(value.content)) {
        throw new AgentDataApiError();
    }
    return value;
}

function settingsView(value: AgentDataSettingsView): AgentDataSettingsView {
    const settings = value?.settings;
    const policy = value?.policy;
    if (!positive(settings?.revision) || typeof settings.automatic_memory_enabled !== 'boolean'
        || typeof settings.model_history_search_enabled !== 'boolean'
        || !nonnegative(settings.explanation_version)
        || !positive(policy?.revision) || !positive(policy.history_retention_days)) throw new AgentDataApiError();
    return value;
}

export async function getAgentPersonalization(): Promise<UserInstructions> {
    return instructions(await request<UserInstructions>('/api/my/ai-agent-user-instructions'));
}

export async function saveAgentPersonalization(body: UserInstructionsUpdate): Promise<UserInstructions> {
    if (!positive(body.expected_revision) || !validPersonalization(body.content)) throw new AgentDataApiError();
    const saved = instructions(await request<UserInstructions>('/api/my/ai-agent-user-instructions', body));
    if (saved.revision !== body.expected_revision + 1 || saved.content !== body.content) throw new AgentDataApiError();
    return saved;
}

export async function getAgentDataSettings(): Promise<AgentDataSettingsView> {
    return settingsView(await request<AgentDataSettingsView>('/api/my/ai-agent-settings'));
}

export async function saveAgentDataSettings(body: AgentDataSettingsUpdate): Promise<AgentDataSettingsView> {
    if (!positive(body.expected_revision) || typeof body.automatic_memory_enabled !== 'boolean'
        || typeof body.model_history_search_enabled !== 'boolean'
        || !nonnegative(body.explanation_version)) {
        throw new AgentDataApiError();
    }
    const saved = settingsView(await request<AgentDataSettingsView>('/api/my/ai-agent-settings', body));
    const { expected_revision, ...fields } = body;
    if (saved.settings.revision !== expected_revision + 1
        || Object.entries(fields).some(([key, value]) => saved.settings[key as keyof typeof saved.settings] !== value)) {
        throw new AgentDataApiError();
    }
    return saved;
}

function fact(value: MemoryRecord): boolean {
    return typeof value?.memory_id === 'string' && /^mem_[\w-]{22}$/.test(value.memory_id)
        && positive(value.revision) && typeof value.content === 'string' && value.content.trim().length > 0
        && (value.context == null || typeof value.context === 'string')
        && ['personal', 'device'].includes(value.scope)
        && ['active', 'pending_conflict', 'superseded'].includes(value.status)
        && Array.isArray(value.source_ids)
        && value.source_ids.every(id => typeof id === 'string' && agentSourceIdPattern.test(id));
}

export async function listAgentMemories(options: { cursor?: string; generation?: number;
    controlGeneration?: number; scope?: 'personal' | 'device' } = {}): Promise<MemoryListResult> {
    const query = new URLSearchParams({ limit: '20' });
    if (options.scope) query.set('scope', options.scope);
    if (options.cursor) {
        query.set('cursor', options.cursor);
        query.set('memory_state_generation', String(options.generation));
        query.set('control_generation', String(options.controlGeneration));
    }
    const page = await request<MemoryListResult>(`/api/my/ai-agent-memories?${query}`);
    if (!Array.isArray(page.memories) || !page.memories.every(fact)
        || new Set(page.memories.map(value => value.memory_id)).size !== page.memories.length
        || (options.scope && page.memories.some(value => value.scope !== options.scope))
        || (options.cursor && (page.memory_state_generation !== options.generation
            || page.control_generation !== options.controlGeneration || page.next_cursor === options.cursor))
        || !positive(page.memory_state_generation) || !positive(page.control_generation)
        || (page.next_cursor != null && !/^mem_[\w-]{22}$/.test(page.next_cursor))) {
        throw new AgentDataApiError();
    }
    return page;
}

async function control(path: string, body: MemoryDeleteRequest | MemoryClearRequest): Promise<AgentDataControlReceipt> {
    const receipt = await request<AgentDataControlReceipt>(path, body);
    if (receipt.operation_id !== body.operation_id || !['applied', 'already_applied', 'stale_target'].includes(receipt.outcome)
        || receipt.reason !== ('memory_id' in body ? 'memory_forget' : 'memory_clear')
        || !positive(receipt.memory_state_generation) || !positive(receipt.control_generation)) {
        throw new AgentDataApiError();
    }
    return receipt;
}

export function deleteAgentMemory(body: MemoryDeleteRequest): Promise<AgentDataControlReceipt> {
    return control('/api/my/ai-agent-memory/delete', body);
}

export function clearAgentMemories(body: MemoryClearRequest): Promise<AgentDataControlReceipt> {
    return control('/api/my/ai-agent-memory/clear', body);
}

export async function readAgentSource(sourceId: string, expectedRevision?: number): Promise<AgentSourceResolution> {
    const query = new URLSearchParams({ source_id: sourceId });
    const source = await request<AgentSourceResolution>(`/api/my/ai-agent-source?${query}`);
    if ((expectedRevision != null && (!positive(expectedRevision) || (source.availability !== 'unavailable' && source.revision !== expectedRevision)))
        || source.source_id !== sourceId || !['available', 'evidence_only', 'partial', 'redacted', 'unavailable'].includes(source.availability)
        || (source.availability === 'unavailable' && (source.content != null || source.locator != null))
        || (source.availability === 'evidence_only' && source.locator != null)
        || (source.content != null && typeof source.content !== 'string')) {
        throw new AgentDataApiError();
    }
    if (source.availability !== 'unavailable') {
        if (!positive(source.recorded_at_unix_ms) || typeof source.statement_timezone !== 'string'
            || source.statement_timezone.length > 128) throw new AgentDataApiError();
        try { new Intl.DateTimeFormat('en', { timeZone: source.statement_timezone }).format(0); }
        catch { throw new AgentDataApiError(); }
    }
    return source;
}

export function agentSourceAnchor(source: AgentSourceResolution): { session: string; message: string } | null {
    if (!['available', 'partial', 'redacted'].includes(source.availability) || !source.locator) return null;
    const value = source.locator;
    const message = value.message_id;
    const validId = (id: unknown): id is string => typeof id === 'string' && id.length > 0 && id.length <= 256 && !/[\s\p{Cc}]/u.test(id);
    if (!validId(value.conversation_id) || !validId(value.root_conversation_id) || !validId(message)
        || !nonnegative(value.fragment_index)
        || !nonnegative(value.byte_start) || !positive(value.byte_end) || value.byte_end <= value.byte_start) return null;
    return { session: value.conversation_id, message };
}

export type SourceHistorySelection = { view: 'around' | 'latest' } | { view: 'older'; cursor: string };
export type AgentSourceHistory = Pick<AiAgentSessionSnapshotDto, 'messages' | 'messagePage'>
    & Partial<Pick<AiAgentSessionSnapshotDto, 'actionPermissionReasons' | 'contextNotices'>>;

export function agentCitations(answer: { id: string; citations?: { answer_message_id: string; answer_revision: number;
    source_id: string; source_revision: number }[] }): { source_id: string; source_revision: number }[] {
    const values = answer.citations ?? [];
    if (!Array.isArray(values) || values.some(value => !value || value.answer_message_id !== answer.id || value.answer_revision !== 1
        || !positive(value.source_revision) || !agentSourceIdPattern.test(value.source_id))
        || new Set(values.map(value => value.source_id)).size !== values.length) return [];
    return values.map(value => ({ source_id: value.source_id, source_revision: value.source_revision }));
}

export async function readAgentSourceHistory(source: AgentSourceResolution, selection: SourceHistorySelection): Promise<AgentSourceHistory> {
    const anchor = agentSourceAnchor(source);
    if (!anchor) throw new AgentDataApiError();
    const query = new URLSearchParams({ session: anchor.session, message_limit: '100' });
    if (selection.view === 'around') query.set('message_around', anchor.message);
    if (selection.view === 'older') query.set('message_before', selection.cursor);
    const snapshot = await request<AiAgentSessionSnapshotDto>(`/api/my/ai-agent-session?${query}`);
    if (snapshot.sessionId !== anchor.session || !Array.isArray(snapshot.messages)
        || snapshot.messages.some(message => typeof message.id !== 'string' || typeof message.text !== 'string' || typeof message.role !== 'string')
        || new Set(snapshot.messages.map(message => message.id)).size !== snapshot.messages.length
        || !snapshot.messagePage || typeof snapshot.messagePage.hasMore !== 'boolean'
        || (snapshot.messagePage.hasMore && !snapshot.messagePage.nextBeforeMessageId)
        || (selection.view === 'around' && !snapshot.messages.some(message => message.id === anchor.message))) {
        throw new AgentDataApiError();
    }
    return { messages: snapshot.messages, messagePage: snapshot.messagePage,
        actionPermissionReasons: snapshot.actionPermissionReasons, contextNotices: snapshot.contextNotices };
}

export type AgentRetainedHistory = AgentHistoryWindow;

export async function readAgentRetainedHistory(source: AgentSourceResolution, view: 'context' | 'latest', cursor?: string): Promise<AgentRetainedHistory> {
    const query = new URLSearchParams({ source_id: source.source_id, revision: String(source.revision), view });
    if (cursor) query.set('cursor', cursor);
    const window = await request<AgentRetainedHistory>(`/api/my/ai-agent-history/window?${query}`);
    if (window.conversation_id !== source.locator?.conversation_id || !agentSourceIdPattern.test(window.anchor_source_id)
        || !positive(window.anchor_revision) || !positive(window.observed_through_generation) || typeof window.partial !== 'boolean'
        || !Array.isArray(window.fragments) || !window.fragments.length
        || new Set(window.fragments.map(value => value.source?.source_id)).size !== window.fragments.length
        || window.fragments.some(value => !value.source || !agentSourceIdPattern.test(value.source.source_id)
            || !positive(value.source.revision) || !['available', 'partial', 'redacted'].includes(value.source.availability)
            || value.source.locator?.conversation_id !== window.conversation_id || typeof value.source.content !== 'string'
            || !positive(value.source.recorded_at_unix_ms) || typeof value.source.statement_timezone !== 'string'
            || !validAgentTimezone(value.source.statement_timezone)
            || typeof value.role !== 'string' || !['input', 'agent_message', 'receipt', 'native_status'].includes(value.kind))
        || [window.previous_cursor, window.next_cursor].some(value => value != null && (typeof value !== 'string' || !value.startsWith('rh1_') || value === cursor))) throw new AgentDataApiError();
    return window;
}

export async function searchAgentHistory(input: import('@/services/types').AgentHistorySearchRequest):
    Promise<import('@/services/types').AgentHistorySearchResult> {
    const value = await request<import('@/services/types').AgentHistorySearchResult>('/api/my/ai-agent-history/search', input);
    if (!Array.isArray(value?.fragments) || !nonnegative(value.observed_through_generation)
        || typeof value.partial !== 'boolean' || !Array.isArray(value.reasons)
        || (value.next_cursor != null && (typeof value.next_cursor !== 'string' || !value.next_cursor.startsWith('oh1_')
            || value.next_cursor === input.cursor))
        || new Set(value.fragments.map(item => item.source.source_id)).size !== value.fragments.length
        || value.fragments.some(item => !item.source?.locator || !positive(item.source.revision) || !positive(item.source.recorded_at_unix_ms)
            || typeof item.source.statement_timezone !== 'string' || !validAgentTimezone(item.source.statement_timezone)
            || typeof item.source.content !== 'string'
            || !['available', 'partial', 'redacted'].includes(item.source.availability)
            || !['input', 'agent_message', 'receipt', 'native_status'].includes(item.kind))) throw new AgentDataApiError();
    return value;
}

export async function listRetainedAgentConversations(cursor?: string): Promise<import('@/services/types').AgentHistoryConversations> {
    const query = new URLSearchParams({ limit: '10', ...(cursor ? { cursor } : {}) });
    const value = await request<import('@/services/types').AgentHistoryConversations>(`/api/my/ai-agent-history/conversations?${query}`);
    if (!Array.isArray(value?.conversations) || typeof value.partial !== 'boolean'
        || new Set(value.conversations.map(item => item.conversation_id)).size !== value.conversations.length
        || (value.next_cursor != null && (typeof value.next_cursor !== 'string' || !value.next_cursor.startsWith('hc1_')
            || value.next_cursor === cursor))
        || value.conversations.some(item => !item.source?.locator || item.source.locator.root_conversation_id !== item.conversation_id
            || !positive(item.source.revision) || typeof item.source.content !== 'string' || !positive(item.source.recorded_at_unix_ms)
            || typeof item.execution_available !== 'boolean' || typeof item.active !== 'boolean'
            || !['available', 'partial', 'redacted'].includes(item.source.availability))) throw new AgentDataApiError();
    return value;
}
export async function deleteRetainedAgentConversation(conversation: string): Promise<void> {
    const result = await request<import('@/services/types').DeleteAiAgentSessionResponse>('/api/my/ai-agent-session/delete',
        { connection: '', session: conversation });
    if (result.deleted !== true) throw new AgentDataApiError();
}

export async function getAgentProcessing(): Promise<import('@/services/types').AgentProcessingStatus> {
    const value = await request<import('@/services/types').AgentProcessingStatus>('/api/my/ai-agent-processing');
    if (!positive(value?.observed_at_unix_ms) || !positive(value.history_retention_days) || !nonnegative(value.history_generation)
        || !nonnegative(value.stored_history_fragments) || !nonnegative(value.indexed_terms) || !Array.isArray(value.coverage)
        || typeof value.partial !== 'boolean'
) throw new AgentDataApiError();
    return value;
}

