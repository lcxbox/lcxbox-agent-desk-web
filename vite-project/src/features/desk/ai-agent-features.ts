export type AiAgentFeatureProfile = {
    schema_version: number;
    turn_stream: boolean;
    capability_inventory: boolean;
    full_session_snapshot: boolean;
    permission_decision: boolean;
    approval_delegation: boolean;
    grant_revoke: boolean;
    background_task_cancel: boolean;
    object_context: boolean;
    exec_pty: boolean;
    device_settings: boolean;
};

export function hasAiAgentBrowserEntry(
    profile: AiAgentFeatureProfile | null | undefined,
): profile is AiAgentFeatureProfile {
    return Boolean(
        profile
        && profile.turn_stream
        && profile.capability_inventory
        && profile.full_session_snapshot,
    );
}
