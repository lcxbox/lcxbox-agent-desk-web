import type { DecisionPauseDto, SkillProposalView } from '@/services/types';
import type { AssistantFileScopeView } from './assistant-file-scope';

export type AssistantDecisionKind = DecisionPauseDto['kind'];
export type AssistantDecisionState = 'pending' | 'submitted' | 'approved' | 'rejected' | 'conflicted' | 'withdrawn';
export type AssistantDecider = 'approval_ai' | 'owner' | 'system';

/** A skill proposal, fresh-task proposal or directory request submitted by a tool call. */
export type AssistantDecisionReceipt = {
    kind: AssistantDecisionKind;
    requestId: string;
    /** `submitted` means the snapshot holds no source for the decision. */
    state: AssistantDecisionState;
    decidedBy?: AssistantDecider;
    reason?: string | null;
    /** A pending request the owner must decide now. */
    awaitingOwner?: boolean;
};

type Outcome = Pick<AssistantDecisionReceipt, 'state' | 'decidedBy' | 'reason'>;

export type AssistantDecisionSources = {
    messages: { role: string; text: string }[];
    skillProposals?: SkillProposalView[];
    fileScope?: AssistantFileScopeView;
    decisionPause?: DecisionPauseDto | null;
};

const key = (kind: AssistantDecisionKind, requestId: string) => `${kind}:${requestId}`;

/** The element of the card where the owner decides the request. */
export function decisionCardId(kind: AssistantDecisionKind, requestId: string): string {
    return kind === 'directory' ? `assistant-directory-${requestId}`
        : kind === 'skill' ? `assistant-skill-proposal-${requestId}` : `assistant-schedule-proposal-${requestId}`;
}

/** The request a tool receipt submitted, while its turn waits for the decision. */
export function decisionRequestOf(receipt: Record<string, unknown>): { kind: AssistantDecisionKind; requestId: string } | null {
    if (receipt.awaiting_decision !== true) return null;
    if (typeof receipt.proposal_id === 'string') return { kind: 'skill', requestId: receipt.proposal_id };
    if (receipt.kind === 'fresh_task' && typeof receipt.schedule_id === 'string') return { kind: 'schedule', requestId: receipt.schedule_id };
    if (typeof receipt.directory_request_id === 'string') return { kind: 'directory', requestId: receipt.directory_request_id };
    return null;
}

/** Decisions the server handed to the assistant as system events. */
function decisionEvent(message: { role: string; text: string }): [string, Outcome] | null {
    if (message.role !== 'system_event') return null;
    try {
        const value = JSON.parse(message.text);
        const decidedBy = value.decided_by === 'approval_ai' || value.decided_by === 'owner' ? value.decided_by : undefined;
        const reason = typeof value.reason === 'string' ? value.reason : null;
        if (value.event === 'skill_proposal_decided' && typeof value.proposal_id === 'string'
            && ['approved', 'rejected', 'conflicted'].includes(value.status)) {
            return [key('skill', value.proposal_id), { state: value.status, decidedBy, reason }];
        }
        if ((value.event === 'scheduled_task_activated' || value.event === 'scheduled_task_rejected')
            && typeof value.schedule_id === 'string') {
            return [key('schedule', value.schedule_id),
                { state: value.event === 'scheduled_task_activated' ? 'approved' : 'rejected', decidedBy, reason }];
        }
    } catch { /* Not a decision event. */ }
    return null;
}

function skillOutcome(proposal: SkillProposalView): Outcome | null {
    if (proposal.status === 'pending') return null;
    return {
        state: proposal.status,
        decidedBy: proposal.status === 'withdrawn' ? 'system'
            : proposal.decisionSource === 'ai_review' ? 'approval_ai' : proposal.decisionSource ?? undefined,
        reason: proposal.decisionReason,
    };
}

function directoryOutcome(directory: AssistantFileScopeView['directories'][number]): Outcome | null {
    const decidedBy = directory.decisionActor === 'ai_approval' ? 'approval_ai'
        : directory.decisionActor === 'owner' || directory.decisionActor === 'system' ? directory.decisionActor : undefined;
    if (directory.state === 'approved' || directory.state === 'revoked') {
        return { state: 'approved', decidedBy, reason: directory.decisionReason };
    }
    // Stopping the conversation records the request as a system rejection.
    return directory.state === 'rejected' ? { state: 'rejected', decidedBy, reason: directory.decisionReason } : null;
}

/** Resolves each submitted request from the snapshot that contains it. */
export function decisionResolver(sources: AssistantDecisionSources) {
    const events = new Map(sources.messages.flatMap(message => {
        const event = decisionEvent(message);
        return event ? [event] : [];
    }));
    const pause = sources.decisionPause;
    return (kind: AssistantDecisionKind, requestId: string): AssistantDecisionReceipt => {
        const skill = kind === 'skill' ? sources.skillProposals?.find(proposal => proposal.proposalId === requestId) : undefined;
        const directory = kind === 'directory'
            ? sources.fileScope?.directories.find(record => record.requestId === requestId) : undefined;
        const outcome = events.get(key(kind, requestId))
            ?? (skill && skillOutcome(skill)) ?? (directory && directoryOutcome(directory));
        if (outcome) return { kind, requestId, ...outcome };
        if (pause?.kind === kind && pause.requestId === requestId) {
            return { kind, requestId, state: 'pending', awaitingOwner: pause.awaitingOwner };
        }
        if (skill || directory?.state === 'pending') {
            return { kind, requestId, state: 'pending', awaitingOwner: skill?.awaitingOwner ?? false };
        }
        return { kind, requestId, state: 'submitted' };
    };
}

/** The paused request is decided while the server has yet to resume the turn. */
export function decisionPauseResuming(sources: AssistantDecisionSources): boolean {
    const pause = sources.decisionPause;
    if (!pause) return false;
    return !['pending', 'submitted'].includes(decisionResolver(sources)(pause.kind, pause.requestId).state);
}

/**
 * Directory requests waiting for the owner. The request the conversation
 * paused on follows the server's `awaitingOwner`; others are the owner's
 * unless the AI reviewer handles model proposals.
 */
export function ownerPendingDirectories<Directory extends AssistantFileScopeView['directories'][number]>(
    directories: Directory[], pause: DecisionPauseDto | null | undefined, automaticApproval: boolean,
): Directory[] {
    return directories.filter(directory => directory.state === 'pending'
        && (pause?.kind === 'directory' && pause.requestId === directory.requestId ? pause.awaitingOwner
            : !(automaticApproval && directory.source === 'model_proposal')));
}
