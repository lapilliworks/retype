#!/usr/bin/env python3
"""Actual Linux ENOSPC during undo, using a private namespace and synthetic files."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import tarfile
import tempfile
import time


def run(binary, args, env, timeout=20):
    process = subprocess.Popen([str(binary), *map(str, args)], env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
        raise AssertionError('bounded command timed out')
    return process.returncode, stdout.decode(), stderr.decode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    binary = args.binary.resolve()
    report = args.report.resolve()
    report.mkdir(parents=True, exist_ok=True)
    if not args.inside:
        result = subprocess.run(['unshare', '--user', '--map-root-user', '--mount', '--propagation', 'private',
            'python3', str(Path(__file__).resolve()), '--inside', '--binary', str(binary), '--report', str(report)], timeout=90)
        raise SystemExit(result.returncode)
    with tempfile.TemporaryDirectory(prefix='rt-fault-', dir='/tmp') as scratch:
        scratch = Path(scratch)
        volume = scratch / 'volume'
        volume.mkdir()
        subprocess.run(['mount', '-t', 'tmpfs', '-o', 'size=8m,nosuid,nodev,noexec', 'tmpfs', str(volume)], check=True)
        daemon = None
        try:
            source = volume / 'original.tar'
            with tarfile.open(source, 'w', format=tarfile.GNU_FORMAT) as package:
                entry = tarfile.TarInfo('payload')
                entry.size = 2 * 1024 * 1024
                entry.mtime = 1
                entry.mode = 0o600
                package.addfile(entry, io.BytesIO(b'x' * entry.size))
            original = hashlib.sha256(source.read_bytes()).hexdigest()
            config = scratch / 'config.toml'
            config.write_text("version = 'retype-config-1'\n[notify]\nenabled = false\n")
            state = scratch / 'state'
            env = os.environ.copy()
            env['RETYPE_CONFIG'] = str(config)
            for key in ['RETYPE_STATE_DIR', 'RETYPE_FIDELITY_MANIFEST', 'RETYPE_DAEMON', 'RETYPE_NO_SPAWN']:
                env.pop(key, None)
            daemon = subprocess.Popen([str(binary), 'daemon', '--detached', '--state-dir', str(state)], env=env,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            deadline = time.monotonic() + 5
            while not (state / 'endpoint').exists():
                assert daemon.poll() is None and time.monotonic() < deadline
                time.sleep(.02)
            code, stdout, stderr = run(binary, ['convert', source, 'zip', '--json', '--no-spawn', '--state-dir', state], env)
            assert code == 0, (stdout, stderr)
            job = json.loads(stdout)['job']
            assert not source.exists() and source.with_suffix('.zip').exists()
            fill = volume / 'fill'
            before = os.statvfs(volume)
            (report / 'space-before-fill.json').write_text(json.dumps({'free': before.f_bavail * before.f_frsize, 'inodes': before.f_favail,
                'files': [str(path.relative_to(volume)) for path in volume.rglob('*')]}, indent=2) + '\n')
            try:
                with fill.open('wb') as output:
                    while True:
                        output.write(b'x' * 65536)
                        output.flush()
            except OSError as error:
                assert error.errno == 28, error
            code, stdout, stderr = run(binary, ['undo', job['id'], '--json', '--no-spawn', '--state-dir', state], env)
            (report / 'undo-full.stdout.json').write_text(stdout)
            (report / 'undo-full.stderr.log').write_text(stderr)
            assert code != 0 and 'E_IO_SPACE' in stdout, (code, stdout, stderr)
            assert not source.exists() and source.with_suffix('.zip').exists()
            assert (state / 'undo' / job['id'] / 'source.tar').exists()
            code, stdout, stderr = run(binary, ['daemon', 'status', '--json', '--no-spawn', '--state-dir', state], env)
            assert code == 0, (stdout, stderr)
            fill.unlink()
            code, stdout, stderr = run(binary, ['undo', job['id'], '--json', '--no-spawn', '--state-dir', state], env)
            (report / 'undo-retry.stdout.json').write_text(stdout)
            assert code == 0, (stdout, stderr)
            assert hashlib.sha256(source.read_bytes()).hexdigest() == original
            assert not source.with_suffix('.zip').exists()
            assert not (state / 'undo' / job['id']).exists()
            (report / 'result.json').write_text(json.dumps({'restoration_enospc': 'passed', 'control_after_failure': 'passed',
                'exact_retry': 'passed', 'original_sha256': original}, indent=2) + '\n')
            print('Actual ENOSPC restoration failure, responsive status and exact-byte retry passed')
        finally:
            if daemon is not None and daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait()
            subprocess.run(['umount', str(volume)], check=True)


if __name__ == '__main__':
    main()
