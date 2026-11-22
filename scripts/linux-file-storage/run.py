#!/usr/bin/env python3
"""Run a production storage test on disposable, size-limited filesystems."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import uuid

TEST = ('worker::agent::file_reference_store::text_mutation::tests::'
        'full_disk::full_backup_filesystem_preserves_target')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true', required=True)
    parser.add_argument('--test-binary', type=Path, required=True)
    parser.add_argument('--target-ext4', action='store_true', help='Fill an isolated 64 MiB ext4 image, not the backup tmpfs')
    parser.add_argument('--commit-full', action='store_true', help='Fill ext4 after staging, then use the real rename path')
    args = parser.parse_args()
    if args.commit_full and not args.target_ext4:
        parser.error('--commit-full requires --target-ext4')
    test = TEST if not args.target_ext4 else TEST.replace('full_backup_filesystem_preserves_target', 'full_target_filesystem_preserves_target')
    if args.commit_full:
        test = TEST.replace('full_backup_filesystem_preserves_target', 'full_target_after_staging_verifies_real_commit')
    binary = args.test_binary.resolve(strict=True)
    if not binary.name.startswith('lcxbox_agent_desk_server-'):
        parser.error('expected the current server lib test binary')
    linked = subprocess.check_output(['ldd', str(binary)], text=True, timeout=10)
    if 'not found' in linked:
        raise RuntimeError('test binary has unresolved dependencies')
    libraries = sorted({Path(p) for p in
                        re.findall(r'(/[^\s]+)\s+\(0x[0-9a-f]+\)', linked)})
    if not libraries:
        raise RuntimeError('no dynamic runtime dependencies found')
    name = 'lcxbox-file-enospc-' + uuid.uuid4().hex[:12]
    fixtures = Path(__file__).resolve().parents[2] / 'target/assistant-validation-tmp'
    fixtures.mkdir(parents=True, exist_ok=True)
    fixture = Path(tempfile.mkdtemp(prefix='linux-enospc-', dir=fixtures))
    command = ['docker', 'run', '--rm', '--pull=never', '--name', name,
               '--network=none', '--read-only', '--cap-drop=ALL',
               '--security-opt=no-new-privileges', '--pids-limit=64', '--memory=512m',
               '--env', 'LCXBOX_ISOLATED_FULL_DISK=1',
               '--env', 'RUST_MIN_STACK=8388608',
               '--mount', f'type=bind,src={binary},dst=/validation/server-tests,readonly']
    if args.target_ext4:
        script = Path(__file__).with_name('ext4-volume.sh').resolve()
        for cap in ['SYS_ADMIN', 'MKNOD', 'CHOWN', 'SETUID', 'SETGID', 'SETPCAP']:
            command += ['--cap-add', cap]
        command += ['--security-opt=apparmor=unconfined', '--device=/dev/loop-control',
                    '--device-cgroup-rule=b 7:* rwm',
                    '--tmpfs=/disk:rw,noexec,nosuid,nodev,size=80m',
                    '--tmpfs=/fixture:rw,noexec,nosuid,nodev,size=1m',
                    '--env=LCXBOX_ISOLATED_EXT4=1',
                    '--env', f'LCXBOX_TEST_UID={os.getuid()}', '--env', f'LCXBOX_TEST_GID={os.getgid()}',
                    '--mount', f'type=bind,src={script},dst=/validation/ext4-volume.sh,readonly']
    else:
        command += ['--user', f'{os.getuid()}:{os.getgid()}',
                    '--mount', f'type=bind,src={fixture},dst=/fixture']
    for library in libraries:
        command += ['--mount', f'type=bind,src={library.resolve(strict=True)},dst={library},readonly']
    for directory, size in [('backup', '1m'), ('tmp', '1m')]:
        command += ['--tmpfs', f'/{directory}:rw,noexec,nosuid,nodev,size={size},mode=0700,uid={os.getuid()},gid={os.getgid()}']
    if args.target_ext4:
        command += ['--entrypoint', '/bin/sh', 'ubuntu:24.04', '/validation/ext4-volume.sh', '/validation/server-tests']
    else:
        command += ['--entrypoint', '/validation/server-tests', 'ubuntu:24.04']
    command += ['--ignored', '--exact', test, '--test-threads=1', '--nocapture']
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=60)
        output = result.stdout + result.stderr
        if result.returncode or '1 passed; 0 failed' not in output:
            raise RuntimeError('isolated ENOSPC test failed: ' + output[-4000:])
    finally:
        subprocess.run(['docker', 'rm', '-f', name], capture_output=True, timeout=15)
        remaining = subprocess.check_output(
            ['docker', 'ps', '-a', '--filter', 'name=^/' + name + '$', '--format', '{{.Names}}'],
            text=True, timeout=10)
        if remaining.strip():
            raise RuntimeError('test container cleanup incomplete')
        shutil.rmtree(fixture)
    if args.commit_full:
        print(json.dumps(dict(test=test, passed=True, filesystem='ext4', targetImageBytes=67108864,
                             changes=['update', 'delete'], fillerKernelError='ENOSPC',
                             realRenameCommitVerified=True, completedBackupPreserved=True,
                             containerRemoved=True)))
        return
    if args.target_ext4:
        print(json.dumps(dict(test=test, passed=True, filesystem='ext4', targetImageBytes=67108864,
                             changes=['update-directory', 'delete-directory', 'update-staging'], kernelError='ENOSPC',
                             nativeSubmissions=0, originalContentAndIdentityPreserved=True,
                             completedBackupPreserved=True, containerRemoved=True)))
        return
    print(json.dumps(dict(test=test, passed=True, filesystem='tmpfs', backupLimitBytes=1048576,
                         changes=['update', 'delete'], kernelError='ENOSPC',
                         nativeSubmissions=0, originalContentAndIdentityPreserved=True,
                         incompleteBackupNotPublished=True, containerRemoved=True)))


if __name__ == '__main__':
    main()
