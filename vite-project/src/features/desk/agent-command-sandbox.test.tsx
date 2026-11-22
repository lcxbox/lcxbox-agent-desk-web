import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import {
    CommandSandboxDiagnostic, CommandSandboxNotice, identityKey, sandboxDiagnostic, validCommandSandbox, type CommandSandbox,
} from './agent-command-sandbox';
import { AgentCommandResult, parseCommandReceipt } from './agent-command-result';
import { CommandConfirmationCard, validCommandReview } from './ai-agent-command';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string, values?: Record<string, unknown>) => values ? `${key}:${JSON.stringify(values)}` : key }) }));

const sandboxed: CommandSandbox = {
    mode: 'sandboxed', attention: 'sandboxed',
    writableRoots: [{ kind: 'cwd', path: '/work/repo' }, { kind: 'grant', path: '/home/u/docs' }],
    implicitWritable: ['exec_temp_dir', 'dev_shm'], identity: null,
};
const prominent: CommandSandbox = { mode: 'unrestricted', attention: 'prominent', writableRoots: [], implicitWritable: [],
    identity: { elevated: true, system: false, root: false } };
const neutral: CommandSandbox = { mode: 'unrestricted', attention: 'neutral', writableRoots: [], implicitWritable: [], identity: null };

describe('command sandbox block', () => {
    it('accepts consistent blocks and rejects missing or contradictory ones', () => {
        for (const valid of [sandboxed, prominent, neutral, { ...sandboxed, writableRoots: [], implicitWritable: ['exec_temp_dir'] },
            { ...sandboxed, implicitWritable: ['exec_temp_dir', 'low_integrity_locations', 'other_sandbox_roots'] },
            { ...neutral, identity: { elevated: false, system: false, root: true } }]) {
            expect(validCommandSandbox(valid)).toBe(true);
        }
        for (const invalid of [undefined, null, {}, [],
            { ...sandboxed, attention: 'neutral' },
            { ...sandboxed, implicitWritable: ['dev_shm'] },
            { ...sandboxed, implicitWritable: [] },
            { ...sandboxed, implicitWritable: ['exec_temp_dir', 'home'] },
            { ...sandboxed, identity: prominent.identity },
            { ...sandboxed, writableRoots: [{ kind: 'other', path: '/x' }] },
            { ...sandboxed, writableRoots: [{ kind: 'cwd', path: '' }] },
            { ...neutral, attention: 'sandboxed' },
            { ...neutral, implicitWritable: ['exec_temp_dir'] },
            { ...prominent, writableRoots: sandboxed.writableRoots },
            { ...prominent, identity: { elevated: false, system: false, root: false } },
            { ...prominent, identity: { elevated: 'yes', system: false, root: false } },
            { ...neutral, mode: 'other' }]) {
            expect(validCommandSandbox(invalid)).toBe(false);
        }
    });

    it('blocks the command review when the sandbox block is missing, invalid or off its snapshot roots', () => {
        const snapshot = { roots: sandboxed.writableRoots, excluded_names: [], honors_ignore_files: true, max_files: 1, max_file_bytes: 1,
            scan_time_limit_ms: 1, estimated_file_count: null, unprotected: [] };
        const review = { command: 'make', shell: 'bash', cwd: '/work/repo', targetDeviceId: 'd', targetSessionId: 's', timeoutMs: 1000,
            maxStdoutBytes: 1, maxStderrBytes: 1, executionBasis: 'owner_blocklist_only', oneShot: true, snapshot };
        expect(validCommandReview({ ...review, sandbox: sandboxed })).toBe(true);
        expect(validCommandReview({ ...review, snapshot: null, sandbox: { ...sandboxed, writableRoots: [] } })).toBe(true);
        expect(validCommandReview({ ...review, sandbox: prominent })).toBe(true);
        expect(validCommandReview(review)).toBe(false);
        expect(validCommandReview({ ...review, sandbox: { ...sandboxed, mode: 'unrestricted' } })).toBe(false);
        expect(validCommandReview({ ...review, sandbox: { ...sandboxed, writableRoots: [sandboxed.writableRoots[0]] } })).toBe(false);
        expect(validCommandReview({ ...review, snapshot: null, sandbox: sandboxed })).toBe(false);
        expect(validCommandReview({ ...review, sandbox: { ...sandboxed,
            writableRoots: [sandboxed.writableRoots[0], { kind: 'grant', path: '/elsewhere' }] } })).toBe(false);
    });

    it('lists writable roots and implicit locations of a sandboxed command', () => {
        const { container } = render(<CommandSandboxNotice value={sandboxed} />);
        const text = container.textContent ?? '';
        expect(text).toContain('commandSandbox.title');
        expect(text).toContain('commandSandbox.sandboxed');
        expect(text).toContain('commandSnapshot.root.cwd:{"path":"/work/repo"}');
        expect(text).toContain('commandSnapshot.root.grant:{"path":"/home/u/docs"}');
        expect(text).toContain('commandSandbox.implicit.exec_temp_dir');
        expect(text).toContain('commandSandbox.implicit.dev_shm');
        expect(text).toContain('commandSandbox.readsUnrestricted');
        expect(screen.queryByRole('alert')).toBeNull();
    });

    it('warns prominently with the identity, and marks neutral commands quietly', () => {
        const strong = render(<CommandSandboxNotice value={prominent} />);
        expect(screen.getByRole('alert').textContent).toContain('commandSandbox.prominent');
        expect(screen.getByTestId('command-sandbox-identity').textContent).toBe('pages.aiAgent.commandSandbox.identity.administrator');
        strong.unmount();

        const quiet = render(<CommandSandboxNotice value={{ ...neutral, identity: { elevated: false, system: false, root: true } }} />);
        expect(screen.queryByRole('alert')).toBeNull();
        expect(quiet.container.textContent).toContain('commandSandbox.neutral');
        expect(screen.getByTestId('command-sandbox-identity').textContent).toBe('pages.aiAgent.commandSandbox.identity.root');
        quiet.unmount();

        render(<CommandSandboxNotice value={neutral} />);
        expect(screen.queryByTestId('command-sandbox-identity')).toBeNull();
    });

    it('names SYSTEM and root ahead of an elevated token', () => {
        expect(identityKey({ elevated: true, system: true, root: false })).toBe('system');
        expect(identityKey({ elevated: false, system: false, root: true })).toBe('root');
        expect(identityKey({ elevated: true, system: false, root: false })).toBe('administrator');
    });

    it('shows the sandbox block on the command card', () => {
        const review = { command: 'make', shell: 'bash', cwd: null, targetDeviceId: 'd', targetSessionId: 's', timeoutMs: 1000,
            maxStdoutBytes: 1, maxStderrBytes: 1, executionBasis: 'template', oneShot: true, sandbox: prominent } as const;
        render(<CommandConfirmationCard value={review as never} />);
        expect(screen.getByTestId('command-sandbox')).toBeTruthy();
        expect(screen.getByRole('alert').textContent).toContain('commandSandbox.prominent');
    });
});

describe('command sandbox result notes', () => {
    const hint = { stage: 'environment', operation: 'run sandboxed command', domain: 'exec_sandbox', code: null, name: null, message: 'x' };
    const refused = { ...hint, operation: 'prepare command sandbox' };
    const other = { ...hint, domain: 'application' };
    const exec = (diagnostics: unknown[]) => JSON.stringify({ Exec: {
        started: true, exit_code: 126, termination_signal: null, failure: null, diagnostics, duration_ms: 5, redactions: [],
        streams: { type: 'split', stdout: '', stderr: 'ps: Operation not permitted', stdout_truncated: false, stderr_truncated: false },
    } });

    it('reads only exec_sandbox diagnostics and tells the hint from a refusal by the failure', () => {
        const failure = { kind: 'unsupported_capability', message: 'Command was not started' };
        expect(sandboxDiagnostic([other, hint], null)).toBe('unavailable_program');
        expect(sandboxDiagnostic([refused], failure)).toBe('refused');
        expect(sandboxDiagnostic(undefined, null)).toBeNull();
        expect(sandboxDiagnostic([null, 'x', other], null)).toBeNull();
        // A Windows device still preparing the write directory: retryable.
        const preparing = { ...hint, operation: 'prepare command sandbox', domain: 'exec_sandbox_preparing' };
        expect(sandboxDiagnostic([preparing], { kind: 'host_at_capacity', message: 'x' })).toBe('preparing');
        expect(parseCommandReceipt(exec([other, hint]))?.sandboxNote).toBe('unavailable_program');
        expect(parseCommandReceipt(exec([]))?.sandboxNote).toBeNull();
    });

    it('shows the run-after hint in the command result, and nothing without it', () => {
        const { unmount } = render(<AgentCommandResult text={exec([hint])} />);
        expect(screen.getByTestId('command-sandbox-diagnostic').textContent).toBe('pages.aiAgent.commandSandbox.diagnostic.unavailable_program');
        unmount();
        render(<AgentCommandResult text={exec([other])} />);
        expect(screen.queryByTestId('command-sandbox-diagnostic')).toBeNull();
    });

    it('renders a refusal note', () => {
        render(<CommandSandboxDiagnostic value="refused" />);
        expect(screen.getByTestId('command-sandbox-diagnostic').textContent).toBe('pages.aiAgent.commandSandbox.diagnostic.refused');
    });
});
