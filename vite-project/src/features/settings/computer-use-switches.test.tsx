import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ComputerUseSwitchesSettings } from './computer-use-switches';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ load: vi.fn(), save: vi.fn() }));
vi.mock('@/services/clients', () => ({
    queryComputerUseSwitches: api.load,
    updateComputerUseSwitches: api.save,
}));
const switches = {
    revision: 3, platform: 'windows', enabled: true, observe: false, office_semantic: false,
    iwork_semantic: false, generic_semantic_ui: false, raw_input_fallback: false,
};
beforeEach(() => {
    vi.clearAllMocks();
    api.load.mockResolvedValue({ data: switches });
    api.save.mockImplementation(async (input) => ({ data: { ...switches, ...input, revision: 4 } }));
});
async function open() {
    render(<ComputerUseSwitchesSettings />);
    expect(api.load).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Load current settings' }));
    await screen.findByRole('checkbox', { name: 'Allow Computer Use on this device' });
}
describe('local Computer Use switches', () => {
    it('saves the edited switches with the observed revision', async () => {
        await open();
        fireEvent.click(screen.getByRole('checkbox', { name: 'Allow observing the screen and application interfaces' }));
        fireEvent.click(screen.getByRole('checkbox', { name: 'Allow generic interface actions' }));
        expect(api.save).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));
        await waitFor(() => expect(api.save).toHaveBeenCalledWith({ expected_revision: 3, enabled: true,
            observe: true, office_semantic: false, iwork_semantic: false, generic_semantic_ui: true,
            raw_input_fallback: false }));
    });
    it('shows only the switches that take effect on this platform', async () => {
        await open();
        expect(screen.getByRole('checkbox', { name: /Office documents/ })).toBeInTheDocument();
        expect(screen.queryByRole('checkbox', { name: /iWork documents/ })).not.toBeInTheDocument();
    });
    it('disables the capabilities while the master switch is off', async () => {
        await open();
        fireEvent.click(screen.getByRole('checkbox', { name: 'Allow Computer Use on this device' }));
        expect(screen.getByRole('checkbox', { name: 'Allow generic interface actions' })).toBeDisabled();
        expect(screen.getByText('These capabilities have no effect while the master switch is off.')).toBeInTheDocument();
    });
    it('requires rereading after an uncertain save without retrying', async () => {
        api.save.mockRejectedValueOnce(new Error('worker unavailable'));
        await open();
        fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));
        await screen.findByRole('status');
        expect(screen.queryByRole('checkbox')).not.toBeInTheDocument();
        expect(api.save).toHaveBeenCalledTimes(1);
    });
});
