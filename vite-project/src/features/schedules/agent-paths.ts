import { devicePath } from '@/features/desk/device-target-paths';
import type { ConnectionModel } from '@/services/types';

export function agentConnections(connections: ConnectionModel[]): Record<string, string> {
    const paths: Record<string, string> = Object.create(null);
    const seen = new Set<string>();
    for (const connection of connections) {
        const device = connection.device_id;
        if (!device || !connection.connection_id) continue;
        if (seen.has(device)) { delete paths[device]; continue; }
        seen.add(device);
        paths[device] = connection.connection_id;
    }
    return paths;
}

export function agentPaths(connections: ConnectionModel[]): Record<string, string> {
    return Object.fromEntries(Object.entries(agentConnections(connections))
        .map(([device]) => [device, devicePath(device, 'agent')]));
}

/** Online devices the owner can schedule, named as the host reports itself. */
export function scheduleDevices(connections: ConnectionModel[], online: ConnectionModel[]) {
    const paths = agentPaths(online);
    const connectionIds = agentConnections(online);
    return connections.flatMap(connection => {
        const id = connection.device_id;
        return id ? [{ id, name: connection.peer.display_name || id, connectionId: connectionIds[id], agentPath: paths[id] }] : [];
    });
}
