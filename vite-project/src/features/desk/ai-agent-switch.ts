type AiAgentProjection = {
    enabled?: boolean | null;
};

/** Only an explicit device settings projection enables AI Agent. */
export function isAiAgentEnabled(settings?: AiAgentProjection | null): boolean {
    return settings?.enabled === true;
}
