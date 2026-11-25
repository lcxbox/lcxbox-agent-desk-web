#!/usr/bin/env python3
"""Stream rustc stack remarks into a compact, deterministic report."""
import argparse
from itertools import islice
import json
import re
import subprocess
import sys
from pathlib import Path

import consumer
import symbols

HARD_FRAME_LIMIT = 128 * 1024

REMARK = re.compile(r"(\d+) stack bytes in function '([^']+)'")
ESCAPES = {'SP': '@', 'BP': '*', 'RF': '&', 'LT': '<', 'GT': '>', 'LP': '(', 'RP': ')', 'C': ','}


class NoStackRecords(ValueError):
    """No protected frame was present in the completed diagnostic stream."""


def symbol_key(symbol):
    if not symbol.startswith(('_ZN', '__ZN')):
        raise ValueError(f'unsupported Rust symbol format: {symbol[:1000]}')
    text = symbol[symbol.index('ZN') + 2:]
    parts = []
    while text and not re.fullmatch(r'E(?:\.llvm\.[0-9A-Fa-f]+)?', text):
        n = re.match(r'\d+', text)
        if n is None:
            raise ValueError(f'invalid legacy symbol: {symbol[:1000]}')
        length = int(n[0])
        text = text[len(n[0]):]
        if len(text) < length:
            raise ValueError('truncated legacy symbol')
        parts.append(text[:length])
        text = text[length:]
    if parts and re.fullmatch(r'h[0-9a-f]{16}', parts[-1]):
        parts.pop()
    name = '::'.join(parts)
    name = re.sub(r'\$([^$]+)\$', lambda m: ESCAPES.get(m[1], chr(int(m[1][1:], 16)) if re.fullmatch(r'u[0-9a-f]+', m[1]) else m[0]), name)
    name = name.replace('..', '::')
    # Generic monomorphizations share one report key, using their largest frame.
    name = re.sub(r'::h[0-9a-f]{16}\b', '', name)
    return name


def protected(symbol):
    return consumer.protected(symbol) or ('desk_signal_facade' in symbol and 'lease_heartbeat' in symbol) or ('desk_agent_protocol' in symbol and 'ai_agent' in symbol and 'validate' in symbol) or ('desk_signal_facade' in symbol and 'ai_agent_session' in symbol and any(
        word in symbol for word in (
            'project_snapshot_message_page', 'project_snapshot_message_window',
            'SnapshotMessagePageProjection', 'snapshot_message_groups', 'visible_snapshot_message',
        )
    )) or ('desk_diagnose_core' in symbol and (
        any(word in symbol for word in (
            'future', 'ai_agent', 'tool_result', 'ui_model_ids', 'ui_references',
            'result_model_ids', 'document_model_ids', 'agent_data_model', 'model_controls', 'model_constraints', 'model_input',
            'conversation_attachment::model_read', 'provider_preflight',
        ))
        or ('ContextProtectionSet' in symbol and 'validate_references' in symbol)
        or ('GoalRun' in symbol and 'finish_agent_context_wait_slice' in symbol)
        or ('schedule' in symbol and any(word in symbol for word in ('context_wait', 'published_input', 'proposal', 'management_tools', 'run_approval', 'dispatch')))
    )) or ('agent_goal_store' in symbol and any(word in symbol for word in (
        'settle_slice', 'persist_slice_on', 'validate_slice_session_on',
        'claim_slice', 'apply_owner_action', 'PreparedGoalClaim', 'scheduler_transition', 'wake_for_agent_context', 'continuation_candidates',
    ))) or ('schedule_store' in symbol and any(word in symbol for word in (
        'retrieval_consent', 'fresh_session', 'context_wait', 'fresh_children_settlement', 'fresh_recovery',
        'cancel_run', 'record_run_cancellation',
        'claim_continuation_on', 'agent_retrieval_deadline_on', 'resume_admission',
        'prepare_continuation_task_on', 'bind_continuation_session_on',
        'ContinuationTaskClaim', 'ContinuationSessionClaim',
        'lock_owner_context_authority', 'manage_from_session',
    ))) or ('schedule_executor' in symbol and any(word in symbol for word in (
        'process_goal', 'scan_goals_once', 'scan_once',
    ))) or any(word in symbol for word in (
        'owner_question', 'decision_resume', 'wake_for_decision', 'agent_loop', 'ai_agent_orchestrator', 'remote_tool_edge', 'remote_tool_routing',
        'agent_model', 'agent_model', 'SignalModelSeam', 'capability_provider_work', 'capability_grant_store',
        'agent_approval_concrete', 'approval_reviewer', 'manager_provider_tools', 'agent_subagent_store',
        'PersistedAgentSession', 'SessionManagementMetadata', 'retire_unsupported', 'conversation_title', 'load_title_limit', 'store_title',
        'CommandConfirmation', 'ExecInput', 'apply_automatic_grant_failure',
        'decide_permission_request_with_authority', 'decide_with_authority',
        'agent_stack_probe', 'for_agent_turn', 'agent_data_store', 'agent_run_event_store',
        'agent_data_request_manifest',
        'read_context_output_with_archive_on', 'manage_schedule_tool',
    ))


def stack_records(lines):
    for line in lines:
        try:
            item = json.loads(line)
        except ValueError:
            continue
        diagnostic = item['message'] if isinstance(item.get('message'), dict) else item
        if isinstance(diagnostic, dict) and (match := REMARK.search(diagnostic.get('message', ''))):
            yield int(match[1]), match[2]


def collect_records(records, error_constructor_frames=None, scope=protected):
    frames = {}
    coverage = set()
    count = 0
    has_v0 = False
    records = iter(records)
    while batch := list(islice(records, 512)):
        count += len(batch)
        decoded = iter(symbols.decode_v0([s for _, s in batch if s.startswith(('_R', '__R'))]))
        for size, raw in batch:
            if raw.startswith(('_R', '__R')):
                has_v0 = True
                name = next(decoded)
                if not (scope(name) or scope(raw) or
                        (error_constructor_frames is not None and 'invalid' in name)):
                    continue
                execution = symbols.execution_name(name)
            elif raw.startswith(('_ZN', '__ZN')):
                name = symbol_key(raw)
                execution = name
            elif scope(raw):
                raise ValueError('unsupported Rust symbol format: ' + raw[:1000])
            else:
                continue
            if (error_constructor_frames is not None and execution and
                    execution.startswith('desk_diagnose_core::') and
                    execution.endswith(('::invalid', '::invalid_input'))):
                key = raw.lstrip('_')
                error_constructor_frames[key] = max(size, error_constructor_frames.get(key, 0))
            if scope(name) or scope(raw):
                frames[name] = max(size, frames.get(name, 0))
                if execution:
                    coverage.add(execution)
    if not frames:
        raise NoStackRecords('no protected stack frames: require fresh codegen and a supported remark format')
    result = {'records': count, 'frames': dict(sorted(frames.items()))}
    if has_v0:
        result['execution_names'] = sorted(coverage)
    return result


def collect(lines, error_constructor_frames=None, scope=protected):
    return collect_records(stack_records(lines), error_constructor_frames, scope)


def retain_entity_identity(data, package):
    """Retain stable names for the same entities re-exported by each host.

    Physical symbols remain recorded for review. This does not change sizes,
    execution coverage or the identity of any operation function.
    """
    if package not in ('lcxbox-agent-desk-manager', 'desk-signal'):
        return data
    prefix = ('desk_database::entity::' if package == 'lcxbox-agent-desk-manager'
              else 'desk_signal::entity::')
    current = 'desk_agent_data_store::entity::'
    def canonical_name(symbol):
        # Signal's native quota model also contains local cleanup cursors, so
        # the common counter view is deliberately a distinct logical type.
        return re.sub(re.escape(current) + r'(\w+)::',
            lambda match: (match[0] if package == 'desk-signal' and match[1] == 'agent_quota'
                           else prefix + match[1] + '::'), symbol)
    return remap_names(data, canonical_name, 'entity_symbol_relocations')


def remap_names(data, canonical_name, origin_field):
    frames, origins = {}, {}
    for symbol, size in data['frames'].items():
        canonical = canonical_name(symbol)
        if canonical in frames:
            raise ValueError('report identity has two physical owners: ' + canonical)
        frames[canonical] = size
        if canonical != symbol:
            origins[canonical] = symbol
    result = dict(data, frames=dict(sorted(frames.items())), **{origin_field: origins})
    if 'execution_names' in data:
        result['execution_names'] = sorted({canonical_name(name) for name in data['execution_names']})
    return result


def retain_operation_identity(data, package):
    """Keep stable report identities for explicitly relocated operations.

    Only explicit identity-preserving ownership moves are listed. New publication paths retain
    their physical names and independent execution coverage.
    """
    moves = {
        'desk_diagnose_core::tool_result::':
            'desk_diagnose_core::ai_agent::data_control::history_restricted::',
    }
    def canonical_name(symbol):
        canonical = symbol
        for physical, original in moves.items():
            if physical.endswith('::'):
                canonical = canonical.replace(physical, original)
            else:
                canonical = re.sub(re.escape(physical) + r'(?=::|[<>, ]|$)',
                                   lambda _: original, canonical)
        return canonical
    return remap_names(data, canonical_name, 'operation_symbol_relocations')


def check_ceiling(report, ceiling, required):
    if ceiling <= 0:
        raise ValueError('stack ceiling must be positive')
    errors = [f'{actual} > {ceiling}: {key}' for key, actual in report['frames'].items() if actual > ceiling]
    coverage = list(report.get('execution_names', report['frames']))
    for name, size in report.get('external_polls', {}).items():
        coverage.append('external poll: ' + name)
        if size > ceiling:
            errors.append(f'{size} > {ceiling}: external poll: {name}')
    simple = [symbols.unparameterized_owner(name) for name in coverage] if 'execution_names' in report else coverage
    for pattern in required:
        names = coverage if '<' in pattern else simple
        if '^physical: ' in pattern:
            # Only an explicitly required physical symbol may stand for an
            # inlined body. Never offer generic arguments to broad expressions.
            if any(re.search(pattern, 'physical: ' + name) for name in report['frames']):
                continue
        if not any(re.search(pattern, key) for key in names):
            errors.append(f'missing required family: {pattern}')
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, help='rustc/Cargo JSON log; stdin when omitted')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--package', required=True)
    parser.add_argument('--profile', choices=['debug', 'release'], required=True)
    parser.add_argument('--target', required=True)
    parser.add_argument('--required', action='append', default=[])
    args = parser.parse_args()
    handle = args.input.open() if args.input else sys.stdin
    try:
        report = collect(handle)
    finally:
        if args.input:
            handle.close()
    report.update(package=args.package, profile=args.profile, target=args.target,
                  toolchain=subprocess.check_output(['rustc', '-Vv'], text=True).strip())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    errors = check_ceiling(report, HARD_FRAME_LIMIT, args.required)
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'{report["records"]} records; {len(report["frames"])} protected entries')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        sys.exit(str(error))
