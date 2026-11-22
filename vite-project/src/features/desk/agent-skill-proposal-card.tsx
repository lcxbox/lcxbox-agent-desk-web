import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Disclosure } from '@/components/ui/disclosure';
import { deskErrorMessage, errorCodeOf, SKILL_PROPOSAL_ERROR_KEYS } from '@/lib/desk-error-i18n';
import { decideAiAgentSkillProposal } from '@/services/clients';
import type { SkillProposalView } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { SkillDiffList } from '@/features/settings/agent-skill-review';
import { ProposalDecisionActions } from './proposal-decision-actions';
import { decisionCardId } from './agent-decision-receipt';

const prefix = 'pages.agentSkills.proposal.';

/** Codes after which the shown proposal is stale and must be read again. */
const stale: number[] = [deskErrorCodeEnum.REVISION_CONFLICT, deskErrorCodeEnum.PRECONDITION_FAILED];

/**
 * One AI-proposed change to a personal skill: create, update, disable or
 * delete. A pending proposal can be decided by the owner at any time;
 * `awaitingOwner` marks it as the owner's pending item.
 */
export function SkillProposalCard({ proposal, onRefresh, onDecided }: {
    proposal: SkillProposalView; onRefresh(): void; onDecided?(proposal: SkillProposalView): void;
}) {
    const { t } = useTranslation();
    const [decided, setDecided] = useState<SkillProposalView | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const view = decided && decided.proposalId === proposal.proposalId && decided.revision >= proposal.revision ? decided : proposal;
    const pending = view.status === 'pending';
    // Disable and delete proposals carry the current version as a snapshot, not a change to review.
    const removal = view.kind === 'disable' || view.kind === 'delete';

    const decide = async (approve: boolean) => {
        if (busy || !pending) return;
        setBusy(true); setError('');
        try {
            const body = await decideAiAgentSkillProposal({ proposalId: view.proposalId, approve, expectedRevision: view.revision });
            if (!body.success || !body.data) throw new Error('Skill proposal decision failed');
            setDecided(body.data);
            onDecided?.(body.data);
        } catch (cause) {
            const code = errorCodeOf(cause);
            setError(deskErrorMessage(t, SKILL_PROPOSAL_ERROR_KEYS, code, null, t('pages.agentSkills.failed')));
            if (code != null && stale.includes(code)) onRefresh();
        } finally { setBusy(false); }
    };

    return <article id={decisionCardId('skill', view.proposalId)} data-agent-pending={pending && view.awaitingOwner ? '' : undefined} tabIndex={-1}
        aria-label={view.name} className="min-w-0 space-y-3 rounded-md border border-amber-500/40 p-3">
        <div className="flex flex-wrap items-center gap-2">
            <h3 className="min-w-0 break-all font-mono text-sm font-medium">{view.name}</h3>
            <Badge variant="secondary">{t(`${prefix}kind.${view.kind}`)}</Badge>
            <Badge variant={pending ? 'default' : 'outline'}>{t(`${prefix}status.${view.status}`)}</Badge>
            {view.decisionSource && <Badge variant="outline">{t(`pages.agentSkills.decision.${view.decisionSource}`)}</Badge>}
        </div>
        {view.reason && <p className="whitespace-pre-wrap break-words text-sm">{view.reason}</p>}
        {view.decisionReasonCode && <p className="text-xs text-muted-foreground">
            {t(`pages.agentSkills.reasonCode.${view.decisionReasonCode}`, { defaultValue: view.decisionReasonCode })}</p>}
        {view.decisionReason && <p className="whitespace-pre-wrap break-words text-xs text-muted-foreground">{view.decisionReason}</p>}
        {view.kind === 'update'
            ? <SkillDiffList diff={view.diff} />
            : <div className="space-y-2">
                <Disclosure title={<span className="font-mono text-xs">SKILL.md</span>} defaultOpen={!removal}>
                    <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/30 p-2 text-xs">{view.skillMd}</pre>
                </Disclosure>
                {view.files.map(file => <Disclosure key={file.path} title={<span className="break-all font-mono text-xs">{file.path}</span>}>
                    <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/30 p-2 text-xs">{file.text}</pre>
                </Disclosure>)}
            </div>}
        {view.kind === 'delete' && <p className="text-sm text-destructive">{t(`${prefix}deleteIrreversible`)}</p>}
        {view.status === 'conflicted' && <p className="text-sm text-muted-foreground">
            {t(`${prefix}${removal ? 'removalConflictedNote' : 'conflictedNote'}`)}</p>}
        {pending && <ProposalDecisionActions disabled={busy} approveLabel={t(`${prefix}approve`)} rejectLabel={t(`${prefix}reject`)}
            onDecide={approve => void decide(approve)} />}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
    </article>;
}

export function SkillProposalCards({ proposals, onRefresh, onDecided }: {
    proposals: SkillProposalView[]; onRefresh(): void; onDecided?(proposal: SkillProposalView): void;
}) {
    return <>
        {proposals.map(proposal => <SkillProposalCard key={proposal.proposalId} proposal={proposal}
            onRefresh={onRefresh} onDecided={onDecided} />)}
    </>;
}

/** Proposals that wait for the owner in the conversation's pending list. */
export function pendingSkillProposalCount(proposals: SkillProposalView[]): number {
    return proposals.filter(proposal => proposal.status === 'pending' && proposal.awaitingOwner).length;
}
