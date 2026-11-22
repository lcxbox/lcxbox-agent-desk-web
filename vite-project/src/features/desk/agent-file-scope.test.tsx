import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentFileScope, type AgentFileScopeView } from './agent-file-scope';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

vi.mock('@/features/file-manager/remote-directory-picker', () => ({ RemoteDirectoryPicker: ({ deskId, onSelect }: { deskId: string; onSelect: (path: string) => void }) => <button data-desk={deskId} onClick={() => onSelect('/private/tmp/a directory ')}>choose</button> }));

describe('conversation directories', () => {
    it('shows automatic review and its reason without exposing a second manual decision', () => {
        const scope: AgentFileScopeView = { revision: 3, directories: [{
            requestId: 'directory', canonicalPath: '/work', purpose: 'report', state: 'pending',
            source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z',
        }] };
        const props = { scope, open: true, onOpenChange: vi.fn(), disabled: false, onUpdate: vi.fn() };
        const view = render(<AgentFileScope {...props} automaticApproval />);
        expect(screen.getByText('pages.aiAgent.directories.pendingAi')).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'pages.aiAgent.directories.approve' })).toBeNull();
        view.rerender(<AgentFileScope {...props} automaticApproval={false} />);
        expect(screen.getByRole('button', { name: 'pages.aiAgent.directories.approve' })).toBeEnabled();
        scope.directories[0] = { ...scope.directories[0], state: 'rejected', decisionActor: 'system', decisionReason: 'Reviewer timed out' };
        view.rerender(<AgentFileScope {...props} automaticApproval />);
        expect(screen.getByText('pages.aiAgent.directories.actor.system')).toBeInTheDocument();
        expect(screen.getByText('Reviewer timed out')).toBeInTheDocument();
    });
    it('offers the owner the decision on the paused request the AI reviewer handed over', () => {
        const directory = { requestId: 'paused', canonicalPath: '/work', purpose: 'report', state: 'pending',
            source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z' };
        const scope: AgentFileScopeView = { revision: 4, directories: [directory, { ...directory, requestId: 'other', canonicalPath: '/other' }] };
        const update = vi.fn((..._args: unknown[]) => true);
        const pause = { kind: 'directory' as const, requestId: 'paused', toolCallId: 'call', awaitingOwner: true };
        const props = { scope, open: true, onOpenChange: vi.fn(), disabled: false, onUpdate: update, automaticApproval: true,
            showPendingActions: false, onPendingJump: vi.fn() };
        const view = render(<AgentFileScope {...props} decisionPause={pause} />);
        expect(screen.getByText('pages.aiAgent.decision.review')).toBeInTheDocument();
        expect(screen.getAllByText('pages.aiAgent.directories.pendingAi')).toHaveLength(1);
        expect(screen.getAllByRole('button', { name: 'pages.aiAgent.directories.approve' })).toHaveLength(1);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.directories.reject' }));
        expect(update).toHaveBeenCalledWith({ kind: 'decide_directory', directory_request_id: 'paused', approve: false, expected_revision: 4 },
            'pages.aiAgent.directories.timeout', undefined);
        view.rerender(<AgentFileScope {...props} decisionPause={{ ...pause, awaitingOwner: false }} />);
        expect(screen.queryByText('pages.aiAgent.decision.review')).toBeNull();
        expect(screen.getAllByText('pages.aiAgent.directories.pendingAi')).toHaveLength(2);
        expect(screen.queryByRole('button', { name: 'pages.aiAgent.directories.approve' })).toBeNull();
    });
    it.each(['target', null])('adds a directory for selected target %s with the observed revision', (target) => {
        const update = vi.fn((..._args: unknown[]) => true);
        render(<AgentFileScope deskId="device" sessionTargetId={target} scope={{ revision: 7, directories: [] }} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        expect(screen.queryByRole('textbox')).toBeNull();
        const button = screen.getByRole('button', { name: 'pages.aiAgent.directories.add' });
        expect(button.getAttribute('type')).toBe('button');
        fireEvent.click(button);
        fireEvent.click(screen.getByText("choose"));
        expect(update).toHaveBeenCalledWith({ kind: 'select_directory', path: '/private/tmp/a directory ', purpose: 'pages.aiAgent.directories.manualPurpose', expected_revision: 7 }, 'pages.aiAgent.directories.timeout', 'device');
    });

    it('browses a listed device through its live connection and keeps the device as the directory owner', () => {
        const update = vi.fn((..._args: unknown[]) => true);
        const props = { scope: { revision: 2, directories: [] }, open: true, onOpenChange: vi.fn(), disabled: false, onUpdate: update,
            devices: [{ id: '42', name: 'UBUNTU-VM' }] };
        const view = render(<AgentFileScope {...props} connectionFor={() => undefined} />);
        const add = screen.getByRole('button', { name: 'pages.aiAgent.directories.add' });
        expect(add).toBeDisabled();
        view.rerender(<AgentFileScope {...props} connectionFor={id => id === '42' ? 'connection-42' : undefined} />);
        fireEvent.click(add);
        const picker = screen.getByText('choose');
        expect(picker.getAttribute('data-desk')).toBe('connection-42');
        fireEvent.click(picker);
        expect(update).toHaveBeenCalledWith({ kind: 'select_directory', path: '/private/tmp/a directory ', purpose: 'pages.aiAgent.directories.manualPurpose', expected_revision: 2 }, 'pages.aiAgent.directories.timeout', '42');
    });

    it('blocks unresolved targets and closes the picker when target readiness is lost', () => {
        const props = { deskId: 'device', scope: { revision: 0, directories: [] }, open: true, onOpenChange: vi.fn(), disabled: false, onUpdate: vi.fn() };
        const view = render(<AgentFileScope {...props} />);
        const add = screen.getByRole('button', { name: 'pages.aiAgent.directories.add' });
        expect(add).toBeDisabled();
        view.rerender(<AgentFileScope {...props} sessionTargetId={null} />);
        expect(add).toBeEnabled();
        fireEvent.click(add);
        expect(screen.getByText('choose')).toBeInTheDocument();
        view.rerender(<AgentFileScope {...props} />);
        expect(add).toBeDisabled();
        expect(screen.queryByText('choose')).toBeNull();
    });

    it('binds approval and revocation to exact directory ids and current revision', () => {
        const update = vi.fn((..._args: unknown[]) => true);
        const scope: AgentFileScopeView = { revision: 9, directories: [
            { requestId: 'pending', canonicalPath: '/resolved/path', purpose: 'purpose', state: 'pending', source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z' },
            { requestId: 'approved', canonicalPath: '/other/path', purpose: 'purpose', state: 'approved', source: 'owner_selection', referenceExpiresAt: '2030-01-01T00:00:00Z' },
        ] };
        render(<AgentFileScope scope={scope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.directories.approve' }));
        expect(update.mock.calls[0][0]).toEqual({ kind: 'decide_directory', directory_request_id: 'pending', approve: true, expected_revision: 9 });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.directories.remove' }));
        expect(update.mock.calls[1][0]).toEqual({ kind: 'revoke_directory', directory_request_id: 'approved', expected_revision: 9 });
    });

    it('says when a directory action cannot be sent instead of ignoring the click', () => {
        const scope: AgentFileScopeView = { revision: 3, directories: [
            { requestId: 'approved', canonicalPath: '/other/path', purpose: 'purpose', state: 'approved', source: 'owner_selection', referenceExpiresAt: '' },
        ] };
        const update = vi.fn((..._args: unknown[]) => false);
        const view = render(<AgentFileScope scope={scope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        expect(screen.queryByRole('alert')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.directories.remove' }));
        expect(screen.getByRole('alert')).toHaveTextContent('pages.aiAgent.directories.submitUnavailable');
        update.mockReturnValue(true);
        view.rerender(<AgentFileScope scope={scope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.directories.remove' }));
        expect(screen.queryByRole('alert')).toBeNull();
    });

    it('does not show directory paths while the panel is closed', () => {
        render(<AgentFileScope scope={{ revision: 0, directories: [] }} open={false} onOpenChange={() => {}} disabled={false} onUpdate={() => true} />);
        expect(screen.queryByRole('dialog')).toBeNull();
    });
});
