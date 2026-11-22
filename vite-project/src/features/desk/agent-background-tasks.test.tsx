import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentBackgroundTasks } from './agent-background-tasks';
import { AgentComposerTools } from './agent-composer-tools';
import type { CommandTaskDto } from '@/services/types';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const command: CommandTaskDto = { deviceId: 'original-device', taskId: 'command-1', callId: 'call-1', executionGeneration: 'generation-1',
    state: 'running', updatedAt: '2026-09-08T10:00:00Z', result: null, resultTruncated: false };
const base = { open: true, onOpenChange: vi.fn(), commands: [], providers: [], tools: [],
    connected: true, canCancelProvider: true, cancelling: null, onCancel: vi.fn().mockResolvedValue(undefined) };

describe('agent background tasks', () => {
    it('offers a named task entry even without tasks and never submits a message', () => {
        const onTasks = vi.fn(); const onSubmit = vi.fn();
        render(<form onSubmit={onSubmit}><AgentComposerTools meter={null} onDetails={vi.fn()}
            onPermissionHistory={vi.fn()} onTasks={onTasks} runningTaskCount={2} /></form>);
        const button = screen.getByRole('button', { name: 'pages.aiAgent.tasks.title (2)' });
        expect(button.textContent).toContain('(2)');
        fireEvent.click(button);
        expect(onTasks).toHaveBeenCalledOnce(); expect(onSubmit).not.toHaveBeenCalled();
    });
    it('shows an empty state', () => {
        render(<AgentBackgroundTasks {...base} />);
        expect(screen.getByText('pages.aiAgent.tasks.empty')).toBeTruthy();
    });
    it('keeps completed results and cancels only the selected unfinished command', async () => {
        const onCancel = vi.fn().mockResolvedValue(undefined);
        render(<AgentBackgroundTasks {...base} onCancel={onCancel} commands={[
            { ...command, taskId: 'done', executionGeneration: 'done-generation', state: 'succeeded', result: 'saved result' }, command,
        ]} />);
        expect(screen.getByText('saved result')).toBeTruthy();
        const buttons = screen.getAllByRole('button', { name: 'pages.aiAgent.tasks.cancel' });
        expect(buttons).toHaveLength(1);
        fireEvent.click(buttons[0]);
        await waitFor(() => expect(onCancel).toHaveBeenCalledWith('command', 'command-1'));
        expect(await screen.findByRole('status')).toBeTruthy();
        expect(screen.getByText('pages.aiAgent.backgroundState.running')).toBeTruthy();
    });
    it('does not offer command cancellation while the device is offline', () => {
        render(<AgentBackgroundTasks {...base} commands={[command]} connected={false} />);
        expect(screen.queryByRole('button', { name: 'pages.aiAgent.tasks.cancel' })).toBeNull();
        expect(within(screen.getByRole('dialog')).getByText('pages.aiAgent.tasks.offline')).toBeTruthy();
    });
});

describe('background task devices', () => {
    it('keeps each original device visible and cancels the selected original task', async () => {
        const onCancel = vi.fn().mockResolvedValue(undefined);
        render(<AgentBackgroundTasks open onOpenChange={vi.fn()} commands={[
            {deviceId:'linux-a', taskId:'command-a',callId:'call-a',executionGeneration:'generation-a',state:'running',updatedAt:'2026-11-06T00:00:00Z',result:null,resultTruncated:false},
        ]} providers={[
            {deviceId:'mac-b',taskId:'task-b',callId:'call-b',providerId:'browser',capabilityId:'browser.wait',toolName:'browser_wait_for',effect:'read_external',state:'running',progressSequence:0,supportsCancel:true,updatedAt:'2026-11-06T00:00:00Z',terminalAt:null},
        ]} tools={[]} connected canCancelProvider cancelling={null} onCancel={onCancel}/>);
        expect(screen.getByText('linux-a')).toBeInTheDocument();
        const original = screen.getByText('mac-b').closest('section')!;
        fireEvent.click(within(original).getByRole('button',{name:'pages.aiAgent.tasks.cancel'}));
        await waitFor(() => expect(onCancel).toHaveBeenCalledWith('provider','task-b'));
    });
});
