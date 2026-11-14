import { Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';

/** Approve / reject row shared by the proposal cards the owner decides. */
export function ProposalDecisionActions({ disabled, approveLabel, rejectLabel, onDecide }: {
    disabled: boolean; approveLabel: string; rejectLabel: string; onDecide(approve: boolean): void;
}) {
    return <div className="flex flex-wrap gap-2">
        <Button type="button" size="sm" disabled={disabled} onClick={() => onDecide(true)}><Check className="mr-2 h-4 w-4" />{approveLabel}</Button>
        <Button type="button" size="sm" variant="outline" disabled={disabled} onClick={() => onDecide(false)}><X className="mr-2 h-4 w-4" />{rejectLabel}</Button>
    </div>;
}
