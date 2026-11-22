import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { expect, it, vi } from 'vitest';
import { AiAgentOverview } from './ai-agent-overview';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

it('opens conversations, attention, and scheduled tasks from the AI Agent overview cards', () => {
    render(<MemoryRouter><AiAgentOverview /></MemoryRouter>);

    expect(screen.getByRole('link', { name: /Multi-device conversation/ })).toHaveAttribute('href', '/ai-agent/conversation');
    expect(screen.getByRole('link', { name: /AI conversations/ })).toHaveAttribute('href', '/ai-agent/conversations');
    expect(screen.getByRole('link', { name: /AI Agent attention/ })).toHaveAttribute('href', '/ai-agent/attention');
    expect(screen.getByRole('link', { name: /Scheduled tasks/ })).toHaveAttribute('href', '/schedules');
    expect(screen.getByRole('link', { name: /Long-term memory/ })).toHaveAttribute('href', '/ai-agent/memories');
    expect(screen.getByRole('link', { name: /Personalization/ })).toHaveAttribute('href', '/ai-agent/personalization');
    expect(screen.queryByText('pages.aiAgent.processing.description')).toBeNull();
});

it('keeps the personal conversation entry out of the organization overview', () => {
    render(<MemoryRouter><AiAgentOverview showPersonalConversation={false} showWorkbench={false} /></MemoryRouter>);

    expect(screen.queryByRole('link', { name: /Multi-device conversation/ })).not.toBeInTheDocument();
    expect(screen.getByRole('link', { name: /AI conversations/ })).toBeInTheDocument();
});
