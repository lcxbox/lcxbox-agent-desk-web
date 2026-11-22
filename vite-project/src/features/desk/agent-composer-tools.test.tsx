import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentComposerTools } from './agent-composer-tools';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('agent composer tools', () => {
    it('orders the meter, details icon and permission history icon and never submits the form', () => {
        const onDetails = vi.fn();
        const onPermissionHistory = vi.fn();
        const onSubmit = vi.fn();
        render(<form onSubmit={onSubmit}><AgentComposerTools meter={<span data-testid="meter" />} onDetails={onDetails} onPermissionHistory={onPermissionHistory} /></form>);
        const tools = screen.getByTestId('agent-composer-tools');
        const details = screen.getByRole('button', { name: 'pages.aiAgent.workspace.details' });
        const history = screen.getByRole('button', { name: 'pages.aiAgent.permissionHistory' });
        expect(Array.from(tools.children)).toEqual([screen.getByTestId('meter'), details, history]);
        for (const button of [details, history]) {
            expect(button.title).toBe(button.getAttribute('aria-label'));
            expect(button.querySelector('.agent-action-label')?.textContent).toBe(button.getAttribute('aria-label'));
            expect(button.querySelector('svg')).not.toBeNull();
            expect(button.getAttribute('aria-haspopup')).toBe('dialog');
            fireEvent.click(button);
        }
        expect(onDetails).toHaveBeenCalledOnce();
        expect(onPermissionHistory).toHaveBeenCalledOnce();
        expect(onSubmit).not.toHaveBeenCalled();
    });
});

it('keeps the background task count in the tooltip and accessible name when the label is hidden', () => {
    render(<AgentComposerTools meter={null} onDetails={() => {}} onPermissionHistory={() => {}}
        onTasks={() => {}} runningTaskCount={4} />);
    const tasks = screen.getByRole('button', { name: 'pages.aiAgent.tasks.title (4)' });
    tasks.querySelector<HTMLElement>('.agent-action-label')!.style.display = 'none';
    expect(tasks).toHaveAccessibleName('pages.aiAgent.tasks.title (4)');
    expect(tasks.title).toBe('pages.aiAgent.tasks.title (4)');
    expect(tasks.querySelector('svg')).not.toBeNull();
});

it('places scheduled tasks immediately after directories and opens them without submitting', () => {
    const onSchedules = vi.fn();
    const onSubmit = vi.fn();
    render(<form onSubmit={onSubmit}><AgentComposerTools meter={null} onDetails={() => {}}
        onPermissionHistory={() => {}} onDirectories={() => {}} onSchedules={onSchedules} /></form>);
    const directories = screen.getByRole('button', { name: 'pages.aiAgent.directories.title' });
    const schedules = screen.getByRole('button', { name: 'pages.aiAgent.schedules.title' });
    expect(directories.nextElementSibling).toBe(schedules);
    fireEvent.click(schedules);
    expect(onSchedules).toHaveBeenCalledOnce();
    expect(onSubmit).not.toHaveBeenCalled();
});
