import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { DiffView } from './diff-view';

describe('DiffView', () => {
    it('numbers old and new lines separately from each hunk start', () => {
        render(<DiffView aria-label="SKILL.md" hunks={[{ oldStart: 4, newStart: 4, lines: [
            { op: 'context', text: 'keep' }, { op: 'remove', text: 'before' }, { op: 'add', text: 'after' }, { op: 'context', text: 'tail' },
        ] }]} />);
        const rows = [...screen.getByRole('group', { name: 'SKILL.md' }).querySelectorAll('tr')]
            .map(row => [...row.querySelectorAll('td')].map(cell => cell.textContent));
        expect(rows).toEqual([
            ['4', '4', ' ', 'keep'],
            ['5', '', '-', 'before'],
            ['', '5', '+', 'after'],
            ['6', '6', ' ', 'tail'],
        ]);
    });

    it('leaves the absent side unnumbered for added and removed files', () => {
        const { container } = render(<DiffView hunks={[{ oldStart: 0, newStart: 1, lines: [{ op: 'add', text: 'one' }, { op: 'add', text: 'two' }] }]} />);
        expect([...container.querySelectorAll('tr')].map(row => row.firstElementChild?.textContent)).toEqual(['', '']);
        expect(container.querySelectorAll('[data-op="add"]')).toHaveLength(2);
    });
});
