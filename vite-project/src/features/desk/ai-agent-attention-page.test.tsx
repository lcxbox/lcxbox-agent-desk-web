import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import AiAgentAttentionPage from './ai-agent-attention-page';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('@/services/hooks/connectionController/useListConnections', () => ({
    useListConnections: () => ({ isError: false, isPending: false, data: { data: [] } }),
}));

afterEach(() => vi.unstubAllGlobals());

it('lists a paused proposal awaiting the owner and opens its conversation', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, json: async () => ({ data: { hasMore: false, items: [{
        attentionId: 'decision', sessionId: 'session', clientConversationId: 'conversation-1', deviceId: '',
        reason: 'decision_approval', requestId: 'proposal-1', updatedAtUnixMs: 1, deadlineUnixMs: null, workbench: null,
    }] } }) })));
    render(<MemoryRouter><AiAgentAttentionPage /></MemoryRouter>);
    expect(await screen.findByText('Review AI proposal or directory request')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: /Open/ })).toHaveAttribute('href', '/ai-agent/conversation?conversation=conversation-1');
});
