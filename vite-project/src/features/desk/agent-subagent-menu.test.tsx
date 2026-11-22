import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { tMock } from '@/test-utils/i18n-mock';
import { AgentSubagentMenu } from './agent-subagents';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.mock('@/hooks/use-mobile', () => ({ useIsMobile: () => true }));
afterEach(cleanup);

it('opens the shared owner approval settings without retargeting a child permission and hides unsupported approval settings', () => {
    const onApprovalSettings = vi.fn();
    const onSelect = vi.fn();
    const view = render(<AgentSubagentMenu onSelect={onSelect} onApprovalSettings={onApprovalSettings} />);
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAgent.workspace.actions') }));
    expect(screen.getByRole('button', { name: tMock('pages.aiAgent.contextTitle') }).querySelector('svg')).toHaveClass('lucide-layers');
    expect(screen.getByRole('button', { name: tMock('pages.aiAgent.workspace.details') }).querySelector('svg')).toHaveClass('lucide-list-tree');
    expect(screen.getByRole('button', { name: tMock('pages.aiAgent.autoApprovalTitle') }).querySelector('svg')).toHaveClass('lucide-bot');
    expect(screen.getByRole('button', { name: tMock('pages.aiAgent.permissionHistory') }).querySelector('svg')).toHaveClass('lucide-shield-check');
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAgent.autoApprovalTitle') }));
    expect(onApprovalSettings).toHaveBeenCalledExactlyOnceWith();
    expect(onSelect).not.toHaveBeenCalled();
    view.rerender(<AgentSubagentMenu onSelect={onSelect} />);
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAgent.workspace.actions') }));
    expect(screen.queryByRole('button', { name: tMock('pages.aiAgent.autoApprovalTitle') })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: tMock('pages.aiAgent.permissionHistory') }));
    expect(onSelect).toHaveBeenCalledExactlyOnceWith('permissions');
    expect(onApprovalSettings).toHaveBeenCalledTimes(1);
});
