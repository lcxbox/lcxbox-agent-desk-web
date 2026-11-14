import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { RestResponseError } from '@/lib/kubb-client';
import { deskErrorCodeEnum } from '@/services/types';
import { AssistantSkillsSettings } from './assistant-skills-settings';

const api = vi.hoisted(() => ({
    listPlatformSkills: vi.fn(), previewPlatformSkill: vi.fn(), createPlatformSkill: vi.fn(), getPlatformSkill: vi.fn(),
    previewPlatformSkillVersion: vi.fn(), updatePlatformSkill: vi.fn(), deletePlatformSkill: vi.fn(),
    listBundledSkills: vi.fn(), setBundledSkill: vi.fn(),
    listMySkills: vi.fn(), previewMySkill: vi.fn(), createMySkill: vi.fn(), getMySkill: vi.fn(),
    previewMySkillVersion: vi.fn(), updateMySkill: vi.fn(), deleteMySkill: vi.fn(),
    getPlatformSkillScan: vi.fn(), getMySkillScan: vi.fn(), getPlatformSkillVersion: vi.fn(), getMySkillVersion: vi.fn(),
    listAiAssistantSkillProposals: vi.fn(), decideAiAssistantSkillProposal: vi.fn(),
}));
vi.mock('@/services/clients', () => api);
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

const ok = <T,>(data: T) => ({ success: true, code: 0, message: '', data });
const summary = {
    skillId: 's1', scope: 'personal', name: 'log-triage', description: 'Triage service logs', enabled: true,
    currentVersion: 2, revision: 7, origin: 'ai_authored', decisionSource: 'owner', updatedAtMs: 0,
} as const;
const detail = {
    summary, compatibility: 'Linux', license: 'MIT', metadata: { team: 'ops' },
    skillMd: '---\nname: log-triage\ndescription: Triage service logs\n---\nRead logs.',
    files: [{ path: 'references/a.md', kind: 'reference', sizeBytes: 3, text: 'old' }],
    versions: [
        { version: 2, origin: 'ai_authored', sizeBytes: 90, createdAtMs: 0 },
        { version: 1, origin: 'imported', sizeBytes: 80, createdAtMs: 0,
            scan: { verdict: 'suspicious', findings: [{ path: 'SKILL.md', line: 2, category: 'injection', reason: 'asks to skip approval' }] } },
    ],
};
const preview = (extra: object = {}) => ({
    name: 'new-skill', files: [{ path: 'scripts/run.sh', kind: 'script', language: 'shell', sizeBytes: 12 }],
    skipped: [{ path: 'assets/logo.png', reason: 'binary' }], warnings: [], diff: [], ...extra,
});

beforeEach(() => {
    vi.resetAllMocks();
    api.listMySkills.mockResolvedValue(ok([summary]));
    api.getMySkillScan.mockResolvedValue(ok({ available: true }));
    api.listAiAssistantSkillProposals.mockResolvedValue(ok([]));
    api.listPlatformSkills.mockResolvedValue(ok([]));
    api.listBundledSkills.mockResolvedValue(ok({ revision: 3, skills: [
        { name: 'issue-analysis', description: 'Analyse issues', enabled: true, skillMd: '---', files: [] },
    ] }));
});

async function startImport() {
    render(<AssistantSkillsSettings scope="personal" />);
    await screen.findByText('log-triage');
    fireEvent.click(screen.getByRole('button', { name: 'Import' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.mouseDown(within(dialog).getByRole('tab', { name: 'Paste SKILL.md' }));
    fireEvent.change(await within(dialog).findByLabelText('SKILL.md'), { target: { value: '---\nname: new-skill\n---' } });
    return dialog;
}

describe('assistant skill management', () => {
    it('explains the scope and shows origin and decision source', async () => {
        render(<AssistantSkillsSettings scope="personal" />);
        expect(await screen.findByText('log-triage')).toBeInTheDocument();
        expect(screen.getByText(/only for work you assign directly/)).toBeInTheDocument();
        expect(screen.getByText('AI-authored')).toBeInTheDocument();
        expect(screen.getByText('Confirmed by you')).toBeInTheDocument();
        expect(api.listBundledSkills).not.toHaveBeenCalled();
    });

    it('scans imports by default and requires an explicit save anyway for suspicious content', async () => {
        const dialog = await startImport();
        expect(within(dialog).getByRole('switch', { name: 'Scan with AI for security issues' })).toBeChecked();
        api.previewMySkill.mockResolvedValue(ok(preview({ scanId: 'scan-1', scan: { verdict: 'suspicious',
            findings: [{ path: 'scripts/run.sh', line: 4, category: 'exfiltration', reason: 'uploads credentials' }] } })));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        expect(await within(dialog).findByText('scripts/run.sh:4')).toBeInTheDocument();
        expect(within(dialog).getByText(/uploads credentials/)).toBeInTheDocument();
        expect(within(dialog).getByText('Not saved assets/logo.png: binary file')).toBeInTheDocument();
        expect(api.previewMySkill).toHaveBeenCalledWith({ package: { skillMd: '---\nname: new-skill\n---' }, aiScan: true });
        expect(within(dialog).queryByRole('button', { name: 'Save' })).toBeNull();
        api.createMySkill.mockResolvedValue(ok({ skill: { ...summary, skillId: 's2', name: 'new-skill', origin: 'imported' },
            files: [], skipped: [], warnings: [] }));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Save anyway' }));
        await waitFor(() => expect(api.createMySkill).toHaveBeenCalledWith({
            package: { skillMd: '---\nname: new-skill\n---' }, scanId: 'scan-1' }));
        expect(await screen.findByText('new-skill')).toBeInTheDocument();
        expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('notes an unavailable scan and saves normally; turning the scan off skips it', async () => {
        const dialog = await startImport();
        api.previewMySkill.mockResolvedValue(ok(preview({ scan: { verdict: 'unavailable', findings: [] } })));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        expect(await within(dialog).findByText(/scan is unavailable/)).toBeInTheDocument();
        expect(within(dialog).getByRole('button', { name: 'Save' })).toBeEnabled();
        fireEvent.click(within(dialog).getByRole('switch', { name: 'Scan with AI for security issues' }));
        expect(within(dialog).queryByRole('button', { name: 'Save' })).toBeNull();
        api.previewMySkill.mockResolvedValue(ok(preview()));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        await within(dialog).findByRole('button', { name: 'Save' });
        expect(api.previewMySkill).toHaveBeenLastCalledWith({ package: { skillMd: '---\nname: new-skill\n---' }, aiScan: false });
        expect(within(dialog).queryByText(/scan is unavailable/)).toBeNull();
    });

    it('uploads a zip package as base64', async () => {
        render(<AssistantSkillsSettings scope="personal" />);
        await screen.findByText('log-triage');
        fireEvent.click(screen.getByRole('button', { name: 'Import' }));
        const dialog = await screen.findByRole('dialog');
        const bytes = new Uint8Array([80, 75, 3, 4, 255]);
        fireEvent.change(within(dialog).getByTestId('skill-zip-input'), { target: { files: [new File([bytes], 'skill.zip')] } });
        api.previewMySkill.mockResolvedValue(ok(preview({ scan: { verdict: 'clean', findings: [] } })));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        expect(await within(dialog).findByText('The AI security scan found no issues.')).toBeInTheDocument();
        expect(api.previewMySkill).toHaveBeenCalledWith({ package: { zipBase64: btoa(String.fromCharCode(...bytes)) }, aiScan: true });
    });

    it('previews the server diff before saving an online edit as a new version', async () => {
        api.getMySkill.mockResolvedValue(ok(detail));
        render(<AssistantSkillsSettings scope="personal" />);
        fireEvent.click(await screen.findByRole('button', { name: 'log-triage' }));
        let dialog = await screen.findByRole('dialog');
        expect(await within(dialog).findByText('MIT')).toBeInTheDocument();
        expect(within(dialog).getByText('team')).toBeInTheDocument();
        fireEvent.click(within(dialog).getByRole('button', { name: 'Edit online' }));
        dialog = await screen.findByRole('dialog', { name: 'Edit log-triage' });
        expect(within(dialog).getByRole('switch', { name: 'Scan with AI for security issues' })).not.toBeChecked();
        fireEvent.change(within(dialog).getByLabelText('references/a.md'), { target: { value: 'new' } });
        fireEvent.change(within(dialog).getByLabelText('New file path'), { target: { value: 'scripts/run.sh' } });
        fireEvent.click(within(dialog).getByRole('button', { name: 'Add file' }));
        api.previewMySkillVersion.mockResolvedValue(ok(preview({ skipped: [], diff: [{ path: 'references/a.md', change: 'modified', truncated: false,
            hunks: [{ oldStart: 1, newStart: 1, lines: [{ op: 'remove', text: 'old' }, { op: 'add', text: 'new' }] }] }] })));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Review changes' }));
        const diff = await within(dialog).findByRole('group', { name: 'references/a.md' });
        expect(diff.querySelector('[data-op="remove"]')).toHaveTextContent('old');
        expect(diff.querySelector('[data-op="add"]')).toHaveTextContent('new');
        const pkg = { skillMd: detail.skillMd, files: [{ path: 'references/a.md', text: 'new' }, { path: 'scripts/run.sh', text: '' }] };
        expect(api.previewMySkillVersion).toHaveBeenCalledWith('s1', { package: pkg, aiScan: false });
        api.updateMySkill.mockResolvedValue(ok({ skill: { ...summary, origin: 'edited', revision: 8 }, files: [], skipped: [], warnings: [] }));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(api.updateMySkill).toHaveBeenCalledWith('s1', { expectedRevision: 7, package: pkg, origin: 'edited' }));
        expect(await screen.findByText('Edited online')).toBeInTheDocument();
    });

    it('asks to reload after a revision conflict', async () => {
        render(<AssistantSkillsSettings scope="personal" />);
        await screen.findByText('log-triage');
        api.updateMySkill.mockRejectedValue(new RestResponseError('stale', deskErrorCodeEnum.REVISION_CONFLICT, null));
        fireEvent.click(screen.getByRole('switch', { name: 'Enable log-triage' }));
        const alert = await screen.findByRole('alert');
        expect(alert).toHaveTextContent('This skill was changed elsewhere. Reload and try again.');
        expect(api.updateMySkill).toHaveBeenCalledWith('s1', { expectedRevision: 7, enabled: false });
        fireEvent.click(within(alert).getByRole('button', { name: 'Reload' }));
        await waitFor(() => expect(api.listMySkills).toHaveBeenCalledTimes(2));
    });

    it('shows the server reason when a package is rejected', async () => {
        const dialog = await startImport();
        api.previewMySkill.mockRejectedValue(new RestResponseError('name must match the directory', deskErrorCodeEnum.INVALID_PARAMS, null));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        expect(await within(dialog).findByRole('alert')).toHaveTextContent('name must match the directory');
    });

    it('deletes a skill only after confirmation', async () => {
        render(<AssistantSkillsSettings scope="personal" />);
        api.getMySkill.mockResolvedValue(ok(detail));
        fireEvent.click(await screen.findByRole('button', { name: 'log-triage' }));
        const dialog = await screen.findByRole('dialog');
        fireEvent.click(await within(dialog).findByRole('button', { name: 'Delete' }));
        const confirm = await screen.findByRole('alertdialog');
        expect(api.deleteMySkill).not.toHaveBeenCalled();
        api.deleteMySkill.mockResolvedValue(ok('s1'));
        api.listMySkills.mockResolvedValue(ok([]));
        fireEvent.click(within(confirm).getByRole('button', { name: 'Delete' }));
        await waitFor(() => expect(api.deleteMySkill).toHaveBeenCalledWith('s1', { expectedRevision: 7 }));
        expect(await screen.findByText('No skills yet.')).toBeInTheDocument();
    });

    it('lists bundled skills on the platform page with only an enable switch', async () => {
        render(<AssistantSkillsSettings scope="platform" />);
        expect(await screen.findByText('issue-analysis')).toBeInTheDocument();
        expect(screen.getByText(/including work from issue subscriptions/)).toBeInTheDocument();
        expect(screen.getByText('Bundled')).toBeInTheDocument();
        api.setBundledSkill.mockResolvedValue(ok({ revision: 4, skills: [
            { name: 'issue-analysis', description: 'Analyse issues', enabled: false, skillMd: '---', files: [] },
        ] }));
        const toggle = screen.getByRole('switch', { name: 'Enable issue-analysis' });
        fireEvent.click(toggle);
        await waitFor(() => expect(api.setBundledSkill).toHaveBeenCalledWith('issue-analysis', { expectedRevision: 3, enabled: false }));
        await waitFor(() => expect(toggle).not.toBeChecked());
        fireEvent.click(screen.getByRole('button', { name: 'issue-analysis' }));
        const dialog = await screen.findByRole('dialog');
        expect(within(dialog).queryByRole('button', { name: 'Edit online' })).toBeNull();
        expect(within(dialog).queryByRole('button', { name: 'Delete' })).toBeNull();
    });

    it('disables the scan switch with a reason when no approval model can scan', async () => {
        api.getMySkillScan.mockResolvedValue(ok({ available: false }));
        const dialog = await startImport();
        const scan = within(dialog).getByRole('switch', { name: 'Scan with AI for security issues' });
        await waitFor(() => expect(scan).toBeDisabled());
        expect(scan).not.toBeChecked();
        expect(within(dialog).getByText('The approval model is unavailable, so uploads cannot be scanned.')).toBeInTheDocument();
        api.previewMySkill.mockResolvedValue(ok(preview()));
        fireEvent.click(within(dialog).getByRole('button', { name: 'Check' }));
        await within(dialog).findByRole('button', { name: 'Save' });
        expect(api.previewMySkill).toHaveBeenCalledWith({ package: { skillMd: '---\nname: new-skill\n---' }, aiScan: false });
    });

    it('shows each version\'s scan and an older version\'s content with its diff to the current one', async () => {
        api.getMySkill.mockResolvedValue(ok(detail));
        render(<AssistantSkillsSettings scope="personal" />);
        fireEvent.click(await screen.findByRole('button', { name: 'log-triage' }));
        const dialog = await screen.findByRole('dialog');
        expect(await within(dialog).findByText('Scan found suspicious content')).toBeInTheDocument();
        expect(within(dialog).queryByRole('button', { name: 'v2' })).toBeNull();
        api.getMySkillVersion.mockResolvedValue(ok({
            info: detail.versions[1], skillMd: '---\nname: log-triage\n---\nOld body.', files: [],
            diffToCurrent: [{ path: 'SKILL.md', change: 'modified', truncated: false,
                hunks: [{ oldStart: 4, newStart: 4, lines: [{ op: 'remove', text: 'Old body.' }, { op: 'add', text: 'Read logs.' }] }] }],
        }));
        fireEvent.click(within(dialog).getByRole('button', { name: 'v1' }));
        const version = await within(dialog).findByRole('region', { name: 'v1' });
        expect(api.getMySkillVersion).toHaveBeenCalledWith('s1', 1);
        expect(within(version).getByText(/asks to skip approval/)).toBeInTheDocument();
        const diff = within(version).getByRole('group', { name: 'SKILL.md' });
        expect(diff.querySelector('[data-op="add"]')).toHaveTextContent('Read logs.');
        expect(within(version).getAllByText(/Old body\./).length).toBeGreaterThan(0);
    });

    it('lists pending AI skill proposals on the personal page and reloads after a decision', async () => {
        const proposal = { proposalId: 'p1', conversationId: 'c1', kind: 'create', name: 'disk-cleanup', reason: 'Repeated task',
            status: 'pending', skillMd: '---', files: [], diff: [], createdAtMs: 0, expiresAtMs: 1, revision: 2 };
        api.listAiAssistantSkillProposals.mockResolvedValue(ok([proposal, { ...proposal, proposalId: 'p0', name: 'old', status: 'rejected' }]));
        render(<AssistantSkillsSettings scope="personal" />);
        const section = await screen.findByRole('region', { name: 'Skill proposals awaiting your decision' });
        expect(within(section).getByText('disk-cleanup')).toBeInTheDocument();
        expect(within(section).queryByText('old')).toBeNull();
        api.decideAiAssistantSkillProposal.mockResolvedValue(ok({ ...proposal, status: 'approved', revision: 3 }));
        api.listAiAssistantSkillProposals.mockResolvedValue(ok([]));
        fireEvent.click(within(section).getByRole('button', { name: 'Approve' }));
        await waitFor(() => expect(api.listMySkills).toHaveBeenCalledTimes(2));
        await waitFor(() => expect(screen.queryByRole('region', { name: 'Skill proposals awaiting your decision' })).toBeNull());
    });
});
