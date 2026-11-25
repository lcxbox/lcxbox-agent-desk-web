import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { Toaster } from '@/components/ui/toaster';
import { tMock } from '@/test-utils/i18n-mock';
import type { OwnerQuestionView } from '@/services/types';
import { AgentOwnerQuestion } from './agent-owner-question';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const respond = vi.hoisted(() => vi.fn());
vi.mock('@/services/clients', () => ({ respondToOwnerQuestion: respond }));

const wait: OwnerQuestionView = {
    waitId: 'group-1',
    prompt: {
        kind: 'questions',
        questions: [
            { question: 'Which timezone?', options: ['UTC', 'Pacific'] },
            { question: 'What duration?' },
            { question: 'Which log format?' },
        ],
    },
};

describe('AgentOwnerQuestion', () => {
    beforeEach(() => {
        respond.mockReset();
        sessionStorage.clear();
    });

    it('submits one complete response in question order after answering out of order', async () => {
        respond.mockResolvedValue({ success: true, data: 'answered' });
        const refresh = vi.fn();
        render(<AgentOwnerQuestion wait={wait} session="work-1" refresh={refresh} />);
        const send = screen.getByRole('button', { name: 'Send' });
        expect(send).toBeDisabled();
        const last = screen.getByRole('group', { name: 'Which log format?' });
        fireEvent.click(within(last).getByRole('button', { name: 'Skip this question' }));
        fireEvent.change(screen.getByRole('textbox', { name: 'What duration?' }), {
            target: { value: '30 seconds' },
        });
        expect(send).toBeDisabled();
        fireEvent.click(screen.getByRole('button', { name: 'UTC' }));
        expect(respond).not.toHaveBeenCalled();
        fireEvent.click(send);
        await waitFor(() => expect(refresh).toHaveBeenCalledOnce());
        expect(respond).toHaveBeenCalledExactlyOnceWith({
            session: 'work-1', waitId: 'group-1', clientRequestId: expect.any(String),
            action: { kind: 'answer', answers: [
                { kind: 'choice', value: 'UTC' },
                { kind: 'text', value: '30 seconds' },
                { kind: 'skipped' },
            ] },
        });
        expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
    });

    it('declines without transmitting drafts when the user explicitly closes the card', async () => {
        respond.mockResolvedValue({ success: true, data: 'declined' });
        const refresh = vi.fn();
        render(<AgentOwnerQuestion wait={wait} session="work-1" refresh={refresh} />);
        fireEvent.change(screen.getByRole('textbox', { name: 'What duration?' }), {
            target: { value: 'unfinished answer' },
        });
        fireEvent.click(screen.getByRole('button', { name: /close/i }));
        await waitFor(() => expect(refresh).toHaveBeenCalledOnce());
        expect(respond).toHaveBeenCalledExactlyOnceWith({
            session: 'work-1', waitId: 'group-1', clientRequestId: expect.any(String),
            action: { kind: 'decline' },
        });
    });

    it('leaves the wait open on unmount and restores its draft only for the same wait', () => {
        const refresh = vi.fn();
        const view = render(<AgentOwnerQuestion wait={wait} session="work-1" refresh={refresh} />);
        fireEvent.click(screen.getByRole('button', { name: 'Pacific' }));
        fireEvent.change(screen.getByRole('textbox', { name: 'What duration?' }), {
            target: { value: 'unfinished answer' },
        });
        view.unmount();

        const other = render(<AgentOwnerQuestion wait={{ ...wait, waitId: 'other' }} session="work-1" refresh={refresh} />);
        expect(screen.getByRole('textbox', { name: 'What duration?' })).toHaveValue('');
        other.unmount();

        render(<AgentOwnerQuestion wait={wait} session="work-1" refresh={refresh} />);
        expect(screen.getByRole('textbox', { name: 'What duration?' })).toHaveValue('unfinished answer');
        expect(screen.getByRole('button', { name: 'Pacific' })).toHaveAttribute('aria-pressed', 'true');
        expect(respond).not.toHaveBeenCalled();
        expect(refresh).not.toHaveBeenCalled();
    });

    it('retries a failed follow-up with the original submission ID, text and wait ID', async () => {
        respond.mockRejectedValueOnce(new Error('Connection lost'))
            .mockResolvedValueOnce({ success: true, data: 'replied' });
        const followUp: OwnerQuestionView = { waitId: 'follow-up-2', prompt: { kind: 'follow_up', text: 'What should change?' } };
        render(<AgentOwnerQuestion wait={followUp} session="work-1" refresh={vi.fn()} />);
        fireEvent.change(screen.getByRole('textbox', { name: 'What should change?' }), {
            target: { value: 'Only use the first device' },
        });
        const submit = screen.getByRole('button', { name: 'Send reply' });
        fireEvent.click(submit);
        await screen.findByRole('alert');
        expect(screen.getByRole('textbox')).toHaveValue('Only use the first device');
        fireEvent.click(submit);
        await waitFor(() => expect(screen.queryByRole('textbox')).not.toBeInTheDocument());
        const original = respond.mock.calls[0][0];
        expect(original).toEqual({
            session: 'work-1', waitId: 'follow-up-2', clientRequestId: expect.any(String),
            action: { kind: 'reply', text: 'Only use the first device' },
        });
        expect(respond).toHaveBeenNthCalledWith(2, original);
    });

    it('shows that a late submission timed out even when the REST call succeeded', async () => {
        respond.mockResolvedValue({ success: true, data: 'timed_out' });
        const timedWait: OwnerQuestionView = {
            ...wait, deadlineUnixMs: Date.now() + 1000,
            prompt: { kind: 'questions', questions: [{ question: 'Which timezone?' }] },
        };
        const refresh = vi.fn();
        render(<><Toaster /><AgentOwnerQuestion wait={timedWait} session="work-1" refresh={refresh} /></>);
        fireEvent.change(screen.getByRole('textbox', { name: 'Which timezone?' }), { target: { value: 'UTC' } });
        fireEvent.click(screen.getByRole('button', { name: 'Send' }));
        await waitFor(() => expect(refresh).toHaveBeenCalledOnce());
        expect(await screen.findAllByText(tMock('pages.aiAgent.ownerQuestionTimedOut'))).not.toHaveLength(0);
        expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
        expect(respond).toHaveBeenCalledOnce();
    });
});
