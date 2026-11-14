import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { RestResponseError } from '@/lib/kubb-client';
import { deskErrorCodeEnum, type SkillProposalView } from '@/services/types';
import { pendingSkillProposalCount, SkillProposalCard } from './assistant-skill-proposal-card';

const api = vi.hoisted(() => ({ decideAiAssistantSkillProposal: vi.fn() }));
vi.mock('@/services/clients', () => api);
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const created: SkillProposalView = {
    proposalId: 'p1', conversationId: 'c1', kind: 'create', name: 'disk-cleanup', reason: 'You asked for it twice',
    status: 'pending', skillMd: '---\nname: disk-cleanup\n---\nFree space safely.',
    files: [{ path: 'scripts/clean.sh', text: 'du -sh /tmp' }], diff: [],
    createdAtMs: 0, expiresAtMs: 1, revision: 3,
};
const updated: SkillProposalView = {
    ...created, proposalId: 'p2', kind: 'update', baseSkillId: 's1', baseVersion: 2, skillMd: '', files: [],
    diff: [{ path: 'SKILL.md', change: 'modified', truncated: false,
        hunks: [{ oldStart: 3, newStart: 3, lines: [{ op: 'remove', text: 'Old step' }, { op: 'add', text: 'New step' }] }] }],
};
const ok = (data: SkillProposalView) => ({ success: true, code: 0, message: '', data });

beforeEach(() => vi.resetAllMocks());

describe('skill proposal card', () => {
    it('shows a new skill in full and approves it with the shown revision', async () => {
        const onDecided = vi.fn();
        render(<SkillProposalCard proposal={created} awaitingOwner onRefresh={vi.fn()} onDecided={onDecided} />);
        const card = screen.getByRole('article', { name: 'disk-cleanup' });
        expect(card).toHaveAttribute('data-assistant-pending');
        expect(within(card).getByText('New skill')).toBeInTheDocument();
        expect(within(card).getByText('You asked for it twice')).toBeInTheDocument();
        expect(within(card).getByText(/Free space safely/)).toBeInTheDocument();
        expect(within(card).getByText('du -sh /tmp')).toBeInTheDocument();
        const approved = { ...created, status: 'approved', decisionSource: 'owner', revision: 4 } as const;
        api.decideAiAssistantSkillProposal.mockResolvedValue(ok(approved));
        fireEvent.click(within(card).getByRole('button', { name: 'Approve' }));
        await waitFor(() => expect(api.decideAiAssistantSkillProposal).toHaveBeenCalledWith({ proposalId: 'p1', approve: true, expectedRevision: 3 }));
        expect(await within(card).findByText('Approved')).toBeInTheDocument();
        expect(within(card).getByText('Confirmed by you')).toBeInTheDocument();
        expect(within(card).queryByRole('button', { name: 'Approve' })).toBeNull();
        expect(card).not.toHaveAttribute('data-assistant-pending');
        expect(onDecided).toHaveBeenCalledWith(approved);
    });

    it('renders an update as the diff to its base version and rejects it', async () => {
        render(<SkillProposalCard proposal={updated} awaitingOwner onRefresh={vi.fn()} />);
        const card = screen.getByRole('article', { name: 'disk-cleanup' });
        expect(within(card).getByText('New version')).toBeInTheDocument();
        const diff = within(card).getByRole('group', { name: 'SKILL.md' });
        expect(diff.querySelector('[data-op="remove"]')).toHaveTextContent('Old step');
        expect(diff.querySelector('[data-op="add"]')).toHaveTextContent('New step');
        api.decideAiAssistantSkillProposal.mockResolvedValue(ok({ ...updated, status: 'rejected', revision: 4 }));
        fireEvent.click(within(card).getByRole('button', { name: 'Reject' }));
        await waitFor(() => expect(api.decideAiAssistantSkillProposal).toHaveBeenCalledWith({ proposalId: 'p2', approve: false, expectedRevision: 3 }));
        expect(await within(card).findByText('Rejected')).toBeInTheDocument();
    });

    it('refreshes a stale proposal after a revision conflict', async () => {
        const onRefresh = vi.fn();
        api.decideAiAssistantSkillProposal.mockRejectedValue(new RestResponseError('changed', deskErrorCodeEnum.REVISION_CONFLICT, null));
        render(<SkillProposalCard proposal={created} awaitingOwner onRefresh={onRefresh} />);
        fireEvent.click(screen.getByRole('button', { name: 'Approve' }));
        expect(await screen.findByRole('alert')).toHaveTextContent('The proposal changed and has been reloaded.');
        expect(onRefresh).toHaveBeenCalledTimes(1);
    });

    it('explains a conflicted proposal and offers no decision', () => {
        render(<SkillProposalCard proposal={{ ...updated, status: 'conflicted', decisionSource: 'ai_review' }} awaitingOwner onRefresh={vi.fn()} />);
        expect(screen.getByText(/propose again based on the new version/)).toBeInTheDocument();
        expect(screen.getByText('AI review')).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    });

    it('counts pending proposals only while the owner is the reviewer', () => {
        const proposals = [created, { ...updated, status: 'approved' as const }];
        expect(pendingSkillProposalCount(proposals, true)).toBe(1);
        expect(pendingSkillProposalCount(proposals, false)).toBe(0);
        render(<SkillProposalCard proposal={created} awaitingOwner={false} onRefresh={vi.fn()} />);
        expect(screen.getByRole('article')).not.toHaveAttribute('data-assistant-pending');
        expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
    });
});
