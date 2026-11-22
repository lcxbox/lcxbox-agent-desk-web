import { render, screen, waitFor } from '@testing-library/react';
import { readFileSync } from 'node:fs';
import { expect, it, vi } from 'vitest';
import { AgentSchedules } from './agent-schedules';
const request = vi.hoisted(() => vi.fn());
const transport = vi.hoisted(() => ({ isConnected: true, subscribe: vi.fn(() => () => {}), sendTracked: vi.fn(), cancelQueued: vi.fn() }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock('./use-desk-signaling', () => ({ useDeskSignaling: () => transport }));
vi.mock('@/features/schedules/client', () => ({ ScheduleClient: class { request = request; receive = vi.fn(); close = vi.fn(); } }));
it('loads all current conversation timer pages including completed tasks only while open', async () => {
    request.mockResolvedValueOnce({ result: 'search_results', tasks: [{ schedule_id: 'one', title: 'Waiting timer', status: 'active' }], next_cursor: 'next' })
        .mockResolvedValueOnce({ result: 'search_results', tasks: [{ schedule_id: 'two', title: 'Completed timer', status: 'completed' }] });
    const { rerender } = render(<AgentSchedules open={false} onOpenChange={() => {}} deviceId="device" sessionId="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" />);
    expect(request).not.toHaveBeenCalled();
    rerender(<AgentSchedules open onOpenChange={() => {}} deviceId="device" sessionId="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" />);
    await screen.findByText('Completed timer');
    expect(screen.getByText('Waiting timer')).toBeTruthy();
    expect(request).toHaveBeenCalledWith(expect.objectContaining({ kind: 'conversation_resume', target_device_id: 'device', source_conversation_id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', after: 'next' }));
});
it('shows read failures instead of an empty success list', async () => {
    request.mockRejectedValue(new Error('offline'));
    render(<AgentSchedules open onOpenChange={() => {}} deviceId="device" sessionId="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" />);
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('pages.aiAgent.schedules.loadError'));
    expect(screen.queryByText('pages.aiAgent.schedules.empty')).toBeNull();
});

it('never falls back to device-wide listing for a new conversation', () => {
    request.mockClear();
    render(<AgentSchedules open onOpenChange={() => {}} deviceId="device" sessionId={null} />);
    expect(request).not.toHaveBeenCalled();
    expect(screen.getByText('pages.aiAgent.schedules.empty')).toBeTruthy();
});

it('discards a previous conversation response after switching conversations', async () => {
    let finish!: (value: unknown) => void;
    request.mockReset().mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }))
        .mockResolvedValue({ result: 'search_results', tasks: [{ schedule_id: 'new', title: 'New conversation timer', status: 'active' }] });
    const { rerender } = render(<AgentSchedules open onOpenChange={() => {}} deviceId="device" sessionId="old" />);
    rerender(<AgentSchedules open onOpenChange={() => {}} deviceId="device" sessionId="new" />);
    await screen.findByText('New conversation timer');
    finish({ result: 'search_results', tasks: [{ schedule_id: 'old', title: 'Old conversation timer', status: 'active' }] });
    await waitFor(() => expect(screen.queryByText('Old conversation timer')).toBeNull());
    expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ source_conversation_id: 'new' }));
});


it('binds schedule filtering to the canonical snapshot session instead of the client UUID', () => {
    const page = readFileSync('src/features/desk/ai-agent-page.tsx', 'utf8');
    expect(page).toContain('<AgentSchedules key={permissionHistoryKey} sessionId={chat.sessionId ?? null}');
    expect(page).not.toContain('<AgentSchedules key={permissionHistoryKey} conversationId={chat.conversationId}');
});

it('lists every timer of a selectable conversation independently from current device selection', async () => {
    request.mockReset().mockResolvedValue({ result: 'search_results', tasks: [{schedule_id: 'frozen-a', title: 'Original device timer', status: 'active'}] });
    render(<AgentSchedules open onOpenChange={() => {}} sessionId="root-session" />);
    await screen.findByText('Original device timer');
    expect(request).toHaveBeenCalledWith(expect.objectContaining({source_conversation_id: 'root-session', target_device_id: null}));
});
