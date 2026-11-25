"""Select agent frames instantiated by production host libraries and binary entry points."""
import re


HOSTS = {
    'lcxbox-agent-desk-manager': (
        'lcxbox_agent_desk_manager::service', 'ScheduleExecutor', 'main_request_executor::MainRequestExecutor',
    ),
    'lcxbox-agent-desk-server': (
        'desk_signal', 'SignalScheduleExecutor', 'ai_agent_orchestrator::main_requests::SignalMainRequestExecutor',
    ),
}

# Apply this to demangled physical symbols so the concrete child future identifies
# its actual stream or runtime specialization, even when codegen is downstream.
SCOPED = re.compile(
    r'::schedule_executor::(?:Signal)?ScheduleExecutor>::'
    r'(?:process_goal\w*|load_goal_context|prepare_goal_turn|dispatch_goal|settle_goal|scan_once|scan_goals(?:_once)?|scan_scheduled_runs|'
    r'drive_subagent\w*|process_subagent\w*|prepare_subagent\w*|run_subagent\w*|run_approval_reviews|scan_(?:default|permission|command)_reviews|run)(?=::|$)'
    r'|::(?:main_request_executor::MainRequestExecutor|main_requests::SignalMainRequestExecutor)>::run(?:_requests|_replies|_sources)?(?=::|$)'
    r'|::(?:agent_)?approval_dispatch::(?:process_pending_permission_review|review_and_settle|finish_permission_review)(?=::|$)'
    r'|::agent_approval_store::(?:claim_permission_review|prepare_permission_review_batch|settle_permission_review|authorize_claim_on|check_settlement_source_on)(?=::|$)'
    r'|::agent_approval_dispatch::PermissionReviewDispatch>::(?:prepare|review_candidates|run_claimed|finalize_batch|deny_goal|deny_directory)(?=::|$)'
    r'|::agent_approval_store::(?:authorize_review_claim_on|reserve_review_claim|current_review_authority_on|settle_review_ledger_on|'
    r'project_review_settlement_on|prepare_candidates_on|restore_candidate_for_batch_on)(?=::|$)'
    r'|::agent_session_store::(?:Signal)?AgentSessionStore(?: as desk_diagnose_core::seam::SessionSeam)?>::(?:claim_turn|claim_existing|claim_new|claim_recovered_orphan)(?=::|$)'
    r'|::agent_subagent_store::SubAgentStore>::(?:child_runtime_candidate|reconcile_task)(?=::|$)'
    r'|::agent_subagent_store::recovery::(?:restore_dependencies_on|write_recovered_session_on|rearm_exec_deliveries_on|fail_unavailable_subject_on)(?=::|$)'
    r'|::agent_run_event_store::(?:ManagerRunEventStore|SignalAgentRunEventStore)>::(?:admit_user_input|claim_user_input)(?=::|$)'
    r'|::agent_loop_driver::scheduled_initial::(?:claim_manager_scheduled_initial|prepare_scheduled_initial|claim_prepared_initial)(?=::|$)'
    r'|::agent_data_store::account_cleanup::(?:purge_on|erase_agent_outputs_on|erase_history_and_memory_on|erase_actor_runtime_on)(?=::|$)'
)


# The scheduler owns this existing Actix task. It now passes a boxed phase
# to JoinSet; this startup specialization can be shared by other callers.
BOXED_LOOP = ('core::pin::Pin<alloc::boxed::Box<dyn core::future::future::Future<Output = '
              'core::result::Result<desk_diagnose_core::agent_loop::LoopOutcome, desk_agent_protocol::AgentError>>>>')


def local_spawn(future):
    return 'tokio::task::local::spawn_local_inner::<' + future + '>::{closure#0}'


def protected(name):
    # Both stream paths now store erased unit futures in FuturesUnordered.
    # These exact shared new/push specializations no longer identify one caller.
    erased = '<futures_util::stream::futures_unordered::FuturesUnordered<core::pin::Pin<alloc::boxed::Box<dyn core::future::future::Future<Output = ()>'
    return (SCOPED.search(name) is not None or name == local_spawn(BOXED_LOOP)
            or (name.startswith(erased) and name.endswith(('::new', '::push'))))


def manager_binary_frame(name):
    return protected(name) or re.search(r'lcxbox_agent_desk_manager::(?:run|serve)::\{closure#0\}', name) is not None


def claim_pattern(package):
    manager = package == 'lcxbox-agent-desk-manager'
    host = 'lcxbox_agent_desk_manager::service' if manager else 'desk_signal'
    store = ('AgentSessionStore' if manager else 'SignalAgentSessionStore')
    return r'^_<' + host + '::agent_session_store::' + store + r' as desk_diagnose_core::seam::SessionSeam>::claim_turn::_\{\{closure\}\}$'


def physical(name):
    return '^physical: ' + re.escape(name) + '$'


def poll(owner, method):
    return '^' + re.escape(owner + '::' + method + '::_{{closure}}') + '$'


def stream_polls(host, scheduler, method, entity):
    # Match the Stream method on this exact iterator. A drop or a helper whose
    # arguments merely mention the scheduler has different ownership.
    row = ('(?:desk_agent_data_store|desk_database|desk_signal)::entity::' + entity + '::Model')
    factory = re.escape('<' + host + '::schedule_executor::' + scheduler + '>::' + method)
    factory += r'::\{closure#0\}::\{closure#\d+\}'
    iterator = (r'futures_util::stream::iter::Iter<core::iter::adapters::map::Map<'
                r'alloc::vec::into_iter::IntoIter<' + row + '>, ' + factory + '>>')
    return ['^physical: <futures_util::stream::stream::' + wrapper + '<' + iterator +
            r'> as futures_core::stream::Stream>::poll_next$'
            for wrapper in ('fuse::Fuse', 'buffer_unordered::BufferUnordered')]


def approval_store_polls(manager):
    host = 'lcxbox_agent_desk_manager::service' if manager else 'desk_signal'
    return [poll(host + '::agent_approval_store', method) for method in
            ('prepare_permission_review_batch', 'claim_permission_review', 'settle_permission_review')]


def approval_polls(manager):
    dispatch = ('lcxbox_agent_desk_manager::service::agent_approval_dispatch' if manager
                else 'desk_signal::approval_dispatch')
    return [poll(dispatch, 'process_pending_permission_review'), *approval_store_polls(manager)]


def approval_stage_polls(manager):
    if manager:
        host = 'lcxbox_agent_desk_manager::service'
        dispatch = host + '::agent_approval_dispatch::PermissionReviewDispatch'
        methods = ('prepare', 'review_candidates', 'run_claimed', 'finalize_batch', 'deny_goal', 'deny_directory')
        storage = ('authorize_review_claim_on', 'reserve_review_claim', 'current_review_authority_on',
                   'settle_review_ledger_on', 'project_review_settlement_on', 'prepare_candidates_on',
                   'restore_candidate_for_batch_on')
    else:
        host = 'desk_signal'
        dispatch = host + '::approval_dispatch'
        methods = ('review_and_settle', 'finish_permission_review')
        storage = ('authorize_claim_on', 'check_settlement_source_on')
    return [*(poll(dispatch, method) for method in methods),
            *(poll(host + '::agent_approval_store', method) for method in storage)]


def business_polls(package):
    host, scheduler, requests = HOSTS[package]
    owner = host + '::schedule_executor::' + scheduler
    manager = package == 'lcxbox-agent-desk-manager'
    methods = ['scan_once', 'drive_subagent', 'process_subagent_task', 'process_subagent_wait']
    if manager:
        methods.extend(['process_goal_inner', 'scan_goals_once', 'scan_default_reviews',
                        'scan_permission_reviews', 'scan_command_reviews'])
    else:
        methods.extend(['process_goal', 'scan_scheduled_runs', 'scan_goals'])
    patterns = [poll(owner, method) for method in methods]
    patterns.extend(poll(host + '::' + requests, method) for method in ('run_requests', 'run_replies', 'run_sources'))
    store = host + '::agent_subagent_store::SubAgentStore'
    patterns.extend(poll(store, method) for method in ('child_runtime_candidate', 'reconcile_task'))
    recovery = host + '::agent_subagent_store::recovery'
    patterns.extend(poll(recovery, method) for method in ('restore_dependencies_on', 'write_recovered_session_on'))
    patterns.append(poll(recovery, 'fail_unavailable_subject_on' if manager else 'rearm_exec_deliveries_on'))
    return patterns


def debug_dispatch_patterns(package):
    host, scheduler, requests = HOSTS[package]
    concrete = '<' + host + '::schedule_executor::' + scheduler + '>'
    patterns = business_polls(package)
    for method, row in [('run_subagent_tasks', 'agent_subagent_run'), ('run_subagent_waits', 'agent_delegation_group')]:
        patterns.extend(stream_polls(host, scheduler, method, row))
    for method in ('run', 'run_subagent_tasks', 'run_subagent_waits', 'run_subagent_recovery', 'run_approval_reviews'):
        patterns.append(physical(local_spawn(concrete + '::' + method + '::{closure#0}')))
    patterns.append(physical(local_spawn('<' + host + '::' + requests + '>::run::{closure#0}')))
    if package == 'lcxbox-agent-desk-server':
        patterns.append(physical(local_spawn(BOXED_LOOP)))
    return patterns


def required_patterns(package, profile):
    if package == 'lcxbox-agent-desk-manager' and profile == 'debug':
        # The unoptimized binary drives serve; scheduler code is in its library.
        return [poll('lcxbox_agent_desk_manager', method) for method in ('run', 'serve')]
    if package == 'lcxbox-agent-desk-server' and profile == 'debug':
        return debug_dispatch_patterns(package)
    host, scheduler, requests = HOSTS[package]
    patterns = []
    owner = host + '::schedule_executor::' + scheduler
    entries = [(owner, method) for method in ('run', 'run_subagent_tasks', 'run_subagent_waits',
                                              'run_subagent_recovery', 'run_approval_reviews')]
    entries.append((host + '::' + requests, 'run'))
    for entry, method in entries:
        future = '<' + entry + '>::' + method + '::{closure#0}'
        instrumented = '<tracing::instrument::Instrumented<' + future + '> as core::future::future::Future>::poll'
        patterns.append('(?:' + poll(entry, method) + '|' + physical(instrumented) + ')')
    if package == 'lcxbox-agent-desk-server':
        patterns.extend(business_polls(package))
        # The optimized boxed dispatch is driven here; its three storage
        # phases have their own emitted polls in the signal library.
        dispatch = 'desk_signal::approval_dispatch::process_pending_permission_review::{closure#0}'
        pinned = '<core::pin::Pin<alloc::boxed::Box<' + dispatch + '>> as core::future::future::Future>::poll'
        patterns.append('(?:' + poll('desk_signal::approval_dispatch', 'process_pending_permission_review') +
                        '|' + physical(pinned) + ')')
    for method, row in [('run_subagent_tasks', 'agent_subagent_run'), ('run_subagent_waits', 'agent_delegation_group')]:
        # BufferUnordered inlines its polling loop on optimized builds. Its
        # Fuse still calls into the concrete map for this row and scheduler.
        patterns.append(stream_polls(host, scheduler, method, row)[0])
    return patterns
