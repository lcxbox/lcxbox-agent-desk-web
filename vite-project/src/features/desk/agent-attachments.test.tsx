import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { selectOption } from '@/test-utils/select-option';
import { AgentAttachments, AgentResultAttachments } from './agent-attachments';
import { deleteAgentAttachments, getAgentAttachment, listAgentAttachments, readAgentAttachment } from '@/services/clients';

vi.mock('@/services/clients', () => ({
    deleteAgentAttachments: vi.fn(), getAgentAttachment: vi.fn(),
    listAgentAttachments: vi.fn(), readAgentAttachment: vi.fn(),
}));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const key = (name: string) => `pages.aiAgent.attachments.${name}`;
const item = {
    attachment_id: 'attachment-a', message_id: 'message', tool_call_id: 'call', part: 'stdout',
    kind: 'text', media_type: 'text/plain', original_bytes: 5000, stored_bytes: 5000,
    source_truncated: false, storage_truncated: false, created_at_unix_ms: 1000, last_accessed_at_unix_ms: 1000,
    status: 'available', unavailable_at_unix_ms: null,
};
const listing = (status = 'available', kind = 'text') => ({ success: true, code: 0,
    data: { attachments: [{ ...item, status, kind }], used_bytes: status === 'available' ? 5000 : 0, capacity_bytes: 104857600, cursor: null },
});
beforeEach(() => {
    vi.mocked(listAgentAttachments).mockResolvedValue(listing());
    vi.mocked(deleteAgentAttachments).mockResolvedValue({ success: true, code: 0, data: true });
    vi.mocked(readAgentAttachment).mockResolvedValue({ success: true, code: 0, data: {
        attachment_id: item.attachment_id, queries: [], body_bytes: 22, cursor: null,
        has_more: false, json_fragment: false, storage_truncated: false,
        lines: [{ line: 1, byte_offset_in_line: 0, text: '<script>bad()</script>', matched_queries: [], context: false, line_complete: true }],
    } });
    URL.createObjectURL = vi.fn(() => 'blob:attachment');
    URL.revokeObjectURL = vi.fn();
});
afterEach(() => { cleanup(); vi.resetAllMocks(); vi.unstubAllGlobals(); });
async function open() {
    fireEvent.click(screen.getByRole('button', { name: key('title') }));
    await screen.findAllByRole('button', { name: key('view') });
}

it('lists metadata only, including refresh and filters, until an explicit view', async () => {
    render(<AgentAttachments sessionId="run" />);
    expect(listAgentAttachments).not.toHaveBeenCalled();
    await open();
    fireEvent.click(screen.getByRole('button', { name: key('refresh') }));
    await selectOption(screen.getByRole('combobox', { name: key('type') }), key('text'));
    expect(getAgentAttachment).not.toHaveBeenCalled();
    expect(readAgentAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    await screen.findByText('<script>bad()</script>');
    expect(document.querySelector('script')).toBeNull();
    expect(readAgentAttachment).toHaveBeenCalledWith(expect.objectContaining({ session: 'run', attachment_id: item.attachment_id }), expect.anything());
});

it('searches multiple text keywords and offers no JSON content search', async () => {
    const view = render(<AgentAttachments sessionId="run" />);
    await open();
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    await screen.findByText('<script>bad()</script>');
    fireEvent.change(screen.getByRole('textbox', { name: key('queries') }), { target: { value: 'error\n错误' } });
    fireEvent.click(screen.getByRole('button', { name: key('search') }));
    await waitFor(() => expect(readAgentAttachment).toHaveBeenLastCalledWith(expect.objectContaining({ queries: ['error', '错误'] }), expect.anything()));
    vi.mocked(listAgentAttachments).mockResolvedValue(listing('available', 'json'));
    view.rerender(<AgentAttachments sessionId="json-run" />);
    await screen.findByRole('button', { name: key('view') });
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    expect(screen.queryByRole('textbox', { name: key('queries') })).toBeNull();
});

it('confirms deletion, refreshes quota and retains the tombstone without a new content read', async () => {
    render(<AgentAttachments sessionId="run" />);
    await open();
    vi.mocked(listAgentAttachments).mockResolvedValue(listing('deleted'));
    fireEvent.click(screen.getByRole('checkbox'));
    fireEvent.click(screen.getByRole('button', { name: /deleteSelected/ }));
    expect(deleteAgentAttachments).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'common.cancel' }));
    expect(deleteAgentAttachments).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.getByRole('button', { name: /deleteSelected/ })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: /deleteSelected/ }));
    fireEvent.click(screen.getByRole('button', { name: 'common.confirm' }));
    await waitFor(() => expect(deleteAgentAttachments).toHaveBeenCalledWith({ session: 'run', attachment_ids: [item.attachment_id] }, expect.anything()));
    await waitFor(() => expect((screen.getByRole('button', { name: key('view') }) as HTMLButtonElement).disabled).toBe(true));
    expect(readAgentAttachment).not.toHaveBeenCalled();
});

it('aborts old conversation requests and ignores a late metadata response', async () => {
    let finish: (value: ReturnType<typeof listing>) => void = () => {};
    vi.mocked(listAgentAttachments).mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    const view = render(<AgentAttachments sessionId="old-run" />);
    fireEvent.click(screen.getByRole('button', { name: key('title') }));
    await waitFor(() => expect(listAgentAttachments).toHaveBeenCalledTimes(1));
    const oldSignal = vi.mocked(listAgentAttachments).mock.calls[0][1]?.signal;
    view.rerender(<AgentAttachments sessionId="new-run" />);
    await screen.findByRole('button', { name: key('view') });
    expect(oldSignal?.aborted).toBe(true);
    finish({ ...listing(), data: { ...listing().data, attachments: [{ ...item, attachment_id: 'old-only' }] } });
    await waitFor(() => expect(screen.queryByLabelText(`${key('select')} old-only`)).toBeNull());
});

it('filters type and availability with the themed menus and disables unavailable selection', async () => {
    vi.mocked(listAgentAttachments).mockResolvedValue({ ...listing(), data: {
        ...listing().data, attachments: [item, { ...item, attachment_id: 'deleted-image', kind: 'image', status: 'deleted', part: 'old-image' }],
    } });
    render(<AgentAttachments sessionId="run" />);
    await open();
    await selectOption(screen.getByRole('combobox', { name: key('type') }), key('image'));
    expect(screen.getByText(/old-image/)).toBeVisible();
    expect(screen.getByRole('checkbox')).toBeDisabled();
    expect(screen.queryByLabelText(`${key('select')} ${item.attachment_id}`)).toBeNull();
    await selectOption(screen.getByRole('combobox', { name: key('status') }), key('available'));
    expect(screen.queryByRole('checkbox')).toBeNull();
    expect(screen.getByText(key('empty'))).toBeVisible();
    await selectOption(screen.getByRole('combobox', { name: key('type') }), key('all'));
    expect(screen.getByRole('checkbox')).toBeEnabled();
    expect(getAgentAttachment).not.toHaveBeenCalled();
});

it('opens only references from the result without fetching bodies automatically', async () => {
    vi.mocked(listAgentAttachments).mockResolvedValue({ ...listing(), data: {
        ...listing().data, attachments: [item, { ...item, attachment_id: 'unrelated', part: 'other-stream' }],
    } });
    render(<AgentResultAttachments sessionId="run" text={JSON.stringify({ parts: [{ content: { reference: { attachment_id: item.attachment_id } } }] })} />);
    expect(listAgentAttachments).not.toHaveBeenCalled();
    await open();
    expect(screen.queryByText('other-stream')).toBeNull();
    expect(screen.getAllByRole('button', { name: key('view') })).toHaveLength(1);
    expect(readAgentAttachment).not.toHaveBeenCalled();
    expect(getAgentAttachment).not.toHaveBeenCalled();
});
