import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentConnectionIcon } from './agent-connection-icon';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('agent connection icon', () => {
    it.each([
        [true, true, 'text-green-500', 'signalConnected'],
        [false, true, 'text-amber-500', 'signalConnecting'],
        [true, false, 'text-muted-foreground', 'disabledTitle'],
        [false, false, 'text-muted-foreground', 'disabledTitle'],
    ] as const)('renders connected=%s enabled=%s with accessible status', (connected, enabled, color, key) => {
        render(<AgentConnectionIcon connected={connected} enabled={enabled} />);
        const status = screen.getByRole('status');
        expect(status.title).toBe(`pages.aiAgent.${key}`);
        expect(status.textContent).toBe(status.title);
        expect(status.tabIndex).toBe(0);
        expect(status.querySelector('svg')?.classList.contains(color)).toBe(true);
        expect(status.querySelector('svg')?.getAttribute('aria-hidden')).toBe('true');
        expect(status.querySelector('span')?.className).toBe('sr-only');
    });

    it('updates color and explanation when connectivity changes', () => {
        const { rerender } = render(<AgentConnectionIcon connected enabled />);
        rerender(<AgentConnectionIcon connected={false} enabled />);
        expect(screen.getByRole('status').title).toBe('pages.aiAgent.signalConnecting');
        expect(screen.getByRole('status').querySelector('svg')?.classList.contains('text-amber-500')).toBe(true);
    });
});
