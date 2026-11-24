import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { LocalPasswordRecovery } from './local-password-recovery';
import { finishLocalRecovery, startLocalRecovery } from '@/services/clients';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('@/services/clients', () => ({
    startLocalRecovery: vi.fn(), finishLocalRecovery: vi.fn(), cancelLocalRecovery: vi.fn(),
}));

it('shows the selected configuration and checks confirmation before submitting', async () => {
    const done = vi.fn();
    vi.mocked(startLocalRecovery).mockResolvedValue({ success: true, code: 0, data: { config_file: '/host/profile.toml' } });
    render(<LocalPasswordRecovery onSuccess={done} />);
    fireEvent.click(screen.getByRole('button', { name: /forgot password/i }));
    expect(await screen.findByText(/\/host\/profile.toml/)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText('Recovery code'), {target:{value:'01234567'}});
    fireEvent.change(screen.getByLabelText('New password'), {target:{value:'updated'}});
    fireEvent.change(screen.getByLabelText('Confirm new password'), {target:{value:'different'}});
    fireEvent.click(screen.getByRole('button', {name:'Reset password'}));
    expect(screen.getByRole('alert')).toHaveTextContent('do not match');
    expect(finishLocalRecovery).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('Confirm new password'), {target:{value:'updated'}});
    fireEvent.click(screen.getByRole('button', {name:'Reset password'}));
    await waitFor(() => expect(done).toHaveBeenCalledOnce());
    expect(finishLocalRecovery).toHaveBeenCalledWith({code:'01234567',new_password:'updated'});
});
