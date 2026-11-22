import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentComposerActions } from './agent-composer-actions';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const defaults = { turnRunning: false, canStop: false, stopping: false, sendDisabled: false,
    onStop: vi.fn() };
const stop = 'pages.aiAgent.stop';
const send = 'pages.aiAgent.send';

describe('agent composer actions', () => {
    it('keeps a visible disabled stop while the first running turn has no durable snapshot', () => {
        const onStop = vi.fn();
        const view = render(<AgentComposerActions {...defaults} turnRunning onStop={onStop} />);
        const button = view.getByRole('button', { name: stop });
        expect(button).toBeDisabled();
        expect(view.queryByRole('button', { name: send })).toBeNull();
        fireEvent.click(button);
        expect(onStop).not.toHaveBeenCalled();
        view.rerender(<AgentComposerActions {...defaults} turnRunning canStop onStop={onStop} />);
        expect(view.getByRole('button', { name: stop })).toBeEnabled();
        fireEvent.click(view.getByRole('button', { name: stop }));
        expect(onStop).toHaveBeenCalledOnce();
    });

    it('keeps the stop pending and prevents input submission until stop settles', () => {
        const view = render(<AgentComposerActions {...defaults} canStop stopping />);
        expect(view.getByRole('button', { name: 'pages.aiAgent.stopping' })).toBeDisabled();
        expect(view.queryByRole('button', { name: send })).toBeNull();
        view.rerender(<AgentComposerActions {...defaults} />);
        expect(view.queryByRole('button', { name: stop })).toBeNull();
        expect(view.getByRole('button', { name: send })).toHaveAttribute('type', 'submit');
    });

    it('allows stopping unfinished children while leaving the idle main composer available', () => {
        const onStop = vi.fn(); const onSubmit = vi.fn();
        const view = render(<form onSubmit={onSubmit}>
            <AgentComposerActions {...defaults} canStop onStop={onStop} />
        </form>);
        fireEvent.click(view.getByRole('button', { name: stop }));
        expect(onStop).toHaveBeenCalledOnce();
        expect(onSubmit).not.toHaveBeenCalled();
        expect(view.getByRole('button', { name: send })).toBeEnabled();
    });

    it('keeps both stop and send available when the current turn accepts queued input', () => {
        const view = render(<AgentComposerActions {...defaults} turnRunning canStop allowQueue />);
        expect(view.getByRole('button', { name: stop })).toBeEnabled();
        expect(view.getByRole('button', { name: send })).toBeEnabled();
        view.rerender(<AgentComposerActions {...defaults} turnRunning canStop allowQueue sendDisabled />);
        expect(view.getByRole('button', { name: send })).toBeDisabled();
        expect(view.getByRole('button', { name: stop })).toBeEnabled();
    });

    it('retains input eligibility controls', () => {
        const view = render(<AgentComposerActions {...defaults} sendDisabled />);
        expect(view.getByRole('button', { name: send })).toBeDisabled();
    });
});
