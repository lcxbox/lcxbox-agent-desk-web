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
    createdAtMs: 0, revision: 3, awaitingOwner: true,
};
const updated: SkillProposalView = {
    ...created, proposalId: 'p2', kind: 'update', baseSkillId: 's1', baseVersion: 2, skillMd: '', files: [],
    diff: [{ path: 'SKILL.md', change: 'modified', truncated: false,
        hunks: [{ oldStart: 3, newStart: 3, lines: [{ op: 'remove', text: 'Old step' }, { op: 'add', text: 'New step' }] }] }],
};
const disabling: SkillProposalView = {
    ...created, proposalId: 'p4', kind: 'disable', baseSkillId: 's1', baseVersion: 2, diff: [],
};
const deleting: SkillProposalView = { ...disabling, proposalId: 'p5', kind: 'delete' };
const ok = (data: SkillProposalView) => ({ success: true, code: 0, message: '', data });

beforeEach(() => vi.resetAllMocks());

describe('skill proposal card', () => {
    it('shows a new skill in full and approves it with the shown revision', async () => {
        const onDecided = vi.fn();
        render(<SkillProposalCard proposal={created} onRefresh={vi.fn()} onDecided={onDecided} />);
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
        render(<SkillProposalCard proposal={updated} onRefresh={vi.fn()} />);
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
        render(<SkillProposalCard proposal={created} onRefresh={onRefresh} />);
        fireEvent.click(screen.getByRole('button', { name: 'Approve' }));
        expect(await screen.findByRole('alert')).toHaveTextContent('The proposal changed and has been reloaded.');
        expect(onRefresh).toHaveBeenCalledTimes(1);
    });

    it('explains a conflicted proposal and offers no decision', () => {
        render(<SkillProposalCard proposal={{ ...updated, status: 'conflicted', decisionSource: 'ai_review' }} onRefresh={vi.fn()} />);
        expect(screen.getByText(/propose again based on the new version/)).toBeInTheDocument();
        expect(screen.getByText('AI review')).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    });

    it.each([
        { proposal: disabling, action: 'Disable skill' },
        { proposal: deleting, action: 'Delete skill' },
    ])('shows a $proposal.kind proposal with its content snapshot collapsed', ({ proposal, action }) => {
        render(<SkillProposalCard proposal={proposal} onRefresh={vi.fn()} />);
        const card = screen.getByRole('article', { name: 'disk-cleanup' });
        expect(within(card).getByText(action)).toBeInTheDocument();
        expect(within(card).queryByRole('group')).toBeNull();
        const skillMd = within(card).getByRole('button', { name: 'SKILL.md' });
        expect(skillMd).toHaveAttribute('aria-expanded', 'false');
        expect(within(card).getByRole('button', { name: 'scripts/clean.sh' })).toHaveAttribute('aria-expanded', 'false');
        expect(within(card).getByText(/Free space safely/).closest('[data-state]')).toHaveAttribute('data-state', 'closed');
        fireEvent.click(skillMd);
        expect(skillMd).toHaveAttribute('aria-expanded', 'true');
        expect(within(card).getByText(/Free space safely/).closest('[data-state]')).toHaveAttribute('data-state', 'open');
        expect(within(card).queryByText('Deleting cannot be undone') != null).toBe(proposal.kind === 'delete');
    });

    it('keeps a new skill expanded and warns only before deleting', () => {
        render(<SkillProposalCard proposal={created} onRefresh={vi.fn()} />);
        expect(screen.getByRole('button', { name: 'SKILL.md' })).toHaveAttribute('aria-expanded', 'true');
        expect(screen.queryByText('Deleting cannot be undone')).toBeNull();
    });

    it.each([
        { proposal: created, note: /propose again based on the new version/ },
        { proposal: updated, note: /propose again based on the new version/ },
        { proposal: disabling, note: /no longer assistant-written; nothing was changed/ },
        { proposal: deleting, note: /no longer assistant-written; nothing was changed/ },
    ])('explains a conflicted $proposal.kind proposal by its kind', ({ proposal, note }) => {
        render(<SkillProposalCard proposal={{ ...proposal, status: 'conflicted' }} onRefresh={vi.fn()} />);
        expect(screen.getByText(note)).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    });

    it('counts and marks only pending proposals the owner must decide now', () => {
        const reviewing = { ...updated, proposalId: 'p3', awaitingOwner: false };
        const proposals = [created, reviewing, { ...updated, status: 'approved' as const }];
        expect(pendingSkillProposalCount(proposals)).toBe(1);
        render(<SkillProposalCard proposal={reviewing} onRefresh={vi.fn()} />);
        const card = screen.getByRole('article');
        expect(card).not.toHaveAttribute('data-assistant-pending');
        expect(card).toHaveAttribute('id', 'assistant-skill-proposal-p3');
        expect(screen.getByRole('button', { name: 'Approve' })).toBeEnabled();
    });

    it('shows a proposal withdrawn by stopping the conversation without a decision', () => {
        render(<SkillProposalCard proposal={{ ...created, status: 'withdrawn' }} onRefresh={vi.fn()} />);
        expect(screen.getByText('Withdrawn')).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
        expect(screen.getByRole('article')).not.toHaveAttribute('data-assistant-pending');
    });
});
