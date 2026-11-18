import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantToolGroup } from './assistant-tool-group';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('assistant tool activity summary', () => {
    it('describes a returned historical result without implying a failed operation', () => {
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'inspect_desktop_ui', status: 'returned',
                argumentsJson: '{}', output: '{"ReadContext":{}}' }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.workspace.toolGroupReturned')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });

    it('counts a confirmed non-send as returned, not as a successful send', () => {
        const hash = 'a'.repeat(64);
        const output = JSON.stringify({ schema_version: 4, snapshot_id: 'snapshot',
            snapshot_sha256: hash, idempotency_key: `send:v1:${hash}`,
            outcome: 'definitely_not_sent', provider_receipt_id: null,
            evidence: 'precondition_rejected_before_activation', observed_at_unix_ms: 1 });
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'send_gmail_message', status: 'ok',
                argumentsJson: '{}', output }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.workspace.toolGroupReturned')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });

    it('keeps a genuinely unknown action result separate from a successful tool call', () => {
        const output = JSON.stringify({ work_id: 'work', action_request_id: 'call-1',
            execution_generation: 'generation', result: 'outcome_unknown', facts: [], output: null });
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'execute_ui_actions', status: 'ok',
                argumentsJson: '{}', output }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.workspace.toolGroupOutcomeUnknown')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });

    it('shows a readable failure reason without exposing an opaque JSON receipt while folded', () => {
        const { container } = render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'execute_ui_actions', status: 'failed',
                argumentsJson: '{}', output: JSON.stringify({ work_id: '7bca7a6b-75fa-481d-8bc2-e2d026477925',
                    error: { message: 'Window closed before step 2' } }) }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('Window closed before step 2')).toBeTruthy();
        expect(screen.queryByText(/7bca7a6b/)).toBeNull();
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state')).toBe('closed');
    });

    it('names a refused call in the folded summary instead of the model-facing English result', () => {
        for (const [output, key] of [
            ['not executed: the owner refused this call', 'ownerRefused'],
            ['not executed: the owner denied this call', 'ownerRefused'],
            ['not executed: AI review refused this call; see the approval record for the reason', 'aiReviewRefused'],
            ['not executed: independent AI review denied this exact action. reason_code=scope. Ask the owner.', 'aiReviewRefused'],
            ['not executed: this call was not approved', 'notApproved'],
            ['not executed: task permission request was rejected', 'taskPermissionRejected'],
        ]) {
            const { unmount } = render(<AssistantToolGroup
                messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
                tools={[{ callId: 'call-1', name: 'restore_command_changes', status: 'failed', argumentsJson: '{}', output }]}
                renderMessage={() => <div>Details</div>} />);
            expect(screen.getByText(`pages.aiAssistant.workspace.refusal.${key}`)).toBeTruthy();
            expect(screen.queryByText(/not executed/)).toBeNull();
            unmount();
        }
        // Other plain-text failures keep their own message.
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'exec_command', status: 'failed', argumentsJson: '{}',
                output: 'not executed: a prior command in this turn was not run' }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('not executed: a prior command in this turn was not run')).toBeTruthy();
    });

    it('summarizes a decision receipt by its decision instead of a returned result', () => {
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'propose_skill', status: 'ok', argumentsJson: '{}',
                output: '{"proposal_id":"p1","status":"pending","awaiting_decision":true}',
                decision: { kind: 'skill', requestId: 'p1', state: 'approved', decidedBy: 'owner' } }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.decision.state.approved · pages.aiAssistant.decision.by.owner')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupReturned')).toBeNull();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });
});
