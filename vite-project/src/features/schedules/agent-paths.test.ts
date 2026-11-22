import { describe, expect, it } from 'vitest';
import type { ConnectionModel } from '@/services/types';
import { agentPaths, agentConnections, scheduleDevices } from './agent-paths';

const connection = (id: string, device = 'stable-client') => ({ connection_id: id, device_id: device, version_info: null, peer: { remote_desk_type:'server', operation_system:'Other', display_name:null }, exec_capabilities: { available_exec_shells: [], exec_pty:false, exec_pty_elevation:false, max_ai_command_runtime_ms:null }, ai_agent_settings: null } as ConnectionModel);
describe('guided run device routing', () => {
    it('uses the Manager public handle and uses it for stable navigation', () => {
        expect(agentPaths([connection('live/connection', 'public-device')])).toEqual({ 'public-device': '/desk/device/public-device/agent' });
    });
    it('provides raw connection IDs without decoding agent URLs and drops ambiguous devices', () => {
        expect(agentConnections([connection('live/connection', 'device')])).toEqual({ device: 'live/connection' });
        expect(agentConnections([connection('one'), connection('two'), connection('three')])).toEqual({});
        expect(agentConnections([connection('', 'device')])).toEqual({});
    });
    it('keeps OSS identity stable across reconnects and refuses ambiguous connections', () => {
        expect(agentPaths([connection('new-live')])).toEqual({ 'stable-client': '/desk/device/stable-client/agent' });
        expect(agentPaths([connection('one'), connection('two'), connection('three')])).toEqual({});
    });
});
describe('schedule device names', () => {
    it('shows the name the host reports and falls back to its identity', () => {
        const named = { ...connection('live', 'client-uuid'), peer: { remote_desk_type: 'server', operation_system: 'Other', display_name: 'Office Mac' } } as ConnectionModel;
        const unnamed = connection('other', 'bare-uuid');
        expect(scheduleDevices([named, unnamed], [named, unnamed]).map(device => [device.id, device.name])).toEqual([['client-uuid', 'Office Mac'], ['bare-uuid', 'bare-uuid']]);
    });
});
