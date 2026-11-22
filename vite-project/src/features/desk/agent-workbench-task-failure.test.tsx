import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { expect, it, vi } from 'vitest';
import { tMock as t } from '@/test-utils/i18n-mock';
import { deskErrorCodeEnum, type AiAgentWorkbenchTask } from '@/services/types';
import { WorkbenchTasks } from './agent-workbench-tasks';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const task = (requestId: string, errorCode?: number): AiAgentWorkbenchTask => ({
    receipt: { requestId, clientMessageId: `${requestId}-input`, state: 'failed', failure: 'activation_failed',
        acceptedAtMs: 1, errorCode },
    updatedAtMs: 2, workerStates: [], deliveries: [],
});
vi.mock('@/services/clients', async importOriginal => ({
    ...await importOriginal<typeof import('@/services/clients')>(),
    listAiAgentWorkbenchTasks: async () => ({ success: true, data: {
        items: [task('with-code', deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE), task('without-code')],
    } }),
}));

it('shows why a work round could not start in the task list', async () => {
    render(<QueryClientProvider client={new QueryClient()}>
        <WorkbenchTasks workbench="main" active onOpen={() => {}} onClose={() => {}} />
    </QueryClientProvider>);
    expect(await screen.findByText(t('pages.agentError.aiContentSafetyUnavailable'))).toBeInTheDocument();
    expect(screen.getByText(t('pages.aiAgent.queueFailure.activation_failed'))).toBeInTheDocument();
});
