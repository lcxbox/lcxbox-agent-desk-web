//! Source names of runtime status labels.
//!
//! A [`ServerResultSource`] names a server-authored tool result or request
//! projection that the source registry accepts as a native control: text with
//! no data of its own, governed only by its single parent's policy. Only the
//! `server_*` label functions take it, and only their label shape registers,
//! so every name they can produce is registered.
//!
//! A name belongs here only when its body is server constants, ids, counts,
//! or bytes its parent label already covers, such as the calling model
//! output's own arguments. A body with any other text (device or Provider
//! output, files, attachments, web pages, subagent output, host error text,
//! reviewer reasons) uses a data constant below and a `data_*` label, which
//! never registers whatever its name.

macro_rules! server_result_sources {
    ($($variant:ident => $name:expr,)+) => {
        /// A registered server-authored result; see the module documentation.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ServerResultSource {
            $($variant,)+
        }

        impl ServerResultSource {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                }
            }
        }
    };
}

server_result_sources! {
    // Agent loop notices for one tool call.
    ApprovedInputMismatch => "approved_input_mismatch",
    DelegationSourcePaused => "delegation_source_paused",
    DirectoryPauseToolCall => "directory_pause_tool_call",
    DirectoryProposalFailed => "directory_proposal_failed",
    DirectoryProposalPending => "directory_proposal_pending",
    FreshVisualVerificationRequired => "fresh_visual_verification_required",
    GoalControlBatchRejected => "goal_control_batch_rejected",
    GoalOpenFailed => "goal_open_failed",
    GoalOpenPending => "goal_open_pending",
    HaltedToolCall => "halted_tool_call",
    InactiveGrantToolCall => "inactive_grant_tool_call",
    InvalidDesktopId => "invalid_desktop_id",
    InvalidServerBoundWebEvidence => "invalid_server_bound_web_evidence",
    InvalidToolArguments => "invalid_tool_arguments",
    LeaseLostToolCall => "lease_lost_tool_call",
    MemoryBatchRejected => "memory_batch_rejected",
    MemoryControlRejected => "memory_control_rejected",
    PermissionDenied => "permission_denied",
    PermissionPauseToolCall => "permission_pause_tool_call",
    PermissionProposalRejected => "permission_proposal_rejected",
    PermissionRequestStatus => "permission_request_status",
    PermissionResumeNotExecuted => "permission_resume_not_executed",
    ProposalPauseToolCall => "proposal_pause_tool_call",
    RepeatLimitToolCall => "repeat_limit_tool_call",
    ScheduleProposalFailed => "schedule_proposal_failed",
    SkillProposalFailed => "skill_proposal_failed",
    SupersedeToolCall => "supersede_tool_call",
    SupersededToolCall => "superseded_tool_call",
    TaskPermissionDenied => "task_permission_denied",
    ToolBudgetAdmissionFailed => "tool_budget_admission_failed",
    UnavailableToolCall => "unavailable_tool_call",
    UnloadedToolCall => "unloaded_tool_call",
    // Execution placeholders and background task notices.
    ExecutionPlaceholder => "execution_placeholder",
    BackgroundTaskOutcomeUnknown => "background_task_outcome_unknown",
    WaitForTask => crate::wait_tools::WAIT_TOOL_NAME,
    // Session recovery.
    ExecutionOutcomeUnknown => "execution_outcome_unknown",
    InterruptedCallRecovery => "interrupted_call_recovery",
    ManualOutcomeDisposition => "manual_outcome_disposition",
    PermissionPauseClosed => "permission_pause_closed",
    // A failed call's error built from its typed origin (`ToolError`).
    ToolCallError => "tool_call_error",
    // Successful receipts of tools whose body is ids, states or the call's
    // own arguments.
    ControlGoal => crate::goal_tools::CONTROL_GOAL_TOOL_NAME,
    DescribeTools => crate::capability_disclosure::LOAD_CAPABILITY_DETAILS_TOOL_NAME,
    LoadSkill => crate::skills::LOAD_TOOL,
    ManageMemories => crate::ai_assistant::data_tools::MANAGE_MEMORIES,
    PrepareIssueReply => crate::issue_reply::PREPARE_TOOL_NAME,
    PreviewComputerAction => crate::ai_assistant::PREVIEW_COMPUTER_ACTION_TOOL,
    RequestPermissions => crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
    UpdateTaskStatus => crate::task_status_tools::UPDATE_TASK_STATUS_TOOL_NAME,
    AttachmentReadReceipt => "attachment_read_receipt",
    MemoryForgetReceipt => "memory_forget_receipt",
    // Proposals and decisions.
    ScheduleActivation => "schedule_activation",
    ScheduleCancellation => "schedule_cancellation",
    ScheduleProposal => "schedule_proposal",
    ScheduleQuery => "schedule_query",
    ScheduledTaskInput => "scheduled_task_input",
    SkillProposal => "skill_proposal",
    SkillProposalDecision => "skill_proposal_decision",
    DirectoryDecision => "directory_decision",
    // Subagent stops.
    OwnerStopResultUnavailable => "owner_stop_result_unavailable",
    PreviousRequestEnded => "previous_request_ended",
    // Request projections.
    EmptyEndTurnRetry => "empty_end_turn_retry",
    GoalCompletedHint => "goal_completed_hint",
    GoalControlCorrection => "goal_control_correction",
    InputWatermark => "input_watermark",
    InvalidToolArgumentsRetry => "invalid_tool_arguments_retry",
    LatestInputProjection => "latest_input_projection",
    PermissionContinuationCheckpoint => "permission_continuation_checkpoint",
    PermissionProtocolRetry => "permission_protocol_retry",
    PostToolPermissionProtocolRetry => "post_tool_permission_protocol_retry",
    RuntimeContext => "runtime_context",
    TruncatedTurnRetry => "truncated_turn_retry",
    // Host recovery of device actions.
    ChildMutationResultUnavailable => "child_mutation_result_unavailable",
    RecoverToolCall => "recover_tool_call",
    RecoveredUnissuedAction => "recovered_unissued_action",
    ScheduledActionStatus => "scheduled_action_status",
    ScheduledCommandStatus => "scheduled_command_status",
    ScheduledReadUnavailable => "scheduled_read_unavailable",
    ScheduledUnboundAction => "scheduled_unbound_action",
    ScheduledUnissuedAction => "scheduled_unissued_action",
    TaskUnknownAction => "task_unknown_action",
}

impl ServerResultSource {
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == name)
    }

    /// Decision receipts that may carry an AI reviewer's reason, and so may
    /// use the multi-parent review label shape.
    pub const fn reviewed(self) -> bool {
        matches!(
            self,
            Self::SkillProposalDecision | Self::ScheduleActivation | Self::DirectoryDecision
        )
    }
}

/// Device or Provider execution output, approval rejections and host errors.
pub const PROVIDER_EXECUTION_STATUS: &str = "provider_execution_status";
/// Artifact identities from earlier Provider results.
pub const REQUESTED_ARTIFACT_REGISTRY_PROJECTION: &str = "requested_artifact_registry_projection";
/// Earlier Provider results reused in one request.
pub const REUSABLE_PROVIDER_RESULT_PROJECTION: &str = "reusable_provider_result_projection";
/// The origin label of an action result.
pub const FREEZE_ACTION_RESULT_ORIGIN: &str = "freeze_action_result_origin";
/// Device-side directory resolution errors.
pub const DIRECTORY_RESOLUTION_FAILED: &str = "directory_resolution_failed";
/// Goal text written by the model, labelled under the owner's message.
pub const GOAL_ORIGINAL_PROJECTION: &str = "goal_original_projection";
pub const GOAL_CLARIFICATION_PROJECTION: &str = "goal_clarification_projection";
pub const GOAL_STATE_PROJECTION: &str = "goal_state_projection";
pub const GOAL_PROPOSAL_DECISION: &str = "goal_proposal_decision";

// Other system source names; their own contracts decide registration.
/// An owner message.
pub const USER_MESSAGE: &str = "send-message";
/// A model output.
pub const MODEL_RESPONSE: &str = "model-response";
/// The projected system prompt.
pub const SYSTEM_PROMPT: &str = "system-prompt-projector";
/// A context checkpoint summary.
pub const CHECKPOINT_SUMMARY: &str = "checkpoint-summary";
/// The owner's frozen personalization.
pub const FROZEN_USER_INSTRUCTIONS: &str = "frozen-user-instructions";
/// The interactive user's home directory.
pub const INTERACTIVE_USER_HOME: &str = "interactive-user-home";
/// The current memories projection.
pub const CURRENT_MEMORIES: &str = "current_memories";
/// A subagent delegation projection.
pub const DELEGATION_STATE: &str = "delegation_state";
/// A subagent report notification.
pub const SUBAGENT_NOTIFICATION: &str = "subagent_notification";
/// The capability authorization prompt.
pub const CAPABILITY_AUTHORIZATION: &str = "capability-authorization";
/// Runtime continuation messages.
pub const SCHEDULED_CONTINUATION: &str = "scheduled-continuation";
pub const PERMISSION_DECISION_RESUME: &str = "permission-decision-resume";
pub const PROPOSAL_DECISION_RESUME: &str = "proposal-decision-resume";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_parse_back() {
        let mut names = std::collections::BTreeSet::new();
        for source in ServerResultSource::ALL {
            assert!(names.insert(source.as_str()), "{source:?}");
            assert_eq!(ServerResultSource::parse(source.as_str()), Some(*source));
        }
        for data in [
            PROVIDER_EXECUTION_STATUS,
            REQUESTED_ARTIFACT_REGISTRY_PROJECTION,
            REUSABLE_PROVIDER_RESULT_PROJECTION,
            FREEZE_ACTION_RESULT_ORIGIN,
            DIRECTORY_RESOLUTION_FAILED,
            GOAL_ORIGINAL_PROJECTION,
            GOAL_CLARIFICATION_PROJECTION,
            GOAL_STATE_PROJECTION,
            GOAL_PROPOSAL_DECISION,
            "unknown",
        ] {
            assert_eq!(ServerResultSource::parse(data), None, "{data}");
        }
    }
}
