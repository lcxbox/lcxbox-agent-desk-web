import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { AlertTriangle } from 'lucide-react';

type RootKind = 'cwd' | 'grant';
type ImplicitWritable = 'exec_temp_dir' | 'user_temp_dir' | 'dev_shm';
const IMPLICIT_WRITABLE: readonly string[] = ['exec_temp_dir', 'user_temp_dir', 'dev_shm'];

/** Account an unrestricted command runs as, present only when notable. */
export interface CommandIdentity {
    elevated: boolean;
    system: boolean;
    root: boolean;
}

/** Frozen execution mode of a pending AI command (`CommandSandboxDto`). */
export interface CommandSandbox {
    mode: 'sandboxed' | 'unrestricted';
    attention: 'sandboxed' | 'neutral' | 'prominent';
    writableRoots: { kind: RootKind; path: string }[];
    implicitWritable: ImplicitWritable[];
    identity?: CommandIdentity | null;
}

const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);

function validIdentity(value: unknown): value is CommandIdentity {
    return record(value) && typeof value.elevated === 'boolean' && typeof value.system === 'boolean' && typeof value.root === 'boolean'
        && (value.elevated || value.system || value.root);
}

/**
 * A sandbox block consistent with the frozen notice: a sandboxed command lists
 * its per-execution temporary directory first and shows no identity; an
 * unrestricted one carries no writable locations.
 */
export function validCommandSandbox(input: unknown): input is CommandSandbox {
    if (!record(input) || !Array.isArray(input.writableRoots) || !Array.isArray(input.implicitWritable)) return false;
    const roots = input.writableRoots.every(root => record(root) && (root.kind === 'cwd' || root.kind === 'grant')
        && typeof root.path === 'string' && root.path.length > 0);
    const implicit = input.implicitWritable.every(location => IMPLICIT_WRITABLE.includes(location as string));
    if (!roots || !implicit || (input.identity != null && !validIdentity(input.identity))) return false;
    if (input.mode === 'sandboxed') {
        return input.attention === 'sandboxed' && input.implicitWritable[0] === 'exec_temp_dir' && input.identity == null;
    }
    return input.mode === 'unrestricted' && (input.attention === 'neutral' || input.attention === 'prominent')
        && input.writableRoots.length === 0 && input.implicitWritable.length === 0;
}

/** The single identity label: SYSTEM and root outrank an elevated token. */
export function identityKey(identity: CommandIdentity): 'system' | 'root' | 'administrator' {
    return identity.system ? 'system' : identity.root ? 'root' : 'administrator';
}

const identityLine = (t: TFunction, identity?: CommandIdentity | null) => identity
    ? <p data-testid="command-sandbox-identity" className="font-medium text-red-700 dark:text-red-300">{t(`pages.aiAssistant.commandSandbox.identity.${identityKey(identity)}`)}</p>
    : null;

export function CommandSandboxNotice({ value }: { value: CommandSandbox }) {
    const { t } = useTranslation();
    if (value.mode === 'sandboxed') {
        return <div data-testid="command-sandbox" className="space-y-1 rounded-md border p-2">
            <p className="font-medium">{t('pages.aiAssistant.commandSandbox.title')}</p>
            <p>{t('pages.aiAssistant.commandSandbox.sandboxed')}</p>
            <ul className="break-all">
                {value.writableRoots.map((root, index) => <li key={index} className="font-mono">
                    {t(`pages.aiAssistant.commandSnapshot.root.${root.kind}`, { path: root.path })}</li>)}
                {value.implicitWritable.map(location => <li key={location}>{t(`pages.aiAssistant.commandSandbox.implicit.${location}`)}</li>)}
            </ul>
            <p className="text-muted-foreground">{t('pages.aiAssistant.commandSandbox.readsUnrestricted')}</p>
        </div>;
    }
    if (value.attention === 'prominent') {
        return <div data-testid="command-sandbox" className="space-y-1">
            <p role="alert" className="flex items-start gap-2 rounded-md border border-red-500/50 bg-red-500/5 p-2 font-semibold text-red-700 dark:text-red-300">
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />{t('pages.aiAssistant.commandSandbox.prominent')}
            </p>
            {identityLine(t, value.identity)}
        </div>;
    }
    return <div data-testid="command-sandbox" className="space-y-1">
        <p className="text-muted-foreground">{t('pages.aiAssistant.commandSandbox.neutral')}</p>
        {identityLine(t, value.identity)}
    </div>;
}

/** What an `exec_sandbox` device diagnostic in a command result says. */
export type SandboxDiagnostic = 'unavailable_program' | 'refused';

/**
 * The command-sandbox note of an `ExecOutput`: an `exec_sandbox` diagnostic on
 * a result without a failure is the run-after hint that a program is
 * unavailable inside the sandbox; with a failure the device refused to start
 * the command in the sandbox.
 */
export function sandboxDiagnostic(diagnostics: unknown, failure: unknown): SandboxDiagnostic | null {
    if (!Array.isArray(diagnostics) || !diagnostics.some(entry => record(entry) && entry.domain === 'exec_sandbox')) return null;
    return failure == null ? 'unavailable_program' : 'refused';
}

/** A sandboxed command's writable roots are exactly its snapshot roots. */
export function sandboxMatchesSnapshot(sandbox: CommandSandbox, snapshotRoots: { kind: string; path: string }[] | undefined): boolean {
    if (sandbox.mode !== 'sandboxed') return true;
    const roots = snapshotRoots ?? [];
    return roots.length === sandbox.writableRoots.length
        && roots.every((root, index) => root.kind === sandbox.writableRoots[index].kind && root.path === sandbox.writableRoots[index].path);
}

export function CommandSandboxDiagnostic({ value }: { value: SandboxDiagnostic }) {
    const { t } = useTranslation();
    return <p data-testid="command-sandbox-diagnostic"
        className="flex items-start gap-2 rounded-md border border-amber-500/50 bg-amber-500/5 p-2 text-amber-800 dark:text-amber-200">
        <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />{t(`pages.aiAssistant.commandSandbox.diagnostic.${value}`)}
    </p>;
}
