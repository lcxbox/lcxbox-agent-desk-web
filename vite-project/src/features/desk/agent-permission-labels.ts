import type { TFunction } from 'i18next';

// Display-only translations: never use labels as submitted authorization values.
function label(t: TFunction, group: string, value: string): string {
    const key = `pages.aiAgent.${group}.${value}`;
    // Protocol values may contain ':' and '.'. Treat the entire flat locale key
    // literally instead of letting i18next parse namespaces or nested paths.
    const translated = t(key, { nsSeparator: false, keySeparator: false });
    return translated === key ? value : translated;
}

export const permissionToolLabel = (t: TFunction, value: string) => label(t, 'permissionTool', value);

// A shared effect or operation category can be too broad for one tool, e.g.
// text edits reuse the output-file category; a per-tool label takes precedence.
function toolSpecific(t: TFunction, group: string, toolName: string | undefined, value: string): string | null {
    if (!toolName) return null;
    const translated = label(t, group, `${toolName}.${value}`);
    return translated === `${toolName}.${value}` ? null : translated;
}

export const permissionEffectLabel = (t: TFunction, value: string, toolName?: string) =>
    toolSpecific(t, 'permissionToolEffect', toolName, value) ?? label(t, 'permissionEffect', value);

// Mirrors `SYNTHESIZED_REASON` in diagnose-core: the server's fallback when the
// proposing agent message has no visible text.
const SYNTHESIZED_REASON = "Requested by the agent's tool call.";

export const permissionReasonLabel = (t: TFunction, reason: string) =>
    reason === SYNTHESIZED_REASON ? t('pages.aiAgent.permissionSynthesizedReason') : reason;

export function isReusableWebHost(value: { toolName: string; resourceScope: string[] }): boolean {
    return value.toolName === 'fetch_public_web_page'
        && value.resourceScope.some((scope) => scope.startsWith('external_web_host:'));
}

export function permissionOperationLabel(t: TFunction, value: string, toolName?: string): string {
    const specific = toolSpecific(t, 'permissionToolOperation', toolName, value);
    if (specific) return specific;
    if (/^(ui|background_input):/.test(value)) {
        const key = `pages.aiAgent.uiAction_${value.slice(value.indexOf(':') + 1)}`;
        const translated = t(key, { nsSeparator: false, keySeparator: false });
        return translated === key ? value : translated;
    }
    const operation = label(t, 'permissionOperation', value);
    return operation === value ? permissionToolLabel(t, value) : operation;
}

export function permissionResourceLabel(t: TFunction, value: string, applicationName?: string | null): string {
    if (applicationName && value.startsWith('ui_application:')) return applicationName;
    const exact = label(t, 'permissionResource', value);
    if (exact !== value) return exact;
    const separator = value.indexOf(':');
    if (separator < 0) return value;
    const key = `pages.aiAgent.permissionResourceKind.${value.slice(0, separator)}`;
    // Keep the full identity/path: distinct resources must remain distinguishable.
    const translated = t(key, { value: value.slice(separator + 1), nsSeparator: false, keySeparator: false });
    return translated === key ? value : translated;
}
