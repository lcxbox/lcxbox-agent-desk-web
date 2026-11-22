import { useTranslation } from 'react-i18next';

// Kinds the host binds to exact content (command, plan, launch request, URL,
// query, selected files, application). The server recomputes them before the
// action runs, so they cannot be narrowed and mean nothing to a reader.
const FINGERPRINT_KINDS = ['command_input', 'command_plan', 'application_input', 'external_url_input',
    'external_query_input', 'selected', 'ui_application'];
const FINGERPRINT = /^[a-z_]+:sha256:[0-9a-f]+$/i;

/** Display only: a `<kind>:sha256:<digest>` resource scope binding a grant to the content shown. */
export function isBindingFingerprint(scope: string): boolean {
    return FINGERPRINT.test(scope) || FINGERPRINT_KINDS.some(kind => scope.startsWith(`${kind}:sha256:`));
}

/**
 * Splits resource scope for display. Submissions always carry the full scope:
 * hidden fingerprints are never deselected. An application fingerprint stays
 * visible where the card can name the application.
 */
export function splitBindingFingerprints(scopes: string[], applicationName?: string | null) {
    const hidden = (scope: string) => isBindingFingerprint(scope)
        && !(applicationName && scope.startsWith('ui_application:'));
    return { shown: scopes.filter(scope => !hidden(scope)), fingerprints: scopes.filter(hidden) };
}

/** One plain sentence in place of fingerprints, with the raw values collapsed. */
export function BindingFingerprintNote({ fingerprints, grant = false }: { fingerprints: string[]; grant?: boolean }) {
    const { t } = useTranslation();
    if (fingerprints.length === 0) return null;
    return <div className="mt-1 space-y-1 text-xs text-muted-foreground">
        <p>{t(grant ? 'pages.aiAgent.grantFingerprintNote' : 'pages.aiAgent.permissionFingerprintNote')}</p>
        {/* Native disclosure: a disabled fieldset around the card must not lock it. */}
        <details>
            <summary className="cursor-pointer">{t('pages.aiAgent.workspace.technicalDetails')}</summary>
            <ul className="mt-1 space-y-0.5">
                {fingerprints.map(scope => <li key={scope}><code className="break-all">{scope}</code></li>)}
            </ul>
        </details>
    </div>;
}
