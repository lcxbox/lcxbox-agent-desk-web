import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentToolCall } from './agent-tool-call';
import type { AiAgentToolActivity } from './use-ai-agent-chat';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const tool: AiAgentToolActivity = {
    callId: 'call-1', name: 'inspect_desktop_ui', status: 'running',
    argumentsJson: '{"queries":["Calendar"]}', output: null,
};

describe('tool call transcript', () => {
    it('labels a permission-paused sibling as historically skipped, not an active denial', () => {
        render(<AgentToolCall running={false} tool={{ ...tool,
            name: 'describe_tools', status: 'failed',
            output: 'not executed: waiting for user permission decision',
        }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.skippedForPermission' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.failure' })).toBeNull();
        fireEvent.click(screen.getByText(/describe_tools/));
        expect(screen.getByText('pages.aiAgent.historicalPermissionSkip')).toBeTruthy();
    });

    it('labels a sibling skipped for an existing pending request the same way', () => {
        render(<AgentToolCall running={false} tool={{ ...tool,
            name: 'describe_tools', status: 'failed',
            output: 'not executed: waiting for the existing user permission decision',
        }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.skippedForPermission' })).toBeTruthy();
    });

    it.each(['ai_approval', 'owner'] as const)('shows the saved %s decision after the input', source => {
        const review = { ...tool, name: 'exec_command', status: 'running' as const, output: null };
        const { rerender } = render(<AgentToolCall running={false} tool={{
            ...review, permissionReview: { state: 'pending' },
        }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.permissionState.pending' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.success' })).toBeNull();
        fireEvent.click(screen.getByText(/exec_command/));
        const input = screen.getByLabelText('pages.aiAgent.toolCall.input');
        rerender(<AgentToolCall running={false} tool={{ ...review, permissionReview: { state: 'approved', source } }} />);
        const decision = screen.getByText(`pages.aiAgent.permissionDecisionSource.${source}`);
        expect(input.compareDocumentPosition(decision) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });

    it('shows an associated unknown file result while folded without calling it success or ordinary failure', () => {
        const output = { work_id: 'work', action_request_id: tool.callId, execution_generation: 'generation',
            result: 'outcome_unknown', facts: [{ index: 0, changed: true, verified: false }], output: null };
        const { container, rerender } = render(<AgentToolCall running={false} tool={{ ...tool,
            name: 'create_text_file', status: 'failed', output: JSON.stringify(output) }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.outcomeUnknown' })).toBeTruthy();
        expect(screen.getByText(/pages.aiAgent.toolCall.inspectBeforeRetry/)).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.failure' })).toBeNull();
        rerender(<AgentToolCall running={false} tool={{ ...tool, status: 'failed',
            output: JSON.stringify({ ...output, action_request_id: 'another-call' }) }} />);
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.outcomeUnknown' })).toBeNull();
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.failure' })).toBeTruthy();
    });

    it('marks a fixed outcome-unknown text as unknown rather than failed', () => {
        render(<AgentToolCall running={false} tool={{ ...tool, name: 'restore_text_file', status: 'failed',
            output: 'Execution did not return a conclusive receipt. Read the current target state before choosing the next action.' }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.outcomeUnknown' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.failure' })).toBeNull();
    });

    it('shows accessible status icons while folded and updates from running to success or failure', () => {
        const { container, rerender } = render(<AgentToolCall tool={tool} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.waiting' }).querySelector('.animate-spin')).toBeTruthy();
        rerender(<AgentToolCall tool={{ ...tool, status: 'ok', output: '' }} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.success' })).toBeTruthy();
        expect(container.querySelector('.animate-spin')).toBeNull();
        rerender(<AgentToolCall tool={{ ...tool, status: 'failed', output: 'access denied' }} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.failure' })).toBeTruthy();
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(container.querySelector('pre')).toBeNull();
        rerender(<AgentToolCall tool={tool} running={false} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.missing' })).toBeTruthy();
        expect(container.querySelector('.animate-spin')).toBeNull();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.success' })).toBeNull();
    });

    it('labels a persisted result as returned without claiming tool success', () => {
        render(<AgentToolCall tool={{ ...tool, status: 'returned', output: 'observed' }} running={false} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.returned' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.success' })).toBeNull();
    });

    it('does not label a verified non-send as a successful send', () => {
        const hash = 'a'.repeat(64);
        const output = JSON.stringify({ schema_version: 4, snapshot_id: 'snapshot',
            snapshot_sha256: hash, idempotency_key: `send:v1:${hash}`,
            outcome: 'definitely_not_sent', provider_receipt_id: null,
            evidence: 'precondition_rejected_before_activation', observed_at_unix_ms: 1 });
        render(<AgentToolCall tool={{ ...tool, name: 'send_gmail_message', status: 'ok', output }} running={false} />);
        expect(screen.getByRole('img', { name: 'pages.aiAgent.toolCall.returned' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.success' })).toBeNull();
    });

    it('starts folded, lazily renders payloads, and keeps expansion when output arrives', async () => {
        const { container, rerender } = render(<AgentToolCall tool={tool} running />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(container.querySelector('pre')).toBeNull();
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await waitFor(() => expect(container.querySelectorAll('pre')).toHaveLength(1));
        expect(container.querySelector('pre')?.textContent).toBe(JSON.stringify(JSON.parse(tool.argumentsJson), null, 2));
        expect(screen.getByText('pages.aiAgent.toolCall.waiting')).toBeTruthy();
        expect(screen.queryByLabelText('pages.aiAgent.toolCall.output')).toBeNull();
        rerender(<AgentToolCall tool={{ ...tool, status: 'failed', output: 'tool error: access denied <script>alert(1)</script>' }} running={false} />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(true);
        expect(screen.getByText(/tool error: access denied/)).toBeTruthy();
        expect(screen.getByLabelText('pages.aiAgent.toolCall.output')).toBeTruthy();
        expect(container.querySelector('script')).toBeNull();
    });

    it('distinguishes missing results from empty output after the conversation stops', async () => {
        const { rerender } = render(<AgentToolCall tool={tool} running={false} />);
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await screen.findByText('pages.aiAgent.toolCall.missing');
        rerender(<AgentToolCall tool={{ ...tool, status: 'ok', output: '' }} running={false} />);
        expect(screen.getByText('pages.aiAgent.toolCall.empty')).toBeTruthy();
    });
    it('shows the first failed batch step while keeping full inputs folded', () => {
        const { container } = render(<AgentToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', argumentsJson: '{"steps":[{"action":{"kind":"invoke"}}]}', output: '{"status":"stopped_on_error","failed_step_number":2,"error":{"message":"window closed"}}' }} running={false} />);
        expect(screen.getByText(/pages.aiAgent.toolCall.batchFailed/)).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
    });

    it.each([
        [0, 'no_effect', 'batchNotStarted'],
        [1, 'no_effect', 'batchPartiallyDispatched'],
        [0, 'may_have_effect', 'batchOutcomeUnknown'],
        [1, 'may_have_effect', 'batchOutcomeUnknown'],
    ])('shows the native execution stage for %s completed dispatches with %s', (count, effect, key) => {
        const output = JSON.stringify({ status: 'stopped_on_error', failed_step_number: Number(count) + 1, completed_steps: count, effect });
        const { container, rerender } = render(<AgentToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAgent.toolCall.${key}`))).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
        rerender(<AgentToolCall tool={{ ...tool, name: 'send_background_input', status: 'failed', output: JSON.stringify({ message: output }) }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAgent.toolCall.${key}`))).toBeTruthy();
    });

    it('does not infer non-execution from an inconsistent receipt', () => {
        render(<AgentToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output: JSON.stringify({ status: 'stopped_on_error', failed_step_number: 2, completed_steps: 0, effect: 'no_effect' }) }} running={false} />);
        expect(screen.getByText(/pages.aiAgent.toolCall.batchFailed/)).toBeTruthy();
        expect(screen.queryByText(/pages.aiAgent.toolCall.batchNotStarted/)).toBeNull();
    });

    it.each([
        ['definitely_not_started', 'batchNotStarted'],
        ['outcome_unknown', 'batchOutcomeUnknown'],
    ])('uses the typed %s result for an executor error without a step receipt', (result, key) => {
        render(<AgentToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output: JSON.stringify({ result, message: 'native UI request stopped' }) }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAgent.toolCall.${key}`))).toBeTruthy();
        expect(screen.queryByText(/pages.aiAgent.toolCall.batchFailed/)).toBeNull();
    });


    describe('decision receipts', () => {
        const receipt = JSON.stringify({ proposal_id: 'p1', status: 'pending', awaiting_decision: true });
        const proposal: AiAgentToolActivity = { ...tool, name: 'propose_skill', status: 'ok', output: receipt };

        it('shows a pending request as awaiting approval without the receipt JSON', () => {
            render(<AgentToolCall running={false} tool={{ ...proposal,
                decision: { kind: 'skill', requestId: 'p1', state: 'pending', awaitingOwner: false } }} />);
            expect(screen.getByRole('img', { name: 'pages.aiAgent.decision.state.pending' })).toBeTruthy();
            expect(screen.queryByRole('img', { name: 'pages.aiAgent.toolCall.returned' })).toBeNull();
            expect(screen.queryByRole('button', { name: 'pages.aiAgent.decision.review' })).toBeNull();
            fireEvent.click(screen.getByText(/propose_skill/));
            expect(screen.getByText('pages.aiAgent.decision.notReturned')).toBeTruthy();
            expect(screen.queryByText(/awaiting_decision/)).toBeNull();
            expect(screen.queryByText('pages.aiAgent.toolCall.output')).toBeNull();
        });

        it('links a request the owner must decide to its decision card', () => {
            const card = document.createElement('article');
            card.id = 'agent-directory-d1';
            card.tabIndex = -1;
            document.body.append(card);
            try {
                render(<AgentToolCall running={false} tool={{ ...proposal, name: 'request_directory',
                    output: JSON.stringify({ directory_request_id: 'd1', state: 'pending', awaiting_decision: true }),
                    decision: { kind: 'directory', requestId: 'd1', state: 'pending', awaitingOwner: true } }} />);
                fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.decision.review' }));
                expect(document.activeElement).toBe(card);
            } finally { card.remove(); }
        });

        it('shows the decision, who made it and the reason handed to the agent', () => {
            render(<AgentToolCall running={false} tool={{ ...proposal,
                decision: { kind: 'skill', requestId: 'p1', state: 'rejected', decidedBy: 'approval_ai', reason: 'Too broad' } }} />);
            const label = 'pages.aiAgent.decision.state.rejected · pages.aiAgent.decision.by.approval_ai';
            expect(screen.getByRole('img', { name: label })).toBeTruthy();
            fireEvent.click(screen.getByText(/propose_skill/));
            expect(screen.getByText('Too broad')).toBeTruthy();
            expect(screen.queryByText('pages.aiAgent.decision.notReturned')).toBeNull();
            expect(screen.queryByText(/awaiting_decision/)).toBeNull();
        });

        it('falls back to a submitted label when the snapshot has no decision source', () => {
            render(<AgentToolCall running={false} tool={{ ...proposal,
                decision: { kind: 'schedule', requestId: 's1', state: 'submitted' } }} />);
            expect(screen.getByRole('img', { name: 'pages.aiAgent.decision.state.submitted' })).toBeTruthy();
            fireEvent.click(screen.getByText(/propose_skill/));
            expect(screen.getByText('pages.aiAgent.decision.notReturned')).toBeTruthy();
            expect(screen.queryByText(/awaiting_decision/)).toBeNull();
        });
    });
});
