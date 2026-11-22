import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentReasoning } from './agent-reasoning';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('agent reasoning', () => {
    it('is folded by default and can be expanded without changing the text', () => {
        const { container } = render(<AgentReasoning text="Model supplied reasoning" />);
        const details = container.querySelector('[data-slot="disclosure"]')!;
        expect(details.getAttribute('data-state') === 'open').toBe(false);
        fireEvent.click(screen.getByText('pages.aiAgent.reasoning'));
        expect(details.getAttribute('data-state') === 'open').toBe(true);
        expect(screen.getByText('Model supplied reasoning')).toBeTruthy();
    });
    it('does not create an empty thinking section', () => {
        const { container, rerender } = render(<AgentReasoning text={undefined} />);
        expect(container.innerHTML).toBe('');
        rerender(<AgentReasoning text={' \n'} />);
        expect(container.innerHTML).toBe('');
    });
});
