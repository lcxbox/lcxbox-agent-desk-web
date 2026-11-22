/** The parts of the model provider query the composer reads. */
export type ModelProviderQueryState = {
    isPending: boolean;
    isSuccess: boolean;
    data?: { data?: { api_key_set?: boolean; model?: string | null } | null };
};

/**
 * Whether the composer must wait for a chat model. Only a host that answers the
 * device-local provider query can report a missing model; a host without that
 * query (the manager keeps models per account) leaves the check to the central,
 * which refuses the turn with its own error.
 */
export function agentModelMissing(query: ModelProviderQueryState): boolean {
    if (query.isPending) return true;
    if (!query.isSuccess) return false;
    const config = query.data?.data;
    return !config?.api_key_set || !config?.model;
}
