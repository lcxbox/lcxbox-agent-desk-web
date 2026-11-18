import { fireEvent, render, screen } from '@testing-library/react';
import '@testing-library/jest-dom';
import { expect, it, vi } from 'vitest';
import { AssistantRequestDevices } from './assistant-request-devices';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const connections = vi.hoisted(() => ({ data: [] as unknown[] }));
vi.mock('@/services/hooks/connectionController/useListConnections', () => ({
    useListConnections: () => ({ isSuccess: true, data: { data: connections.data } }),
}));

const host = (deviceId: string, name: string) => ({
    device_id: deviceId,
    connection_id: `${deviceId}-live`,
    peer: { display_name: name, operation_system: 'Linux' },
    ai_assistant_settings: { enabled: true },
});

it('tells apart devices that share a name, and only those', () => {
    connections.data = [
        host('299df942-f666-40d5-88a3-6757161e26e8', 'UBUNTU-VM'),
        host('6eec0ef7-d828-4f7b-a34f-45913eb6d635', 'UBUNTU-VM'),
        host('aaaa1111-0000-0000-0000-000000000000', 'Office Mac'),
    ];
    render(<AssistantRequestDevices selected={[]} onChange={() => {}} />);
    fireEvent.pointerDown(screen.getByRole('button'), { button: 0, ctrlKey: false });

    const items = screen.getAllByRole('menuitemcheckbox').map(item => item.textContent);
    expect(items).toEqual([
        'UBUNTU-VM299df942Linux',
        'UBUNTU-VM6eec0ef7Linux',
        'Office MacLinux',
    ]);
});
