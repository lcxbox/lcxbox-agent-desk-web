import { useState, type ReactNode } from 'react';
import { Check, LoaderCircle, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTranslation } from 'react-i18next';
import type { PermissionRequestDto } from '@/services/types';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import {
    decidePermissionGroup, permissionEntries, permissionGroupApproveAll, permissionGroupDecidedCount, permissionGroupDenyAll,
    permissionGroupOpen, permissionIsAutomaticallyReviewed, permissionRequestOpen, permissionWaitsForGroup,
    type PermissionDecide, type PermissionGroup,
} from './agent-permission-group';

export { permissionIsAutomaticallyReviewed } from './agent-permission-group';

/** What the group buttons need to decide cards the way each card would. */
export type PermissionGroupDecisions = {
    canDecide: (request: PermissionRequestDto) => boolean;
    disabled: boolean;
    onDecide: PermissionDecide;
};

function PermissionGroupView({ group, automaticApproval, decisions, children }: {
    group: PermissionGroup;
    automaticApproval: boolean;
    decisions?: PermissionGroupDecisions;
    children: (request: PermissionRequestDto) => ReactNode;
}) {
    const { t } = useTranslation();
    const [running, setRunning] = useState(false);
    const [failed, setFailed] = useState<string | null>(null);
    const open = permissionGroupOpen(group);
    const policy = { automaticApproval, canDecide: decisions?.canDecide ?? (() => false) };
    const approveAll = permissionGroupApproveAll(group, policy);
    const denyAll = permissionGroupDenyAll(group, policy);
    const step = (request: PermissionRequestDto) => t('pages.aiAgent.permissionGroup.step', {
        index: (request.group?.order ?? group.members.indexOf(request)) + 1, total: group.size,
    });
    const failedMember = group.members.find(member => member.requestId === failed && member.state === 'pending');
    const run = async (approve: boolean) => {
        if (!decisions) return;
        setRunning(true);
        setFailed(null);
        // The decide callback captured at the click is used for every card: it
        // was taken while no decision was in flight, and each call waits for the
        // previous one to finish before the next is sent.
        const result = await decidePermissionGroup(approve ? approveAll.send : denyAll, approve, decisions.onDecide);
        setFailed(result.failed);
        setRunning(false);
    };
    const buttonsDisabled = !decisions || decisions.disabled || running;
    return <div data-testid="ai-agent-permission-group" data-group-id={group.groupId} className="space-y-3 rounded-md border p-3">
        <div className="space-y-1">
            <div className="flex flex-wrap items-center justify-between gap-2">
                <p className="text-sm font-medium">
                    {t('pages.aiAgent.permissionGroup.title', { count: group.size })}
                    <span className="ml-2 text-xs font-normal text-muted-foreground">
                        {t('pages.aiAgent.permissionGroup.progress', { decided: permissionGroupDecidedCount(group), total: group.size })}
                    </span>
                </p>
                {open && decisions && (approveAll.send.length > 0 || denyAll.length > 0) && <div className="flex flex-wrap gap-2">
                    <Button type="button" size="sm" className="gap-1.5 px-2.5" disabled={buttonsDisabled || approveAll.send.length === 0}
                        onClick={() => void run(true)}>
                        <Check className="h-4 w-4" />
                        {t('pages.aiAgent.permissionGroup.approveAll')}
                    </Button>
                    <Button type="button" size="sm" variant="outline" className="gap-1.5 px-2.5" disabled={buttonsDisabled || denyAll.length === 0}
                        onClick={() => void run(false)}>
                        <X className="h-4 w-4" />
                        {t('pages.aiAgent.permissionGroup.denyAll')}
                    </Button>
                </div>}
            </div>
            {open && <p className="text-xs text-muted-foreground">{t('pages.aiAgent.permissionGroup.validity')}</p>}
            {open && approveAll.individual.length > 0 && <p className="text-xs text-muted-foreground">
                {t('pages.aiAgent.permissionGroup.individual', { count: approveAll.individual.length })}
            </p>}
            {failedMember && <p role="alert" className="text-xs text-destructive">
                {t('pages.aiAgent.permissionGroup.stopped', { step: step(failedMember) })}
            </p>}
        </div>
        {group.members.map(member => <div key={member.requestId} className="space-y-1">
            <p className="text-xs font-medium text-muted-foreground">{step(member)}</p>
            {permissionIsAutomaticallyReviewed(member, automaticApproval)
                ? <p role="status" className="flex items-center gap-2 text-xs text-muted-foreground">
                    <LoaderCircle className="h-4 w-4 shrink-0 animate-spin" aria-hidden="true" />
                    {t('pages.aiAgent.permissionAutomaticReview')}
                </p>
                : <>
                    {permissionWaitsForGroup(member, group) && <p role="status" className="text-xs text-emerald-700 dark:text-emerald-300">
                        {t('pages.aiAgent.permissionGroup.waiting')}
                    </p>}
                    {children(member)}
                </>}
        </div>)}
    </div>;
}

export function AgentPermissionRecords({ requests, children, open, onOpenChange, automaticApproval = false, approvalRecords, decisions }: {
    requests: PermissionRequestDto[];
    automaticApproval?: boolean;
    children: (request: PermissionRequestDto) => ReactNode;
    open: boolean;
    onOpenChange: (open: boolean) => void;
    approvalRecords?: ReactNode;
    /** Enables Approve all / Deny all on card groups. */
    decisions?: PermissionGroupDecisions;
}) {
    const { t } = useTranslation();
    const entries = permissionEntries(requests);
    const reviewing = requests.some(request => permissionIsAutomaticallyReviewed(request, automaticApproval));
    // A group stays with the open cards until every card of the step is decided.
    const active = entries.filter(entry => entry.kind === 'group'
        ? permissionGroupOpen(entry.group)
            && entry.group.members.some(member => !permissionIsAutomaticallyReviewed(member, automaticApproval))
        : permissionRequestOpen(entry.request) && !permissionIsAutomaticallyReviewed(entry.request, automaticApproval));
    const latest = requests.at(-1);
    const reviewFailed = latest?.decision?.items.some(item => item.source === 'system' && item.outcome === 'fault');
    const history = entries.filter(entry => entry.kind === 'group'
        ? !permissionGroupOpen(entry.group)
        : !permissionRequestOpen(entry.request));
    const render = (entry: (typeof entries)[number]) => entry.kind === 'group'
        ? <PermissionGroupView key={entry.group.groupId} group={entry.group} automaticApproval={automaticApproval} decisions={decisions}>
            {children}
        </PermissionGroupView>
        : children(entry.request);
    return <>
        {reviewing && <div role="status" data-testid="ai-agent-automatic-review" className="flex items-center gap-2 px-1 text-xs text-muted-foreground">
            <LoaderCircle className="h-4 w-4 shrink-0 animate-spin" aria-hidden="true" />
            {t('pages.aiAgent.permissionAutomaticReview')}
        </div>}
        {reviewFailed && <div role="alert" className="space-y-2 rounded-md border border-destructive/40 p-3 text-sm text-destructive">
            <p>{t('pages.aiAgent.permissionReviewFaultSummary')}</p>
            <Button type="button" size="sm" variant="outline" onClick={() => onOpenChange(true)}>
                {t('pages.aiAgent.permissionHistory')}
            </Button>
        </div>}
        {active.length > 0 && <div data-testid="ai-agent-permission-requests" className="space-y-3 rounded-md border border-amber-500/40 p-3">
            <p className="text-sm font-medium">{t('pages.aiAgent.permissionTitle')}</p>
            <p className="text-xs text-muted-foreground">{t('pages.aiAgent.permissionDescription')}</p>
            {active.map(render)}
        </div>}
        <Sheet open={open} onOpenChange={onOpenChange}>
            <SheetContent className="flex w-full flex-col overflow-hidden sm:max-w-xl">
                <SheetHeader>
                    <SheetTitle>{t('pages.aiAgent.permissionHistory')}</SheetTitle>
                    <SheetDescription>{t('pages.aiAgent.permissionHistoryHint')}</SheetDescription>
                </SheetHeader>
                <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-4">
                    {approvalRecords}
                    {history.length ? [...history].reverse().map(render) : <p className="text-sm text-muted-foreground">{t('pages.aiAgent.permissionHistoryEmpty')}</p>}
                </div>
            </SheetContent>
        </Sheet>
    </>;
}
