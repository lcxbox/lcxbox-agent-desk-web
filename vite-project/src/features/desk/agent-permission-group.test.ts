import { describe, expect, it, vi } from 'vitest';
import type { GrantRequestItemDto, PermissionRequestDto } from '@/services/types';
import {
    decidePermissionGroup, permissionEntries, permissionGroupApproveAll, permissionGroupDecidedCount, permissionGroupDenyAll,
    permissionWaitsForGroup, type PermissionGroup,
} from './agent-permission-group';
import { permissionDecisionItems } from './agent-permission-decision';

const item = (overrides: Partial<GrantRequestItemDto> = {}): GrantRequestItemDto => ({
    itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session',
    reason: 'Inspect selected device', expectedEffect: 'read_device', resourceScope: ['target:a'],
    operationScope: ['observe'], exportDestinations: [], suggestedMaxUses: 2, suggestedTtlSeconds: 300,
    ...overrides,
});
const request = (requestId: string, state: PermissionRequestDto['state'], order?: number, items = [item()]): PermissionRequestDto => ({
    schemaVersion: 1, requestId, inputRevision: 1, createdAt: '2026-11-22T00:00:00Z', state, items,
    ...(order === undefined ? {} : { group: { groupId: 'g', order, size: 3 } }),
});
const interactive = (requestId: string, order: number) => request(requestId, 'pending', order, [item({
    itemId: 'pty', toolName: 'exec_command', expectedEffect: 'execute_command',
    commandConfirmation: { interactive: {} } as GrantRequestItemDto['commandConfirmation'],
})]);
const group = (members: PermissionRequestDto[]): PermissionGroup => ({ groupId: 'g', size: members.length, members });
const owner = { automaticApproval: false, canDecide: () => true };

describe('permission card groups', () => {
    it('groups members in execution order at the first member and keeps lone requests as they are', () => {
        const entries = permissionEntries([
            request('lone-before', 'approved'), request('third', 'pending', 2), request('first', 'pending', 0),
            request('lone-between', 'pending'), request('second', 'approved', 1),
        ]);
        expect(entries.map(entry => entry.kind === 'group' ? entry.group.members.map(member => member.requestId) : entry.request.requestId))
            .toEqual(['lone-before', ['first', 'second', 'third'], 'lone-between']);
        expect(entries[1]).toMatchObject({ kind: 'group', group: { groupId: 'g', size: 3 } });
    });

    it('counts decided cards and marks approved members that wait for the rest', () => {
        const approved = request('a', 'approved', 0);
        const partial = request('b', 'partially_approved', 1);
        const pending = request('c', 'pending', 2);
        const open = group([approved, partial, pending]);
        expect(permissionGroupDecidedCount(open)).toBe(2);
        expect(permissionWaitsForGroup(approved, open)).toBe(true);
        expect(permissionWaitsForGroup(partial, open)).toBe(true);
        expect(permissionWaitsForGroup(pending, open)).toBe(false);
        const settled = group([approved, partial, request('c', 'denied', 2)]);
        expect(permissionWaitsForGroup(approved, settled)).toBe(false);
        expect(permissionGroupDecidedCount(settled)).toBe(3);
    });

    it('approve all skips decided, AI-reviewed, undecidable and interactive members', () => {
        const members = [
            request('decided', 'approved', 0), request('manual', 'pending', 1), interactive('pty', 2),
            request('stale', 'pending', 3), request('revalidate', 'needs_revalidation', 4),
        ];
        const canDecide = (value: PermissionRequestDto) => value.requestId !== 'stale';
        const plan = permissionGroupApproveAll(group(members), { automaticApproval: false, canDecide });
        expect(plan.send.map(value => value.requestId)).toEqual(['manual']);
        expect(plan.individual.map(value => value.requestId)).toEqual(['pty']);
        const reviewed = permissionGroupApproveAll(group(members), { automaticApproval: true, canDecide });
        expect(reviewed.send).toEqual([]);
        expect(reviewed.individual.map(value => value.requestId)).toEqual(['pty']);
    });

    it('deny all includes interactive members but not AI-reviewed ones', () => {
        const members = [request('manual', 'pending', 0), interactive('pty', 1)];
        expect(permissionGroupDenyAll(group(members), owner).map(value => value.requestId)).toEqual(['manual', 'pty']);
        expect(permissionGroupDenyAll(group(members), { ...owner, automaticApproval: true }).map(value => value.requestId)).toEqual(['pty']);
    });

    it('sends each decision after the previous one finished, with the card default body', async () => {
        const blocked = item({ itemId: 'send', toolName: 'send_email', expectedEffect: 'send_external' });
        const members = [request('one', 'pending', 0), request('two', 'pending', 1, [item(), blocked])];
        const log: string[] = [];
        const decide = vi.fn(async (value: PermissionRequestDto) => {
            log.push(`start:${value.requestId}`);
            await Promise.resolve();
            log.push(`end:${value.requestId}`);
            return true;
        });
        await expect(decidePermissionGroup(members, true, decide)).resolves.toEqual({ decided: ['one', 'two'], failed: null });
        expect(log).toEqual(['start:one', 'end:one', 'start:two', 'end:two']);
        expect(decide.mock.calls[1][1]).toEqual(permissionDecisionItems(members[1]));
        expect(decide.mock.calls[1][1][1]).toEqual({ itemId: 'send', decision: 'deny' });
        expect(decide.mock.calls[0][1][0]).toMatchObject({ decision: 'approve', ttl_seconds: 300, max_uses: 2 });
    });

    it('denies every item when denying all', async () => {
        const decide = vi.fn().mockResolvedValue(true);
        await decidePermissionGroup([interactive('pty', 0)], false, decide);
        expect(decide).toHaveBeenCalledWith(expect.objectContaining({ requestId: 'pty' }), [{ itemId: 'pty', decision: 'deny' }]);
    });

    it('stops at the first rejected or failed decision and reports it', async () => {
        const members = [request('one', 'pending', 0), request('two', 'pending', 1), request('three', 'pending', 2)];
        const rejected = vi.fn().mockResolvedValueOnce(true).mockResolvedValueOnce(false).mockResolvedValue(true);
        await expect(decidePermissionGroup(members, true, rejected)).resolves.toEqual({ decided: ['one'], failed: 'two' });
        expect(rejected).toHaveBeenCalledTimes(2);
        const thrown = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(true);
        await expect(decidePermissionGroup(members, false, thrown)).resolves.toEqual({ decided: [], failed: 'one' });
        expect(thrown).toHaveBeenCalledTimes(1);
    });
});
