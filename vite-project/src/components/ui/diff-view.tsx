import { cn } from '@/lib/utils';

export type DiffLineOp = 'context' | 'add' | 'remove';

export interface DiffHunk {
    /** 1-based first line in the old text; 0 when the old text is absent. */
    oldStart: number;
    /** 1-based first line in the new text; 0 when the new text is absent. */
    newStart: number;
    lines: { op: DiffLineOp; text: string }[];
}

const marker: Record<DiffLineOp, string> = { context: ' ', add: '+', remove: '-' };
const tone: Record<DiffLineOp, string> = {
    context: '',
    add: 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
    remove: 'bg-destructive/10 text-destructive',
};

/** Line numbers for each row: old numbers advance on context/remove, new numbers on context/add. */
function numbered(hunk: DiffHunk) {
    let oldLine = hunk.oldStart;
    let newLine = hunk.newStart;
    return hunk.lines.map(line => {
        const row = {
            ...line,
            old: line.op === 'add' ? null : oldLine,
            new: line.op === 'remove' ? null : newLine,
        };
        if (line.op !== 'add') oldLine += 1;
        if (line.op !== 'remove') newLine += 1;
        return row;
    });
}

/** Renders server-computed line hunks; the component never computes a diff itself. */
export function DiffView({ hunks, className, ...props }: { hunks: DiffHunk[]; className?: string; 'aria-label'?: string }) {
    return <div role="group" className={cn('overflow-x-auto rounded-md border bg-muted/30 font-mono text-xs', className)} {...props}>
        {hunks.map((hunk, index) => <table key={index} className="w-full border-collapse first:mt-0 [&+&]:border-t">
            <tbody>
                {numbered(hunk).map((line, row) => <tr key={row} data-op={line.op} className={tone[line.op]}>
                    <td className="w-10 select-none px-2 text-right text-muted-foreground">{line.old || ''}</td>
                    <td className="w-10 select-none px-2 text-right text-muted-foreground">{line.new || ''}</td>
                    <td className="w-4 select-none text-center" aria-hidden="true">{marker[line.op]}</td>
                    <td className="whitespace-pre pr-2">{line.text}</td>
                </tr>)}
            </tbody>
        </table>)}
    </div>;
}
