import type { PermissionDecisionBody, PermissionRequestDto } from '@/services/types';
import { interactiveCommandItem } from './agent-interactive-command';
import { permissionDecisionItems, permissionDenyItems } from './agent-permission-decision';

/** A pending request that approval AI decides; the owner does not decide it. */
export function permissionIsAutomaticallyReviewed(request: PermissionRequestDto, automaticApproval: boolean) {
    return automaticApproval && request.state === 'pending'
        && !request.items.some(item => Boolean(item.commandConfirmation?.interactive));
}

/** The requests one model step created together, in execution order. */
export type PermissionGroup = { groupId: string; size: number; members: PermissionRequestDto[] };

export type PermissionEntry =
    | { kind: 'request'; request: PermissionRequestDto }
    | { kind: 'group'; group: PermissionGroup };

/**
 * Collects the requests that share a group id into one entry placed where its
 * first member appears; requests without a group stay single entries.
 */
export function permissionEntries(requests: PermissionRequestDto[]): PermissionEntry[] {
    const entries: PermissionEntry[] = [];
    const groups = new Map<string, PermissionGroup>();
    for (const request of requests) {
        const group = request.group;
        if (!group) {
            entries.push({ kind: 'request', request });
            continue;
        }
        const current = groups.get(group.groupId);
        if (current) {
            current.members.push(request);
            current.size = Math.max(current.size, group.size);
            continue;
        }
        const created = { groupId: group.groupId, size: group.size, members: [request] };
        groups.set(group.groupId, created);
        entries.push({ kind: 'group', group: created });
    }
    for (const group of groups.values()) {
        group.members.sort((left, right) => (left.group?.order ?? 0) - (right.group?.order ?? 0));
        group.size = Math.max(group.size, group.members.length);
    }
    return entries;
}

const OPEN_STATES: PermissionRequestDto['state'][] = ['pending', 'needs_revalidation'];

export function permissionRequestOpen(request: PermissionRequestDto) {
    return OPEN_STATES.includes(request.state);
}

/** The step still waits for a decision on some card of the group. */
export function permissionGroupOpen(group: PermissionGroup) {
    return group.members.some(permissionRequestOpen);
}

export function permissionGroupDecidedCount(group: PermissionGroup) {
    return group.size - group.members.filter(member => member.state === 'pending').length;
}

/** Approved, but its grant starts only once every other card of the step is decided. */
export function permissionWaitsForGroup(request: PermissionRequestDto, group: PermissionGroup) {
    return ['approved', 'partially_approved'].includes(request.state)
        && group.members.some(member => member !== request && member.state === 'pending');
}

export type PermissionGroupPolicy = {
    automaticApproval: boolean;
    canDecide: (request: PermissionRequestDto) => boolean;
};

function ownerDecides(request: PermissionRequestDto, policy: PermissionGroupPolicy) {
    return request.state === 'pending' && policy.canDecide(request)
        && !permissionIsAutomaticallyReviewed(request, policy.automaticApproval);
}

/**
 * Approve all decides, in step order, every pending card the owner decides
 * now. Interactive commands need the owner's live terminal, so they are left
 * for their own card and returned separately.
 */
export function permissionGroupApproveAll(group: PermissionGroup, policy: PermissionGroupPolicy) {
    const owned = group.members.filter(member => ownerDecides(member, policy));
    return {
        send: owned.filter(member => !interactiveCommandItem(member)),
        individual: owned.filter(member => Boolean(interactiveCommandItem(member))),
    };
}

/** Deny all denies every pending card the owner decides now, interactive ones included. */
export function permissionGroupDenyAll(group: PermissionGroup, policy: PermissionGroupPolicy) {
    return group.members.filter(member => ownerDecides(member, policy));
}

export type PermissionDecide = (request: PermissionRequestDto, items: PermissionDecisionBody['items']) => Promise<boolean>;

/**
 * Sends the decisions one at a time, each after the previous one completed,
 * and stops at the first one that is rejected, fails or is superseded.
 */
export async function decidePermissionGroup(requests: PermissionRequestDto[], approve: boolean, decide: PermissionDecide) {
    const decided: string[] = [];
    for (const request of requests) {
        let accepted = false;
        try {
            accepted = await decide(request, approve ? permissionDecisionItems(request) : permissionDenyItems(request));
        } catch {
            accepted = false;
        }
        if (!accepted) return { decided, failed: request.requestId };
        decided.push(request.requestId);
    }
    return { decided, failed: null };
}
