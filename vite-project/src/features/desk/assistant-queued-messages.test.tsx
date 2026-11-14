import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { AssistantQueuedMessages } from './assistant-queued-messages';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

describe('AssistantQueuedMessages', () => {
    it('offers cancelling only queued inputs and marks activated ones', async () => {
        let finish: () => void = () => undefined;
        const onCancel = vi.fn(() => new Promise<void>(resolve => { finish = resolve; }));
        render(<AssistantQueuedMessages onCancel={onCancel} items={[
            { id: 'a', text: 'Starting next', requestId: 'ra', state: 'activated' },
            { id: 'b', text: 'Waiting', requestId: 'rb', state: 'queued' },
            { id: 'c', text: 'Still sending', requestId: null, state: null },
        ]} />);
        const items = screen.getAllByRole('listitem');
        expect(items.map(item => item.textContent)).toEqual(['Starting nextStarting soon', 'WaitingQueuedCancel', 'Still sending']);
        const cancel = screen.getByRole('button', { name: 'Cancel' });
        fireEvent.click(cancel);
        expect(onCancel).toHaveBeenCalledWith('rb');
        expect(cancel).toBeDisabled();
        await act(async () => finish());
        expect(cancel).not.toBeDisabled();
    });

    it('folds a long input and expands it locally', () => {
        const text = Array.from({ length: 8 }, (_, line) => `Line ${line + 1}`).join('\n');
        render(<AssistantQueuedMessages onCancel={vi.fn()} items={[
            { id: 'long', text, requestId: 'r', state: 'queued' },
            { id: 'short', text: 'Short', requestId: 's', state: 'queued' },
        ]} />);
        const body = screen.getByText(/Line 1/);
        expect(body).toHaveClass('line-clamp-4');
        expect(screen.getAllByRole('button', { name: 'Show more' })).toHaveLength(1);
        fireEvent.click(screen.getByRole('button', { name: 'Show more' }));
        expect(body).not.toHaveClass('line-clamp-4');
        expect(screen.getByRole('button', { name: 'Show less' })).toHaveAttribute('aria-expanded', 'true');
    });

    it('renders nothing without queued inputs', () => {
        const { container } = render(<AssistantQueuedMessages onCancel={vi.fn()} items={[]} />);
        expect(container).toBeEmptyDOMElement();
    });
});
