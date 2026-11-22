import type { ReactNode } from 'react';
import { fireEvent, render, renderHook, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import '@testing-library/jest-dom';
import { beforeEach, expect, it, vi } from 'vitest';
import { RestResponseError } from '@/lib/kubb-client';
import { AgentRequestDevices, useConversationRequestDevices } from './agent-request-devices';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const connections = vi.hoisted(() => ({ data: [] as unknown[] }));
vi.mock('@/services/hooks/connectionController/useListConnections', () => ({
    useListConnections: () => ({ isSuccess: true, data: { data: connections.data } }),
}));

const roster = vi.hoisted(() => ({ get: vi.fn() }));
vi.mock('@/services/clients', () => ({
    getAiAgentDeviceRoster: roster.get,
    updateAiAgentDeviceRoster: vi.fn(),
}));

beforeEach(() => {
    roster.get.mockReset();
});

// The app's client turns retries off globally; the roster query sets its own.
const renderRoster = () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false, retryDelay: 1 } } });
    const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
    return renderHook(() => useConversationRequestDevices('personal', 'conv-1', () => 'conv-1'), { wrapper });
};

it('retries a dropped roster request so the composer becomes ready without a reload', async () => {
    roster.get
        .mockRejectedValueOnce(new Error('Network Error'))
        .mockResolvedValueOnce({ success: true, data: { revision: 2, targetRefs: [] } });
    const { result } = renderRoster();

    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.error).toBeNull();
    expect(roster.get).toHaveBeenCalledTimes(2);
});

it('does not retry a business failure of the roster request', async () => {
    roster.get.mockRejectedValue(new RestResponseError('denied', 1, null));
    const { result } = renderRoster();

    await waitFor(() => expect(result.current.error).toBe('pages.aiAgent.conversations.loadFailed'));
    expect(result.current.ready).toBe(false);
    expect(roster.get).toHaveBeenCalledTimes(1);
});

const host = (deviceId: string, name: string) => ({
    device_id: deviceId,
    connection_id: `${deviceId}-live`,
    peer: { display_name: name, operation_system: 'Linux' },
    ai_agent_settings: { enabled: true },
});

it('tells apart devices that share a name, and only those', () => {
    connections.data = [
        host('299df942-f666-40d5-88a3-6757161e26e8', 'UBUNTU-VM'),
        host('6eec0ef7-d828-4f7b-a34f-45913eb6d635', 'UBUNTU-VM'),
        host('aaaa1111-0000-0000-0000-000000000000', 'Office Mac'),
    ];
    render(<AgentRequestDevices selected={[]} onChange={() => {}} />);
    fireEvent.pointerDown(screen.getByRole('button'), { button: 0, ctrlKey: false });

    const items = screen.getAllByRole('menuitemcheckbox').map(item => item.textContent);
    expect(items).toEqual([
        'UBUNTU-VM299df942Linux',
        'UBUNTU-VM6eec0ef7Linux',
        'Office MacLinux',
    ]);
});
