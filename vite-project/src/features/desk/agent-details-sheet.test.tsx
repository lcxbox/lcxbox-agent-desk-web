import { useState } from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentDetailsSheet, type AgentPanelId } from './agent-details-sheet';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

function Fixture() {
    const [panel, setPanel] = useState<AgentPanelId | null>(null);
    return <><button onClick={() => setPanel('details')}>open</button><AgentDetailsSheet panel={panel} onPanelChange={setPanel}
        sections={{ details: <p>activity-only</p>, capabilities: <p>complete-inventory</p>, context: <p>selection-only</p>, connection: <p>local-settings</p>, observation: <p>manual-observation</p> }} /></>;
}

describe('agent detail navigation', () => {
    it('does not mount auxiliary panels in the conversation and exposes the full inventory from details', () => {
        render(<Fixture />);
        expect(screen.queryByRole('dialog')).toBeNull();
        expect(screen.queryByText('complete-inventory')).toBeNull();
        fireEvent.click(screen.getByText('open'));
        expect(screen.getByText('activity-only')).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAgent.workspace.capabilities' }));
        expect(screen.getByText('complete-inventory')).toBeTruthy();
        expect(screen.queryByText('selection-only')).toBeNull();
        expect(screen.queryByText('manual-observation')).toBeNull();
    });
});
