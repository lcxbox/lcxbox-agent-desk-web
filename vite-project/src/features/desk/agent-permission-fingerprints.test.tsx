import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { BindingFingerprintNote, isBindingFingerprint, splitBindingFingerprints } from './agent-permission-fingerprints';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const hex = 'c'.repeat(64);

describe('binding fingerprints', () => {
    it('recognizes content fingerprints and leaves readable scope alone', () => {
        for (const scope of [`command_input:sha256:${hex}`, `command_plan:sha256:${hex}`, `external_url_input:sha256:${hex}`,
            `future_kind:sha256:${hex}`, 'external_query_input:sha256:query', 'selected:sha256:abc']) {
            expect(isBindingFingerprint(scope)).toBe(true);
        }
        for (const scope of ['target:current_device', 'external_web_host:example.com', 'command:exact_input_required',
            `wayland_output:${hex}`, 'file:/tmp/sha256:abc', 'future_kind:sha256:not hex']) {
            expect(isBindingFingerprint(scope)).toBe(false);
        }
    });

    it('keeps an application fingerprint visible when the card can name the application', () => {
        const scopes = ['ui_application:sha256:opaque', `command_input:sha256:${hex}`, 'target:a'];
        expect(splitBindingFingerprints(scopes, 'Calendar')).toEqual({
            shown: ['ui_application:sha256:opaque', 'target:a'], fingerprints: [`command_input:sha256:${hex}`],
        });
        expect(splitBindingFingerprints(scopes).fingerprints).toEqual(['ui_application:sha256:opaque', `command_input:sha256:${hex}`]);
    });

    it('explains a grant in its own words and keeps the raw values collapsed', () => {
        render(<BindingFingerprintNote fingerprints={[`command_input:sha256:${hex}`]} grant />);
        expect(screen.getByText('pages.aiAgent.grantFingerprintNote')).toBeVisible();
        expect(screen.getByText(`command_input:sha256:${hex}`)).not.toBeVisible();
    });

    it('renders nothing without fingerprints', () => {
        const { container } = render(<BindingFingerprintNote fingerprints={[]} />);
        expect(container).toBeEmptyDOMElement();
    });
});
