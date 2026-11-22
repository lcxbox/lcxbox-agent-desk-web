/** Descriptions are presentation-only; never use this key as an executable capability. */
export function capabilityDescriptionKey(displayNameKey: string): string {
    return displayNameKey.startsWith('agent.capability.')
        ? displayNameKey.replace('agent.capability.', 'agent.capabilityDescription.')
        : 'pages.aiAgent.workspace.descriptionUnavailable';
}
