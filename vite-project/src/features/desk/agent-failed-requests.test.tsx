import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentFailedRequests } from './agent-failed-requests';
import { deskErrorCodeEnum, type MainRequestReceipt } from '@/services/types';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const failed = (requestId: string, errorCode?: number): MainRequestReceipt => ({
    requestId, clientMessageId: `${requestId}-input`, state: 'failed', failure: 'activation_failed',
    acceptedAtMs: 1_700_000_000_000, errorCode,
});
const details = (container: HTMLElement) => container.querySelector('details') as HTMLDetailsElement;

describe('agent failed requests', () => {
    it('names the recorded cause, or the generic activation failure without one', () => {
        render(<AgentFailedRequests waitingIds={[]} failed={[
            failed('with-code', deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE), failed('without-code'),
        ]} />);
        expect(screen.getByText('pages.agentError.aiContentSafetyUnavailable')).toBeTruthy();
        expect(screen.getByText('pages.aiAgent.queueFailure.activation_failed')).toBeTruthy();
    });

    it('keeps failures loaded with the page collapsed', () => {
        const { container } = render(<AgentFailedRequests waitingIds={[]} failed={[failed('earlier')]} />);
        expect(details(container).open).toBe(false);
        expect(screen.getByText('pages.aiAgent.failedQueuedRequests')).toBeTruthy();
    });

    it('opens when a request this page saw waiting fails', () => {
        const { container, rerender } = render(<AgentFailedRequests waitingIds={['current']} failed={[]} />);
        expect(container.querySelector('details')).toBeNull();
        rerender(<AgentFailedRequests waitingIds={[]} failed={[failed('current')]} />);
        expect(details(container).open).toBe(true);
    });

    it('stays closed after the owner closes it until another failure appears', () => {
        const { container, rerender } = render(<AgentFailedRequests waitingIds={['first', 'second']} failed={[]} />);
        rerender(<AgentFailedRequests waitingIds={['second']} failed={[failed('first')]} />);
        expect(details(container).open).toBe(true);
        details(container).open = false;
        fireEvent(details(container), new Event('toggle'));
        expect(details(container).open).toBe(false);
        rerender(<AgentFailedRequests waitingIds={['second', 'third']} failed={[failed('first')]} />);
        expect(details(container).open).toBe(false);
        rerender(<AgentFailedRequests waitingIds={['third']} failed={[failed('second'), failed('first')]} />);
        expect(details(container).open).toBe(true);
    });

        it('renders nothing without failures', () => {
        const { container } = render(<AgentFailedRequests waitingIds={['queued']} failed={[]} />);
        expect(container.firstChild).toBeNull();
    });
});
