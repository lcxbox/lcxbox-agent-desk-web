import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import type { PermissionRequestDto } from '@/services/types';
import { AgentPermissionRecords, type PermissionGroupDecisions } from './agent-permission-records';
import { AgentComposerTools } from './agent-composer-tools';

vi.mock('react-i18next', () => ({ useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) => options ? `${key} ${JSON.stringify(options)}` : key,
}) }));
const request = (requestId: string, state: PermissionRequestDto['state']) => ({ requestId, state, items: [] } as unknown as PermissionRequestDto);
const row = (value: PermissionRequestDto) => <div key={value.requestId} data-request>{value.requestId}</div>;
function Records({ requests, automaticApproval = false }: { requests: PermissionRequestDto[]; automaticApproval?: boolean }) {
    const [open, setOpen] = useState(false);
    return <>
        <AgentPermissionRecords requests={requests} automaticApproval={automaticApproval} open={open} onOpenChange={setOpen}>{row}</AgentPermissionRecords>
        <AgentComposerTools meter={<span>meter</span>} onDetails={() => {}} onPermissionHistory={() => setOpen(true)} />
    </>;
}

describe('permission request history', () => {
    it('hides resolved rows regardless of history length and opens them on demand', () => {
        const requests = Array.from({ length: 100 }, (_, i) => request(`resolved-${i}`, 'approved'));
        render(<Records requests={requests} />);
        expect(screen.queryByText('resolved-0')).toBeNull();
        expect(screen.queryByTestId('ai-agent-permission-requests')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.permissionHistory' }));
        expect(screen.getByText('resolved-99')).toBeTruthy();
    });

    it('keeps pending requests visible and moves resolved ones without opening history', () => {
        const { rerender } = render(<Records requests={[request('pending-one', 'pending'), request('review-one', 'needs_revalidation')]} />);
        expect(screen.getByText('pending-one')).toBeTruthy();
        expect(screen.getByText('review-one')).toBeTruthy();
        rerender(<Records requests={[request('pending-one', 'partially_approved')]} />);
        expect(screen.queryByText('pending-one')).toBeNull();
        expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('replaces manual cards with review status and restores them when automatic approval is disabled', () => {
        const requests = [request('automatic', 'pending')];
        const { rerender } = render(<Records requests={requests} automaticApproval />);
        expect(screen.getByRole('status')).toHaveTextContent('pages.aiAgent.permissionAutomaticReview');
        expect(screen.queryByText('automatic')).toBeNull();
        expect(screen.queryByTestId('ai-agent-permission-requests')).toBeNull();
        rerender(<Records requests={requests} />);
        expect(screen.queryByRole('status')).toBeNull();
        expect(screen.getByText('automatic')).toBeTruthy();
        rerender(<Records requests={[request('automatic', 'approved')]} automaticApproval />);
        expect(screen.queryByRole('status')).toBeNull();
        expect(screen.queryByText('automatic')).toBeNull();
    });

    it('keeps interactive commands and requests needing revalidation available for owner review', () => {
        const interactive = { ...request('interactive', 'pending'), items: [{ commandConfirmation: { interactive: {} } }] } as PermissionRequestDto;
        render(<Records requests={[request('automatic', 'pending'), interactive, request('revalidate', 'needs_revalidation')]} automaticApproval />);
        expect(screen.queryByText('automatic')).toBeNull();
        expect(screen.getByText('interactive')).toBeTruthy();
        expect(screen.getByText('revalidate')).toBeTruthy();
    });

    it('keeps the latest reviewer fault visible and opens its durable decision without implying execution', () => {
        const failed = { ...request('failed-review', 'denied'), decision: { source: 'system', decidedAt: '2026-10-13T00:00:00Z', items: [{ itemId: 'failed', approved: false, source: 'system', outcome: 'fault' }] } } as PermissionRequestDto;
        const { rerender } = render(<Records requests={[failed]} automaticApproval />);
        expect(screen.getByRole('alert')).toHaveTextContent('pages.aiAgent.permissionReviewFaultSummary');
        expect(screen.queryByText('failed-review')).toBeNull();
        fireEvent.click(screen.getAllByRole('button', { name: 'pages.aiAgent.permissionHistory' })[0]);
        expect(screen.getByText('failed-review')).toBeTruthy();
        rerender(<Records key="after" requests={[failed, request('new-review', 'pending')]} automaticApproval />);
        expect(screen.queryByRole('alert')).toBeNull();
        expect(screen.getByRole('status')).toBeTruthy();
    });

    it('does not present a reasoned AI denial or an owner denial as a reviewer fault', () => {
        const denied = { ...request('valid-denial', 'denied'), decision: { source: 'ai_approval', items: [] } } as PermissionRequestDto;
        render(<Records requests={[denied]} automaticApproval />);
        expect(screen.queryByRole('alert')).toBeNull();
    });

    it('closes records when remounted for a different conversation', () => {
        const { rerender } = render(<Records key="a" requests={[request('old', 'denied')]} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.permissionHistory' }));
        rerender(<Records key="b" requests={[]} />);
        expect(screen.queryByRole('dialog')).toBeNull();
        expect(screen.queryByText('old')).toBeNull();
    });
});

const member = (requestId: string, state: PermissionRequestDto['state'], order: number, interactive = false) => ({
    ...request(requestId, state), group: { groupId: 'step', order, size: 3 },
    items: [interactive
        ? { itemId: 'pty', toolName: 'exec_command', resourceScope: [], operationScope: [], exportDestinations: [], commandConfirmation: { interactive: {} } }
        : { itemId: `${requestId}-item`, toolName: 'inspect_desktop_session', expectedEffect: 'read_device', resourceScope: [], operationScope: [],
            exportDestinations: [], suggestedTtlSeconds: 60, suggestedMaxUses: 1 }],
} as unknown as PermissionRequestDto);
function GroupRecords({ requests, decisions, automaticApproval = false }: {
    requests: PermissionRequestDto[]; decisions?: PermissionGroupDecisions; automaticApproval?: boolean;
}) {
    return <AgentPermissionRecords requests={requests} automaticApproval={automaticApproval} open={false} onOpenChange={() => {}} decisions={decisions}>
        {row}
    </AgentPermissionRecords>;
}
const decisions = (onDecide: PermissionGroupDecisions['onDecide']): PermissionGroupDecisions => ({ canDecide: () => true, disabled: false, onDecide });

describe('permission card groups', () => {
    it('renders the cards of one step together in order with progress and the validity note', () => {
        render(<GroupRecords requests={[member('c', 'pending', 2), member('a', 'approved', 0), member('b', 'pending', 1), request('lone', 'pending')]}
            decisions={decisions(vi.fn())} />);
        const group = screen.getByTestId('ai-agent-permission-group');
        expect(group).toHaveTextContent('pages.aiAgent.permissionGroup.title {"count":3}');
        expect(group).toHaveTextContent('pages.aiAgent.permissionGroup.progress {"decided":1,"total":3}');
        expect(group).toHaveTextContent('pages.aiAgent.permissionGroup.validity');
        const cards = [...group.querySelectorAll('[data-request]')].map(node => node.textContent);
        expect(cards).toEqual(['a', 'b', 'c']);
        const steps = screen.getAllByText(/pages\.aiAgent\.permissionGroup\.step/).map(node => node.textContent);
        expect(steps).toEqual([1, 2, 3].map(index => `pages.aiAgent.permissionGroup.step {"index":${index},"total":3}`));
        expect(screen.getByRole('status')).toHaveTextContent('pages.aiAgent.permissionGroup.waiting');
        expect(screen.getByText('lone').closest('[data-testid="ai-agent-permission-group"]')).toBeNull();
        expect(screen.getByRole('button', { name: 'pages.aiAgent.permissionGroup.approveAll' })).toBeEnabled();
    });

    it('moves a group to history only once every card is decided', () => {
        const { rerender } = render(<GroupRecords requests={[member('a', 'approved', 0), member('b', 'pending', 1)]} />);
        expect(screen.getByText('a')).toBeTruthy();
        rerender(<GroupRecords requests={[member('a', 'approved', 0), member('b', 'denied', 1)]} />);
        expect(screen.queryByText('a')).toBeNull();
        expect(screen.queryByTestId('ai-agent-permission-requests')).toBeNull();
    });

    it('renders a lone request exactly as before, without group controls', () => {
        render(<GroupRecords requests={[request('lone', 'pending')]} decisions={decisions(vi.fn())} />);
        expect(screen.getByText('lone')).toBeTruthy();
        expect(screen.queryByTestId('ai-agent-permission-group')).toBeNull();
        expect(screen.queryByRole('button', { name: 'pages.aiAgent.permissionGroup.approveAll' })).toBeNull();
    });

    it('approves all in order and leaves interactive and AI-reviewed cards to their own decision', async () => {
        const onDecide = vi.fn().mockResolvedValue(true);
        const requests = [member('pty', 'pending', 2, true), member('first', 'pending', 0), member('second', 'pending', 1)];
        const { rerender } = render(<GroupRecords requests={requests} decisions={decisions(onDecide)} />);
        expect(screen.getByTestId('ai-agent-permission-group')).toHaveTextContent('pages.aiAgent.permissionGroup.individual {"count":1}');
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.permissionGroup.approveAll' }));
        await waitFor(() => expect(onDecide).toHaveBeenCalledTimes(2));
        expect(onDecide.mock.calls.map(call => call[0].requestId)).toEqual(['first', 'second']);
        expect(onDecide.mock.calls[0][1]).toEqual([expect.objectContaining({ itemId: 'first-item', decision: 'approve' })]);
        onDecide.mockClear();
        rerender(<GroupRecords requests={requests} decisions={decisions(onDecide)} automaticApproval />);
        expect(screen.queryByText('first')).toBeNull();
        expect(screen.getByText('pty')).toBeTruthy();
        expect(screen.getAllByText('pages.aiAgent.permissionAutomaticReview')).toHaveLength(3);
        expect(screen.getByRole('button', { name: 'pages.aiAgent.permissionGroup.approveAll' })).toBeDisabled();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.permissionGroup.denyAll' }));
        await waitFor(() => expect(onDecide).toHaveBeenCalledTimes(1));
        expect(onDecide).toHaveBeenCalledWith(expect.objectContaining({ requestId: 'pty' }), [{ itemId: 'pty', decision: 'deny' }]);
    });

    it('stops at a failed decision and names the card that failed', async () => {
        const onDecide = vi.fn().mockResolvedValueOnce(true).mockResolvedValueOnce(false);
        const requests = [member('first', 'pending', 0), member('second', 'pending', 1), member('third', 'pending', 2)];
        render(<GroupRecords requests={requests} decisions={decisions(onDecide)} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.permissionGroup.denyAll' }));
        const alert = await screen.findByRole('alert');
        expect(alert).toHaveTextContent('pages.aiAgent.permissionGroup.stopped');
        expect(alert).toHaveTextContent('index\\":2');
        expect(onDecide).toHaveBeenCalledTimes(2);
    });
});
