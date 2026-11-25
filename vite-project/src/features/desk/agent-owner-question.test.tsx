import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
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

    it.each(['questions', 'follow_up'] as const)('reports a real field click once for %s and keeps the card open after confirmation', async kind => {
        let acknowledge!: (value: { success: boolean; data: string }) => void;
        respond.mockReturnValue(new Promise(resolve => { acknowledge = resolve; }));
        const prompt: OwnerQuestionView['prompt'] = kind === 'questions'
            ? { kind, questions: [{ question: 'Which timezone?' }] }
            : { kind, text: 'What needs adjusting?' };
        const timed = { ...wait, prompt, deadlineUnixMs: Date.now() + 60_000 };
        const view = render(<AgentOwnerQuestion wait={timed} session="work-1" refresh={vi.fn()} />);
        const field = screen.getByRole('textbox');
        act(() => field.focus());
        expect(respond).not.toHaveBeenCalled();
        fireEvent.click(field);
        fireEvent.change(field, { target: { value: 'UTC' } });
        expect(respond).toHaveBeenCalledExactlyOnceWith({
            session: 'work-1', waitId: 'group-1', clientRequestId: expect.any(String), action: { kind: 'interact' },
        });
        expect(screen.getByText(/Waiting until/)).toBeInTheDocument();
        await act(async () => acknowledge({ success: true, data: 'interacting' }));
        expect(screen.queryByText(/Waiting until/)).not.toBeInTheDocument();
        expect(field).toHaveValue('UTC');
        fireEvent.click(field);
        view.unmount();
        render(<AgentOwnerQuestion wait={{ ...timed, deadlineUnixMs: null }} session="work-1" refresh={vi.fn()} />);
        expect(screen.getByRole('textbox')).toHaveValue('UTC');
        expect(respond).toHaveBeenCalledOnce();
    });

    it.each(['question', 'background'])('reports a %s tap, while scrolling the card leaves the timer alone', async target => {
        respond.mockResolvedValue({ success: true, data: 'interacting' });
        render(<AgentOwnerQuestion wait={{ ...wait, deadlineUnixMs: Date.now() + 60_000 }} session="work-1" refresh={vi.fn()} />);
        const card = screen.getByRole('region', { name: 'Questions from the agent' });
        fireEvent.scroll(card);
        expect(respond).not.toHaveBeenCalled();
        const questionText = within(card).getByText('Which timezone?');
        fireEvent.click(target === 'background' ? card : questionText);
        await waitFor(() => expect(screen.queryByText(/Waiting until/)).not.toBeInTheDocument());
        expect(respond.mock.calls.map(([body]) => body.action.kind)).toEqual(['interact']);
    });

    it('closes a timed card directly without first declaring the user present', async () => {
        respond.mockResolvedValue({ success: true, data: 'declined' });
        render(<AgentOwnerQuestion wait={{ ...wait, deadlineUnixMs: Date.now() + 60_000 }} session="work-1" refresh={vi.fn()} />);
        fireEvent.click(screen.getByRole('button', { name: /close/i }));
        await waitFor(() => expect(screen.queryByRole('textbox')).not.toBeInTheDocument());
        expect(respond.mock.calls.map(([body]) => body.action.kind)).toEqual(['decline']);
    });

    it('retries interaction on the next input without discarding the answer or the deadline', async () => {
        respond.mockRejectedValueOnce(new Error('offline')).mockResolvedValueOnce({ success: true, data: 'interacting' });
        render(<AgentOwnerQuestion wait={{ ...wait, deadlineUnixMs: Date.now() + 60_000 }} session="work-1" refresh={vi.fn()} />);
        fireEvent.click(screen.getByRole('button', { name: 'UTC' }));
        await screen.findByRole('alert');
        expect(screen.getByText(/Waiting until/)).toBeInTheDocument();
        fireEvent.click(screen.getAllByRole('button', { name: 'Skip this question' })[1]);
        await waitFor(() => expect(screen.queryByText(/Waiting until/)).not.toBeInTheDocument());
        expect(respond).toHaveBeenCalledTimes(2);
        expect(respond.mock.calls[1][0]).toEqual(respond.mock.calls[0][0]);
        expect(screen.getByRole('button', { name: 'UTC' })).toHaveAttribute('aria-pressed', 'true');
    });

    it('allows a direct final submission before the interaction response arrives', async () => {
        let acknowledge!: (value: { success: boolean; data: string }) => void;
        respond.mockReturnValueOnce(new Promise(resolve => { acknowledge = resolve; }))
            .mockResolvedValueOnce({ success: true, data: 'answered' });
        const timed = { ...wait, deadlineUnixMs: Date.now() + 60_000,
            prompt: { kind: 'questions' as const, questions: [{ question: 'Timezone?' }] } };
        render(<AgentOwnerQuestion wait={timed} session="work-1" refresh={vi.fn()} />);
        fireEvent.change(screen.getByRole('textbox'), { target: { value: 'UTC' } });
        fireEvent.click(screen.getByRole('button', { name: 'Send' }));
        await waitFor(() => expect(screen.queryByRole('textbox')).not.toBeInTheDocument());
        expect(respond.mock.calls.map(([body]) => body.action.kind)).toEqual(['interact', 'answer']);
        await act(async () => acknowledge({ success: true, data: 'interacting' }));
        expect(screen.queryByText(/Waiting until/)).not.toBeInTheDocument();
    });

    it('shows the host timeout when a restored draft is submitted after its deadline', async () => {
        const prompt: OwnerQuestionView['prompt'] = { kind: 'follow_up', text: 'What needs adjusting?' };
        const view = render(<AgentOwnerQuestion wait={{ ...wait, prompt }} session="work-1" refresh={vi.fn()} />);
        fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Use UTC' } });
        view.unmount();
        respond.mockResolvedValue({ success: true, data: 'timed_out' });
        render(<><Toaster /><AgentOwnerQuestion wait={{ ...wait, prompt, deadlineUnixMs: Date.now() }} session="work-1" refresh={vi.fn()} /></>);
        expect(respond).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'Send reply' }));
        await waitFor(() => expect(screen.queryByRole('textbox')).not.toBeInTheDocument());
        expect(screen.getAllByText(tMock('pages.aiAgent.ownerQuestionTimedOut'))).not.toHaveLength(0);
        expect(respond.mock.calls.map(([body]) => body.action)).toEqual([{ kind: 'reply', text: 'Use UTC' }]);
    });

    it('shows the host timeout if the first click arrives too late', async () => {
        respond.mockResolvedValue({ success: true, data: 'timed_out' });
        render(<><Toaster /><AgentOwnerQuestion wait={{ ...wait, deadlineUnixMs: Date.now() }} session="work-1" refresh={vi.fn()} /></>);
        fireEvent.click(screen.getByRole('button', { name: 'Pacific' }));
        await waitFor(() => expect(screen.queryByRole('textbox')).not.toBeInTheDocument());
        expect(screen.getAllByText(tMock('pages.aiAgent.ownerQuestionTimedOut'))).not.toHaveLength(0);
        expect(respond.mock.calls.map(([body]) => body.action.kind)).toEqual(['interact']);
    });
});
