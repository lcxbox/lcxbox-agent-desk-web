import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ComputerUseCommunicationPolicySettings } from './computer-use-communication-policy';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ load: vi.fn(), save: vi.fn() }));
vi.mock('@/services/clients', () => ({
    queryComputerUseCommunicationPolicy: api.load,
    updateComputerUseCommunicationPolicy: api.save,
}));
const policy = { revision: 7, enabled: false, browser_semantic: false, communication_handoff: false, communication_send: false };
beforeEach(() => {
    vi.clearAllMocks();
    api.load.mockResolvedValue({ data: policy });
    api.save.mockImplementation(async (input) => ({ data: { ...input, revision: 8 } }));
});
async function openPolicy() {
    render(<ComputerUseCommunicationPolicySettings />);
    expect(api.load).not.toHaveBeenCalled();
    expect(api.save).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Load current settings' }));
    await screen.findByRole('checkbox', { name: 'Allow separately authorized message sending' });
}
describe('local message policy', () => {
    it('requires explicit load and save and does not silently enable prerequisites', async () => {
        await openPolicy();
        fireEvent.click(screen.getByRole('checkbox', { name: 'Allow separately authorized message sending' }));
        expect(api.save).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 7,
            browser_semantic: false, communication_handoff: false, communication_send: true }));
        await screen.findByRole('status');
        // The master switch is edited on the Computer Use card; here it only explains why
        // these switches have no effect.
        expect(screen.queryByRole('checkbox', { name: /master switch/ })).not.toBeInTheDocument();
        expect(screen.getByText(/The Computer Use master switch is off/)).toBeInTheDocument();
    });
    it('saves explicit prerequisite switches with the observed revision', async () => {
        await openPolicy();
        for (const checkbox of screen.getAllByRole('checkbox')) fireEvent.click(checkbox);
        fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 7,
            browser_semantic: true, communication_handoff: true, communication_send: true }));
    });
    it('requires rereading after an uncertain save without retrying', async () => {
        api.save.mockRejectedValueOnce(new Error('worker unavailable'));
        await openPolicy();
        fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));
        await screen.findByRole('status');
        expect(screen.queryByRole('checkbox')).not.toBeInTheDocument();
        expect(api.save).toHaveBeenCalledTimes(1);
        expect(api.load).toHaveBeenCalledTimes(1);
    });
    it('does not render editable defaults when the read fails', async () => {
        api.load.mockRejectedValueOnce(new Error('unauthorized'));
        render(<ComputerUseCommunicationPolicySettings />);
        fireEvent.click(screen.getByRole('button', { name: 'Load current settings' }));
        await screen.findByRole('status');
        expect(screen.queryByRole('checkbox')).not.toBeInTheDocument();
        expect(api.save).not.toHaveBeenCalled();
    });
});
