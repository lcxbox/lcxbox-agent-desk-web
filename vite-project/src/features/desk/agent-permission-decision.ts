import type { GrantRequestItemDto, PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { isReusableWebHost } from './agent-permission-labels';
import { isOutputInput, outputApprovalBlocked } from './ai-agent-output-confirmation';
import { validCommandReview } from './ai-agent-command';
import { validLaunchReview } from './ai-agent-launch';
import { fileApprovalBlocked } from './ai-agent-file-confirmation';

export type PermissionItemEdit = {
    resourceScope?: string[];
    operationScope?: string[];
    exportDestinationIndexes?: number[];
    ttlSeconds?: number;
    maxUses?: number;
};

export function needsApplicationScope(tool: string) {
    return ['execute_ui_actions', 'send_background_input'].includes(tool);
}

export function hasNativeUiAuthority(item: GrantRequestItemDto) {
    return !needsApplicationScope(item.toolName)
        || Boolean(item.applicationScope);
}

/** True when the card cannot approve this item, so a submission denies it. */
export function permissionItemApprovalBlocked(item: GrantRequestItemDto) {
    return (item.expectedEffect === 'send_external' && !item.externalSendConfirmation)
        || (item.toolName === 'exec_command' && !validCommandReview(item.commandConfirmation))
        || (item.toolName === 'launch_application' && !validLaunchReview(item.launchConfirmation))
        || !hasNativeUiAuthority(item)
        || fileApprovalBlocked(item)
        || outputApprovalBlocked(item);
}

/** Items a card selects before the owner changes anything. */
export function defaultApprovedItemIds(request: PermissionRequestDto) {
    return request.items
        .filter((item) => !permissionItemApprovalBlocked(item))
        .map((item) => item.itemId);
}

/**
 * The decision items a card's approve button sends: selected, approvable
 * items are approved with their (possibly narrowed) scope, every other item
 * is denied in the same batch.
 */
export function permissionDecisionItems(
    request: PermissionRequestDto,
    selected: string[] = defaultApprovedItemIds(request),
    edits: Record<string, PermissionItemEdit> = {},
): PermissionDecisionBody['items'] {
    return request.items.map((item) => {
        if (!selected.includes(item.itemId) || permissionItemApprovalBlocked(item)) {
            return { itemId: item.itemId, decision: 'deny' as const };
        }
        const edit = edits[item.itemId] ?? {};
        const destinationIndexes = edit.exportDestinationIndexes
            ?? item.exportDestinations.map((_, index) => index);
        return {
            itemId: item.itemId,
            decision: 'approve' as const,
            resource_scope: edit.resourceScope ?? item.resourceScope,
            operation_scope: edit.operationScope ?? item.operationScope,
            export_destinations: item.exportDestinations.filter((_, index) =>
                destinationIndexes.includes(index)),
            ttl_seconds: edit.ttlSeconds ?? item.suggestedTtlSeconds,
            max_uses: isReusableWebHost(item) || item.expectedEffect === 'send_external' || isOutputInput(item)
                ? 1
                : (edit.maxUses ?? item.suggestedMaxUses),
        };
    });
}

export function permissionDenyItems(request: PermissionRequestDto): PermissionDecisionBody['items'] {
    return request.items.map((item) => ({ itemId: item.itemId, decision: 'deny' as const }));
}
