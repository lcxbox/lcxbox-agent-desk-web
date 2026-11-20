import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantFailedRequests } from './assistant-failed-requests';
import { deskErrorCodeEnum, type MainRequestReceipt } from '@/services/types';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const failed = (requestId: string, errorCode?: number): MainRequestReceipt => ({
    requestId, clientMessageId: `${requestId}-input`, state: 'failed', failure: 'activation_failed',
    acceptedAtMs: 1_700_000_000_000, errorCode,
});
const details = (container: HTMLElement) => container.querySelector('details') as HTMLDetailsElement;

describe('assistant failed requests', () => {
    it('names the recorded cause, or the generic activation failure without one', () => {
        render(<AssistantFailedRequests waitingIds={[]} failed={[
            failed('with-code', deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE), failed('without-code'),
        ]} />);
        expect(screen.getByText('pages.agentError.aiContentSafetyUnavailable')).toBeTruthy();
        expect(screen.getByText('pages.aiAssistant.queueFailure.activation_failed')).toBeTruthy();
    });

    it('keeps failures loaded with the page collapsed', () => {
        const { container } = render(<AssistantFailedRequests waitingIds={[]} failed={[failed('earlier')]} />);
        expect(details(container).open).toBe(false);
        expect(screen.getByText('pages.aiAssistant.failedQueuedRequests')).toBeTruthy();
    });

    it('opens when a request this page saw waiting fails', () => {
        const { container, rerender } = render(<AssistantFailedRequests waitingIds={['current']} failed={[]} />);
        expect(container.querySelector('details')).toBeNull();
        rerender(<AssistantFailedRequests waitingIds={[]} failed={[failed('current')]} />);
        expect(details(container).open).toBe(true);
    });

    it('stays closed after the owner closes it until another failure appears', () => {
        const { container, rerender } = render(<AssistantFailedRequests waitingIds={['first', 'second']} failed={[]} />);
        rerender(<AssistantFailedRequests waitingIds={['second']} failed={[failed('first')]} />);
        expect(details(container).open).toBe(true);
        details(container).open = false;
        fireEvent(details(container), new Event('toggle'));
        expect(details(container).open).toBe(false);
        rerender(<AssistantFailedRequests waitingIds={['second', 'third']} failed={[failed('first')]} />);
        expect(details(container).open).toBe(false);
        rerender(<AssistantFailedRequests waitingIds={['third']} failed={[failed('second'), failed('first')]} />);
        expect(details(container).open).toBe(true);
    });

        it('renders nothing without failures', () => {
        const { container } = render(<AssistantFailedRequests waitingIds={['queued']} failed={[]} />);
        expect(container.firstChild).toBeNull();
    });
});
