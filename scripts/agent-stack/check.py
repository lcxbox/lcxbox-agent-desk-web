#!/usr/bin/env python3
"""Build one protected target with fresh LLVM stack remarks and enforce its baseline."""
import argparse
from functools import cache
import hashlib
import re
import shutil
import json
import os
import subprocess
import sys
import tempfile
import tomllib
import uuid
from pathlib import Path
import consumer
import report
import symbols

ROOT = Path(__file__).resolve().parents[2]

SCHEDULE_ERROR = 'desk_diagnose_core::schedule::proposal::invalid_input'


FRESH_TASK_POLL = 'desk_diagnose_core::agent_loop::fresh_task::resume_claimed_fresh_task_turn::{closure#0}'


@cache
def llvm_tool(name):
    host = next(line[6:] for line in subprocess.check_output(
        ['rustc', '-Vv'], cwd=ROOT, text=True).splitlines() if line.startswith('host: '))
    sysroot = Path(subprocess.check_output(['rustc', '--print', 'sysroot'], cwd=ROOT, text=True).strip())
    binary = sysroot / 'lib' / 'rustlib' / host / 'bin' / ('llvm-' + name + ('.exe' if os.name == 'nt' else ''))
    return str(binary) if binary.is_file() else shutil.which('llvm-' + name) or shutil.which(name)


def external_fresh_task_poll(rlib):
    # The non-generic poll is emitted by desk-diagnose-core. Only its exact
    # undefined symbol proves the host refers to that body; a drop or boxing
    # specialization that mentions the future in its type arguments cannot.
    nm = llvm_tool('nm')
    if not nm:
        raise ValueError('stack coverage requires llvm-nm (rustup component add llvm-tools-preview)')
    with subprocess.Popen([nm, '-g', '-P', rlib], stdout=subprocess.PIPE, text=True) as process:
        found = has_external_fresh_task_poll(process.stdout)
        if process.wait():
            raise ValueError('could not inspect the host archive for external poll references')
    return found


def has_external_fresh_task_poll(lines):
    undefined, defined = set(), set()
    for line in lines:
        if 'resume_claimed_fresh_task_turn' not in line:
            continue
        fields = line.split()
        if len(fields) >= 2 and fields[0].startswith(('_R', '__R')):
            (undefined if fields[1] == 'U' else defined).add(fields[0])
    return FRESH_TASK_POLL in symbols.decode_v0(sorted(undefined - defined))



def schedule_error_frame(lines, measured):
    """Find a measured function at the exact location of the required constructor."""
    locations = {}
    required = None
    member = 0
    object_format = ''
    for line in lines:
        if 'file format' in line:
            member += 1
            object_format = line.partition('file format')[2].strip().lower()
            continue
        if not member or 'desk_diagnose_core' not in line or 'invalid' not in line:
            continue
        fields = line.split()
        raw = fields[-1]
        symbol = raw.lstrip('_')
        is_required = re.fullmatch(r'ZN18desk_diagnose_core8schedule8proposal13invalid_input17h[0-9a-f]{16}E', symbol)
        if symbol.startswith('R') and '13invalid_input' in raw:
            is_required = symbols.decode_v0([raw])[0] == SCHEDULE_ERROR
        if symbol not in measured and not is_required:
            continue
        # ELF and Mach-O objdump tables report the section of a defined function.
        if object_format.startswith(('elf', 'mach-o')) and 'F' in fields and re.fullmatch(r'[0-9a-fA-F]+', fields[0]):
            section = fields[fields.index('F') + 1]
            if section != '__TEXT,__text' and not section.startswith('.text'):
                continue
            position = (member, section, int(fields[0], 16))
        elif object_format.startswith(('coff', 'pe')):
            # COFF may give many sections the same name. Use the section number.
            coff = re.search(r'\(sec\s+(\d+)\).*\(ty\s+20\).*\b0x([0-9a-fA-F]+)\s+' + re.escape(raw) + r'$', line.rstrip())
            if not coff or int(coff[1]) == 0:
                continue
            position = (member, int(coff[1]), int(coff[2], 16))
        else:
            continue
        if is_required:
            required = position
        if symbol in measured and (position not in locations or measured[symbol] > locations[position][0]):
            locations[position] = (measured[symbol], raw)
    return locations.get(required)


def known_overflow_probe(workspace=None):
    with tempfile.TemporaryDirectory(prefix='agent-stack-probe-') as directory:
        source = Path(directory, 'probe.rs')
        source.write_text(f'fn main() {{ let data = [0u8; {report.HARD_FRAME_LIMIT * 2}]; std::hint::black_box(&data); }}\n')
        result = subprocess.run([
            'rustc', str(source), '--crate-name', 'agent_stack_probe', '--emit=obj',
            '-o', str(Path(directory, 'probe.o')), '-C', 'opt-level=0',
            '-C', 'remark=prolog-epilog', '--error-format=json',
        ], cwd=workspace, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if result.returncode:
            raise ValueError('stack analysis positive control did not compile')
        parsed = report.collect(result.stdout.splitlines())
        if parsed['frames'].get('agent_stack_probe::main', 0) <= report.HARD_FRAME_LIMIT:
            raise ValueError('stack analysis did not detect its known oversized frame')


def measure_external_core(workspace, profile, target, host_features):
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'],
        cwd=workspace, text=True))
    core_name = 'desk-diagnose-core'
    member = any(package['name'] == core_name and package['id'] in metadata['workspace_members']
                 for package in metadata['packages'])
    # Cargo accepts feature flags only on workspace members. For a dependency
    # selected by -p it still uses this workspace's lockfile and resolved graph.
    core = collect_package(workspace, core_name, profile, target,
                           ','.join(host_features) if member else '', member)
    if set(core['resolved_features']) != set(host_features):
        raise ValueError(f'external fresh-task poll requires host features {sorted(host_features)}, '
                         f'but core reported {sorted(core["resolved_features"])}')
    if FRESH_TASK_POLL not in core['frames']:
        raise ValueError('external fresh-task poll is missing from the measured core')
    return core


def collect_package(workspace, package, profile, target, features, no_defaults, binary=False):
    args = ['cargo', 'rustc', '--locked', '-p', package, '--message-format=json']
    args.extend(['--bin', package] if binary else ['--lib'])
    target_name = package if binary else package.replace('-', '_')
    target_kind = 'bin' if binary else 'lib'
    if target:
        args.extend(['--target', target])
    if profile == 'release':
        args.append('--release')
    if features:
        args.extend(['--features', features])
    if no_defaults:
        args.append('--no-default-features')
    # An unused, unique cfg changes only the selected rustc invocation, forcing
    # codegen without deleting package artifacts or invalidating dependencies.
    nonce = f'desk_stack_codegen_{uuid.uuid4().hex}'
    args.extend(['--', '--cfg', nonce, '-C', 'remark=prolog-epilog'])
    state = {'finished': False, 'fresh_artifact': False}
    constructors = {} if package == 'desk-diagnose-core' and profile == 'release' else None
    with subprocess.Popen(args, cwd=workspace, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          text=True, bufsize=1) as process:
        def lines():
            for line in process.stdout:
                try:
                    item = json.loads(line)
                except ValueError:
                    if 'stack bytes' not in line:
                        print(line.rstrip(), file=sys.stderr)
                    yield line
                    continue
                if item.get('reason') == 'build-finished':
                    state['finished'] = item.get('success') is True
                if item.get('reason') == 'compiler-artifact' and item.get('target', {}).get('name') == 'desk_diagnose_core':
                    state['core_features'] = item['features']
                if item.get('reason') == 'compiler-artifact' and item['target']['name'] == target_name and target_kind in item['target']['kind']:
                    state['fresh_artifact'] = item.get('fresh') is False
                    state['cargo_profile'] = item.get('profile', {})
                    state['resolved_features'] = item.get('features', [])
                    state['rlib'] = next((p for p in item.get('filenames', []) if p.endswith('.rlib')), None)
                diagnostic = item.get('message')
                if isinstance(diagnostic, dict) and diagnostic.get('level') in ('error', 'warning'):
                    print(diagnostic.get('rendered', diagnostic.get('message', '')), file=sys.stderr)
                yield line
        scope = report.protected
        if binary:
            scope = consumer.manager_binary_frame
        elif package == 'lcxbox-agent-desk-server':
            scope = consumer.protected
        try:
            parsed = report.collect(lines(), constructors, scope)
        except report.NoStackRecords as error:
            code = process.wait()
            if code or not state['finished']:
                raise ValueError(f'cargo rustc did not finish the {package} {target_kind} build successfully (exit {code})') from error
            raise
        code = process.wait()
    if code or not state['finished'] or not state['fresh_artifact']:
        raise ValueError('analysis requires a successful, freshly code-generated selected artifact')
    if (not binary and package in ('desk-signal', 'lcxbox-agent-desk-manager')
            and FRESH_TASK_POLL not in parsed['frames'] and external_fresh_task_poll(state['rlib'])):
        # Use the host's workspace, target, profile and resolved core features.
        # A reference by itself has no frame size and cannot satisfy coverage.
        core = measure_external_core(workspace, profile, target, state['core_features'])
        parsed['external_core_features'] = core['resolved_features']
        parsed['external_polls'] = {symbols.execution_name(FRESH_TASK_POLL): core['frames'][FRESH_TASK_POLL]}
    if constructors and SCHEDULE_ERROR not in parsed['frames'] and state['rlib']:
        objdump = llvm_tool('objdump')
        if objdump:
            with subprocess.Popen([objdump, '-t', state['rlib']], stdout=subprocess.PIPE, text=True) as table:
                aliased = schedule_error_frame(table.stdout, constructors)
                if table.wait() == 0 and aliased:
                    size, symbol = aliased
                    parsed['frames'][SCHEDULE_ERROR] = size
                    if 'execution_names' in parsed:
                        parsed['execution_names'].append(SCHEDULE_ERROR)
                    parsed['schedule_error_frame_symbol'] = symbol
    parsed.update(cargo_profile=state['cargo_profile'], resolved_features=state['resolved_features'])
    return parsed


def build_configuration(workspace):
    # Record configuration that can affect codegen. Registry-only user
    # configuration does not change this build's stack layout.
    configurations = []
    workspace = workspace.resolve()
    cargo_home = Path(os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')))
    for directory in [cargo_home, *reversed([workspace, *workspace.parents])]:
        for name in ('config.toml', 'config'):
            path = directory / name if directory == cargo_home else directory / '.cargo' / name
            if not path.is_file():
                continue
            data = tomllib.loads(path.read_text())
            relevant = {key: data[key] for key in ('build', 'target', 'profile') if data.get(key)}
            if 'target' in relevant:
                relevant['target'] = {key: value for key, value in relevant['target'].items() if value}
                if not relevant['target']:
                    del relevant['target']
            # Ignore registry/network/env-only settings. The same complete
            # codegen table repeated in ancestor and child configs is recorded
            # once; neither path names nor unrelated auth settings are budgets.
            if relevant and relevant not in configurations:
                configurations.append(relevant)

    environment = {key: value for key, value in os.environ.items()
        if key.startswith(('CARGO_PROFILE_', 'CARGO_TARGET_')) and key.endswith(('OPT_LEVEL','LTO','CODEGEN_UNITS','PANIC','DEBUG','DEBUG_ASSERTIONS','OVERFLOW_CHECKS','RUSTFLAGS','RUSTDOCFLAGS','TARGET_FEATURES'))}
    manifest = tomllib.loads((workspace / "Cargo.toml").read_text())
    return dict(configurations=configurations, environment=environment, manifest_profiles=manifest.get("profile", {}))


def workspace_identity(workspace):
    workspace = workspace.resolve()
    return dict(workspace_context='manager-workspace' if (workspace / 'backend/manager').is_dir() else 'oss-workspace',
                lockfile_sha256=hashlib.sha256((workspace / 'Cargo.lock').read_bytes()).hexdigest())


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--workspace', type=Path, default=ROOT)
    p.add_argument('--package', required=True)
    p.add_argument('--bin', action='store_true', help='measure the production binary target of the same name')
    p.add_argument('--profile', choices=('debug', 'release'), default='debug')
    p.add_argument('--target')
    p.add_argument('--features', default='')
    p.add_argument('--no-default-features', action='store_true')
    p.add_argument('--ceiling', type=int, default=report.HARD_FRAME_LIMIT, help='fixed frame ceiling; defaults to the project hard limit')
    p.add_argument('--required', action='append', default=[], help='required symbol family regex; repeat as needed')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--report-only', action='store_true', help='collect evidence without enforcing the hard limit or coverage')
    a = p.parse_args()
    if a.ceiling is not None and not 0 < a.ceiling <= report.HARD_FRAME_LIMIT:
        p.error(f'--ceiling must be between 1 and {report.HARD_FRAME_LIMIT}')
    toolchain = subprocess.check_output(['rustc', '-Vv'], cwd=a.workspace, text=True).strip()
    target = a.target or next(line[6:] for line in toolchain.splitlines() if line.startswith('host: '))
    known_overflow_probe(a.workspace)
    parsed = collect_package(a.workspace, a.package, a.profile, a.target, a.features, a.no_default_features, a.bin)
    parsed = report.retain_entity_identity(parsed, a.package)
    parsed = report.retain_operation_identity(parsed, a.package)
    parsed.update(**workspace_identity(a.workspace))
    parsed.update(package=a.package, target_kind='bin' if a.bin else 'lib', profile=a.profile, target=target, toolchain=toolchain,
                  features=a.features, no_default_features=a.no_default_features,
                  rustflags=os.environ.get('RUSTFLAGS', ''),
                  encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS', ''),
                  build_configuration=build_configuration(a.workspace))
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(json.dumps(parsed, indent=2) + '\n')
    errors = []
    if not a.report_only and a.ceiling is not None:
        errors.extend(report.check_ceiling(parsed, a.ceiling, a.required))
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print(f'{a.package} {target} {a.profile}: {parsed["records"]} records, {len(parsed["frames"])} protected entries')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
