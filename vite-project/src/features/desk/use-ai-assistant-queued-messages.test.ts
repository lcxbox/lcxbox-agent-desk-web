import { act, fireEvent, render, renderHook, waitFor } from '@testing-library/react';
import { createElement } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { AssistantFailedRequests } from './assistant-failed-requests';

import { SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED } from './constants';
import type { SignalingSubscriber } from './use-desk-signaling';
import { useAiAssistantChat } from './use-ai-assistant-chat';
import { deskErrorCodeEnum } from '@/services/types';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

type Receipt = {
    requestId: string; clientMessageId: string; state: 'queued' | 'activated' | 'cancelled' | 'failed';
    acceptedAtMs: number; question?: string | null; failure?: string | null; errorCode?: number;
};
type Snapshot = Record<string, unknown> & { seq: number; messages: unknown[]; mainRequests: Receipt[] };

const subagents = { active_tasks: [], task: null, tasks: null, parent_session_id: null, attention_tasks: [], attention_count: 0 };

/** A server whose snapshot and queue list reads can be held to model in-flight responses. */
async function setup(initial: Partial<Snapshot> = {}, options: { queueOnly?: boolean } = {}) {
    const server = {
        snapshot: { controlRevision: 1, inputRevision: 1, mainStopped: false, subagents, sessionId: 'root',
            seq: 1, active: false, messages: [], mainRequests: [], ...initial } as Snapshot,
        /** List reads mirror the snapshot receipts unless set. */
        list: null as Receipt[] | null,
        holdSnapshots: false,
        holdLists: false,
    };
    const held: Array<() => void> = [];
    const hold = () => new Promise<void>(resolve => held.push(resolve));
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
        if (url.endsWith('/requests/cancel')) {
            const { requestId } = JSON.parse(String(init?.body)) as { requestId: string };
            server.snapshot.mainRequests = server.snapshot.mainRequests.filter(item => item.requestId !== requestId);
            if (server.list) server.list = server.list.filter(item => item.requestId !== requestId);
            return { ok: true, json: async () => ({ success: true, data: true }) };
        }
        if (url.includes('/requests?')) {
            const items = structuredClone(server.list ?? server.snapshot.mainRequests);
            if (server.holdLists) await hold();
            return { ok: true, json: async () => ({ success: true, data: { sessionId: 'root', items } }) };
        }
        // A response reflects the server at the moment it was requested.
        const data = structuredClone(server.snapshot);
        if (server.holdSnapshots) await hold();
        return { ok: true, json: async () => ({ success: true, data }) };
    });
    vi.stubGlobal('fetch', fetchMock);
    let subscriber: SignalingSubscriber | null = null;
    const sendMessage = vi.fn((_type: number, _payload: unknown, _to?: string, id?: string) => id!);
    const hook = renderHook(() => useAiAssistantChat({ deskId: options.queueOnly ? '' : 'queue-view',
        initialConversationId: 'conversation', queueOnly: options.queueOnly,
        subscribe: handler => { subscriber = handler; return () => { subscriber = null; }; }, sendMessage }));
    const settle = () => act(async () => { for (let i = 0; i < 5; i += 1) await new Promise(resolve => setTimeout(resolve, 0)); });
    if (!options.queueOnly) await waitFor(() => expect(hook.result.current.sessionId).toBe('root'));
    const input = (index: number) => ({
        id: (sendMessage.mock.calls[index][1] as { client_message_id: string }).client_message_id,
        stream: sendMessage.mock.calls[index][3]!,
    });
    const deliver = async (stream: string, event: Record<string, unknown>) => {
        await act(async () => subscriber?.({ request_id: stream, signaling_type: SIGNALING_TYPE_CODE_AI_ASSISTANT_UPDATED,
            signaling_data: { request_id: stream, ...event } }));
        await settle();
    };
    const queued = (index: number, requestId: string, waiting?: boolean) => deliver(input(index).stream, {
        seq: 1, kind: 'status', status: 'queued', main_request_id: requestId, main_request_session_id: 'root',
        ...(waiting === undefined ? {} : { main_request_waiting: waiting }) });
    const refresh = async () => {
        server.snapshot = { ...server.snapshot, seq: server.snapshot.seq + 1 };
        act(() => hook.result.current.refreshSnapshot());
        await settle();
    };
    const release = async () => {
        server.holdSnapshots = false;
        server.holdLists = false;
        held.splice(0).forEach(resolve => resolve());
        await settle();
    };
    const send = (question: string) => act(() => { expect(hook.result.current.start(question)).toBe(true); });
    const view = () => ({
        messages: hook.result.current.messages.filter(message => message.role === 'user').map(message => message.text),
        queued: hook.result.current.queuedMessages.map(item => `${item.text}:${item.state ?? 'sending'}`),
    });
    return { ...hook, server, fetchMock, sendMessage, input, deliver, queued, refresh, release, send, settle, view };
}

const receipt = (requestId: string, clientMessageId: string, state: Receipt['state'], acceptedAtMs: number, question?: string): Receipt =>
    ({ requestId, clientMessageId, state, acceptedAtMs, ...(question === undefined ? {} : { question }) });

describe('assistant queued messages', () => {
    afterEach(() => { vi.unstubAllGlobals(); });

    it('keeps an idle send at the end of the conversation until it is saved', async () => {
        const chat = await setup({ messages: [{ id: 'earlier', role: 'user', text: 'Earlier' }] });
        try {
            chat.send('Check the disk');
            const { id } = chat.input(0);
            expect(chat.view()).toEqual({ messages: ['Earlier', 'Check the disk'], queued: [] });
            chat.server.snapshot.mainRequests = [receipt('r1', id, 'queued', 1, 'Check the disk')];
            await chat.queued(0, 'r1', false);
            expect(chat.view()).toEqual({ messages: ['Earlier', 'Check the disk'], queued: [] });
            chat.server.snapshot.mainRequests = [receipt('r1', id, 'activated', 1, 'Check the disk')];
            await chat.refresh();
            expect(chat.view()).toEqual({ messages: ['Earlier', 'Check the disk'], queued: [] });
            chat.server.snapshot = { ...chat.server.snapshot, active: true, requestId: 'r1', mainRequests: [],
                messages: [...chat.server.snapshot.messages, { id, role: 'user', text: 'Check the disk' }] };
            await chat.refresh();
            expect(chat.view()).toEqual({ messages: ['Earlier', 'Check the disk'], queued: [] });
            expect(chat.result.current.messages.filter(message => message.id === id)).toHaveLength(1);
        } finally { chat.unmount(); }
    });

    it.each([
        ['an earlier queued request', { mainRequests: [receipt('other', 'elsewhere', 'queued', 1, 'Earlier input')] }],
        ['a stopped conversation', { mainStopped: true }],
        ['a pending permission request', { permissionRequests: [{ requestId: 'p', inputRevision: 1, state: 'pending', items: [] }] }],
        ['a decision pause', { decisionPause: { kind: 'skill', requestId: 'p1', toolCallId: 'call-1', awaitingOwner: true } }],
        ['a running turn', { active: true, requestId: 'running' }],
    ])('places a send behind %s in the queue area', async (_case, initial) => {
        const chat = await setup(initial as Partial<Snapshot>);
        try {
            chat.send('Next step');
            expect(chat.view().messages).not.toContain('Next step');
            expect(chat.view().queued.at(-1)).toBe('Next step:sending');
        } finally { chat.unmount(); }
    });

    it('moves a send once when the server reports a different placement', async () => {
        const chat = await setup();
        try {
            chat.send('Guessed idle');
            chat.server.snapshot.mainRequests = [receipt('r1', chat.input(0).id, 'queued', 1, 'Guessed idle')];
            await chat.queued(0, 'r1', true);
            expect(chat.view()).toEqual({ messages: [], queued: ['Guessed idle:queued'] });
        } finally { chat.unmount(); }
        const stopped = await setup({ mainStopped: true });
        try {
            stopped.send('Guessed busy');
            expect(stopped.view()).toEqual({ messages: [], queued: ['Guessed busy:sending'] });
            stopped.server.snapshot.mainRequests = [receipt('r1', stopped.input(0).id, 'queued', 1, 'Guessed busy')];
            await stopped.queued(0, 'r1', false);
            expect(stopped.view()).toEqual({ messages: ['Guessed busy'], queued: [] });
            // The turn state changing later does not move it again.
            stopped.server.snapshot = { ...stopped.server.snapshot, mainStopped: false, active: true, requestId: 'other' };
            await stopped.refresh();
            expect(stopped.view()).toEqual({ messages: ['Guessed busy'], queued: [] });
        } finally { stopped.unmount(); }
    });

    it('keeps an unconfirmed send through retries and removes it when delivery is rejected', async () => {
        const chat = await setup({ active: true, requestId: 'running' });
        chat.sendMessage.mockImplementationOnce(() => { throw new Error('offline'); });
        try {
            chat.send('Retry me');
            expect(chat.result.current.deliveryState).toBe('unconfirmed');
            await chat.refresh();
            expect(chat.view().queued).toEqual(['Retry me:sending']);
            await act(async () => { await chat.result.current.retryDelivery(); });
            await chat.settle();
            expect(chat.view().queued).toEqual(['Retry me:sending']);
            await chat.deliver(chat.input(0).stream, { seq: 1, kind: 'error', error: { message: 'Rejected' } });
            expect(chat.view()).toEqual({ messages: [], queued: [] });
        } finally { chat.unmount(); }
    });

    it('keeps a busy send in place from sending through its receipt and running turn changes', async () => {
        const chat = await setup({ active: true, requestId: 'running',
            mainRequests: [receipt('other', 'elsewhere', 'queued', 1, 'From another page')] });
        try {
            expect(chat.view().queued).toEqual(['From another page:queued']);
            chat.send('Mine');
            expect(chat.view().queued).toEqual(['From another page:queued', 'Mine:sending']);
            const { id } = chat.input(0);
            // The event arrives before any receipt read returns.
            chat.server.holdLists = true;
            chat.server.holdSnapshots = true;
            chat.server.snapshot.mainRequests = [...chat.server.snapshot.mainRequests, receipt('r1', id, 'queued', 2, 'Mine')];
            await chat.queued(0, 'r1', true);
            expect(chat.view().queued).toEqual(['From another page:queued', 'Mine:queued']);
            expect(chat.result.current.queuedMessages[1].requestId).toBe('r1');
            await chat.release();
            expect(chat.view().queued).toEqual(['From another page:queued', 'Mine:queued']);
            chat.server.snapshot = { ...chat.server.snapshot, active: false };
            await chat.refresh();
            expect(chat.view()).toEqual({ messages: [], queued: ['From another page:queued', 'Mine:queued'] });
        } finally { chat.unmount(); }
    });

    it('shows a list read that returns before the snapshot once', async () => {
        const chat = await setup({ active: true, requestId: 'running' });
        try {
            chat.send('Only once');
            const { id } = chat.input(0);
            chat.server.snapshot.mainRequests = [receipt('r1', id, 'queued', 1, 'Only once')];
            chat.server.holdSnapshots = true;
            await chat.queued(0, 'r1');
            expect(chat.view().queued).toEqual(['Only once:queued']);
            await chat.release();
            expect(chat.view()).toEqual({ messages: [], queued: ['Only once:queued'] });
        } finally { chat.unmount(); }
    });

    it('shows an activated input until its message is saved, then only in the conversation', async () => {
        const chat = await setup({ active: true, requestId: 'running',
            mainRequests: [receipt('r1', 'elsewhere', 'activated', 1, 'Starting now')] });
        try {
            expect(chat.result.current.queuedMessages).toEqual([{ id: 'elsewhere', text: 'Starting now', requestId: 'r1', state: 'activated' }]);
            chat.server.snapshot = { ...chat.server.snapshot, requestId: 'r1', mainRequests: [],
                messages: [{ id: 'elsewhere', role: 'user', text: 'Starting now' }] };
            await chat.refresh();
            expect(chat.view()).toEqual({ messages: ['Starting now'], queued: [] });
        } finally { chat.unmount(); }
    });

    it('removes a cancelled input at once and ignores an older snapshot still in flight', async () => {
        const chat = await setup({ active: true, requestId: 'running' });
        try {
            chat.send('Cancel me');
            const { id } = chat.input(0);
            chat.server.snapshot.mainRequests = [receipt('r1', id, 'queued', 1, 'Cancel me')];
            await chat.queued(0, 'r1', true);
            expect(chat.view().queued).toEqual(['Cancel me:queued']);
            chat.server.holdSnapshots = true;
            act(() => chat.result.current.refreshSnapshot());
            await chat.settle();
            await act(async () => { await chat.result.current.cancelQueuedRequest('r1'); });
            expect(chat.view()).toEqual({ messages: [], queued: [] });
            expect(chat.result.current.pendingInputCount).toBe(0);
            await chat.release();
            expect(chat.view()).toEqual({ messages: [], queued: [] });
        } finally { chat.unmount(); }
    });

    it('lets a list read add receipts but only a snapshot remove them', async () => {
        const chat = await setup({ active: true, requestId: 'running',
            mainRequests: [receipt('x', 'elsewhere', 'queued', 1, 'From another page')] });
        try {
            chat.send('A');
            const a = chat.input(0).id;
            chat.server.snapshot.mainRequests = [...chat.server.snapshot.mainRequests, receipt('ra', a, 'queued', 2, 'A')];
            await chat.queued(0, 'ra', true);
            chat.send('B');
            const b = chat.input(1).id;
            // The list no longer has A or the other page's input, and the snapshot is not newer yet.
            chat.server.list = [receipt('rb', b, 'queued', 3, 'B')];
            chat.server.holdSnapshots = true;
            await chat.queued(1, 'rb', true);
            expect(chat.view().queued).toEqual(['From another page:queued', 'A:queued', 'B:queued']);
            await chat.release();
            chat.server.list = null;
            chat.server.snapshot = { ...chat.server.snapshot, requestId: 'ra', mainRequests: [receipt('rb', b, 'queued', 3, 'B')],
                messages: [{ id: a, role: 'user', text: 'A' }] };
            await chat.refresh();
            expect(chat.view()).toEqual({ messages: ['A'], queued: ['B:queued'] });
        } finally { chat.unmount(); }
    });

    it('orders queued inputs by acceptance and drops failed ones from the queue area', async () => {
        const chat = await setup({ active: true, requestId: 'running', mainRequests: [
            receipt('late', 'late-input', 'queued', 5, 'Second'),
            receipt('early', 'early-input', 'queued', 3, 'First'),
        ] });
        try {
            chat.send('Mine');
            expect(chat.view().queued).toEqual(['First:queued', 'Second:queued', 'Mine:sending']);
            const { id } = chat.input(0);
            chat.server.snapshot.mainRequests = [...chat.server.snapshot.mainRequests,
                { ...receipt('mine', id, 'failed', 7), failure: 'unknown' }];
            await chat.queued(0, 'mine', true);
            expect(chat.view().queued).toEqual(['First:queued', 'Second:queued']);
            expect(chat.result.current.failedRequests.map(item => item.requestId)).toEqual(['mine']);
        } finally { chat.unmount(); }
    });

    it('restores a failed activation and its cause from receipts alone after a reload', async () => {
        const chat = await setup({ mainRequests: [
            receipt('waiting', 'waiting-input', 'queued', 2, 'Still waiting'),
            { ...receipt('closed', 'closed-input', 'failed', 1), failure: 'activation_failed',
                errorCode: deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE },
        ] });
        try {
            await waitFor(() => expect(chat.result.current.failedRequests).toHaveLength(1));
            expect(chat.result.current.failedRequests[0]).toMatchObject({
                requestId: 'closed', failure: 'activation_failed',
                errorCode: deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE });
            expect(chat.result.current.waitingRequestIds).toEqual(['waiting']);
        } finally { chat.unmount(); }
    });

    it('opens the failure list for a send shown as queued that is first read back as failed', async () => {
        const earlier = { ...receipt('earlier', 'earlier-input', 'failed', 1), failure: 'activation_failed' };
        const chat = await setup({ active: true, requestId: 'running', mainRequests: [earlier] });
        // The page passes the chat state to the list on every render.
        const props = () => ({ failed: chat.result.current.failedRequests, waitingIds: chat.result.current.waitingRequestIds });
        const list = render(createElement(AssistantFailedRequests, props()));
        const sync = () => list.rerender(createElement(AssistantFailedRequests, props()));
        const details = () => list.container.querySelector('details') as HTMLDetailsElement;
        try {
            await waitFor(() => expect(chat.result.current.failedRequests).toHaveLength(1));
            sync();
            expect(details().open).toBe(false);
            chat.send('Mine');
            const { id } = chat.input(0);
            // A throttled background tab reads the receipts only after activation failed.
            chat.server.holdLists = true;
            chat.server.holdSnapshots = true;
            chat.server.snapshot.mainRequests = [{ ...receipt('mine', id, 'failed', 2), failure: 'activation_failed' }, earlier];
            await chat.queued(0, 'mine', true);
            expect(chat.view().queued).toEqual(['Mine:queued']);
            expect(chat.result.current.waitingRequestIds).toEqual(['mine']);
            sync();
            await chat.release();
            expect(chat.view().queued).toEqual([]);
            expect(chat.result.current.failedRequests.map(item => item.requestId)).toEqual(['mine', 'earlier']);
            sync();
            expect(details().open).toBe(true);
            // Closed by the owner, it stays closed while nothing new fails.
            details().open = false;
            fireEvent(details(), new Event('toggle'));
            await chat.refresh();
            sync();
            expect(details().open).toBe(false);
        } finally { list.unmount(); chat.unmount(); }
    });

    it('keeps the workbench queue path free of conversation and queue entries', async () => {
        const chat = await setup({}, { queueOnly: true });
        try {
            await waitFor(() => expect(chat.fetchMock).toHaveBeenCalled());
            chat.send('Reproduce the issue');
            expect(chat.result.current.messages).toEqual([]);
            expect(chat.result.current.queuedMessages).toEqual([]);
            chat.server.snapshot.mainRequests = [receipt('r1', chat.input(0).id, 'queued', 1, 'Reproduce the issue')];
            await chat.queued(0, 'r1', true);
            expect(chat.result.current.acceptedInput?.question).toBe('Reproduce the issue');
            expect(chat.result.current.messages).toEqual([]);
        } finally { chat.unmount(); }
    });
});
