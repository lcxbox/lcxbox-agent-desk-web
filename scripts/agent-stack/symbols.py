"""Decode Rust v0 symbols in batches using the same demangler on every host."""
from functools import cache
import json
import re
from pathlib import Path
import subprocess
import tempfile


DECODE_ERRORS = re.compile(r"\{(?:invalid syntax|recursion limit reached|size limit reached)\}")


@cache
def demangler():
    # The analyzed package can hold the Cargo target lock while emitting remarks.
    # Build this host tool in a separate directory, including for cross-target runs.
    directory = tempfile.TemporaryDirectory(prefix='desk-stack-demangle-')
    crate = Path(__file__).resolve().parent / 'demangle'
    host = next(line[6:] for line in subprocess.check_output(
        ['rustc', '-Vv'], cwd=crate, text=True).splitlines() if line.startswith('host: '))
    build = subprocess.run(['cargo', 'build', '--locked', '--manifest-path', str(crate / 'Cargo.toml'),
                            '--target', host, '--target-dir', directory.name, '--message-format=json'],
                           cwd=crate, check=True, capture_output=True, text=True)
    artifacts = (json.loads(line) for line in build.stdout.splitlines())
    return directory, next(item['executable'] for item in artifacts if item.get('executable'))


def decode_v0(symbols):
    if not symbols:
        return []
    result = subprocess.run([demangler()[1]], input='\n'.join(symbols) + '\n',
                            capture_output=True, text=True)
    names = result.stdout.splitlines()
    if result.returncode or len(names) != len(symbols):
        raise ValueError('invalid or unsupported Rust v0 symbol: ' + result.stderr.strip())
    if any(DECODE_ERRORS.search(name) for name in names):
        raise ValueError('incomplete Rust v0 demangling in stack report')
    return names


def without_arguments(path, opening):
    pieces = []
    start = 0
    while (index := path.find(opening, start)) >= 0:
        pieces.append(path[start:index])
        depth = 1
        end = index + len(opening)
        while end < len(path) and depth:
            if path[end] == '<':
                depth += 1
            elif path[end] == '>' and path[end - 1] != '-':
                depth -= 1
            end += 1
        if depth:
            return None
        start = end
    return ''.join([*pieces, path[start:]])


def execution_name(name):
    """Identify the callee and poll depth independently of its generic arguments."""
    key = without_arguments(name, '::<')
    if key is None:
        return None
    if key.startswith('<'):
        depth = 0
        trait_start = None
        for i, char in enumerate(key):
            if char == '<':
                depth += 1
            elif char == '>' and key[i - 1] != '-':
                depth -= 1
                if not depth:
                    break
            elif depth == 1 and key.startswith(' as ', i):
                trait_start = i
        else:
            return None
        receiver = without_arguments(key[1:trait_start or i], '<')
        if not receiver or '::{closure#' in receiver:
            return None
        owner = receiver if trait_start is None else '_<' + receiver + key[trait_start:i] + '>'
        key = owner + key[i + 1:]
    if not key.removeprefix('_<').startswith(('desk_', 'lcxbox_', 'agent_stack_probe::')):
        return None
    return re.sub(r'\{closure#\d+\}', '_{{closure}}', key)


def unparameterized_owner(key):
    # Fully qualified trait requirements can specify concrete parameter types.
    # Broad family expressions must see just the method and its owner.
    if key.startswith('_<'):
        end = key.rfind('>::')
        if end >= 0:
            owner = without_arguments(key[2:end], '<')
            return '_<' + owner + key[end:] if owner else ''
    return key
