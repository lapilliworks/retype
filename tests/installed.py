#!/usr/bin/env python3
"""Check an exact Linux release archive in a private installation prefix."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tarfile
import tempfile
import time


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--report', required=True, type=Path)
    args = parser.parse_args()
    archive = args.archive.resolve()
    report = args.report.resolve()
    report.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parent.parent
    assert archive.name.endswith('.tar.gz')
    with tarfile.open(archive) as package:
        roots = {Path(member.name).parts[0] for member in package.getmembers()}
    assert len(roots) == 1
    name = roots.pop()
    suffix = '-x86_64-unknown-linux-gnu' if 'x86_64' in name else '-aarch64-unknown-linux-gnu'
    assert name.startswith('retype-') and name.endswith(suffix)
    tag = name[len('retype-'):-len(suffix)]
    checks = []
    processes = []
    with tempfile.TemporaryDirectory(prefix='rt-installed-', dir='/tmp') as private:
        private = Path(private)
        work = private / 'work'
        work.mkdir()
        prefix = private / 'prefix'
        state = private / 'state'
        mirror = private / 'releases' / tag
        mirror.mkdir(parents=True)
        shutil.copy2(archive, mirror / archive.name)
        (mirror / 'SHA256SUMS').write_text(f'{sha(archive)}  {archive.name}\n')
        config = private / 'config.toml'
        config.write_text("version = 'retype-config-1'\n[notify]\nenabled = false\n[deferral]\nmode = 'now'\n")
        env = os.environ.copy()
        for key in ['RETYPE_DAEMON', 'RETYPE_NO_SPAWN', 'RETYPE_STATE_DIR', 'RETYPE_FIDELITY_MANIFEST', 'RETYPE_FIXTURES', 'RETYPE_TEST_SESSION']:
            env.pop(key, None)
        env['RETYPE_CONFIG'] = str(config)
        env['RETYPE_RELEASE_BASE'] = mirror.parent.as_uri()
        env['RETYPE_TARGET'] = suffix[1:]
        env['XDG_CONFIG_HOME'] = str(private / 'config')
        env['XDG_STATE_HOME'] = str(private / 'state-home')
        env['XDG_DATA_HOME'] = str(private / 'data')
        binary = prefix / 'bin/retype'

        def run(label, command, success=True, timeout=40):
            process = subprocess.Popen([str(part) for part in command], cwd=work, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            processes.append(process)
            try:
                stdout, stderr = process.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
                raise AssertionError(f'{label} timed out')
            (report / f'{label}.stdout.log').write_bytes(stdout)
            (report / f'{label}.stderr.log').write_bytes(stderr)
            assert (process.returncode == 0) == success, (label, process.returncode, stdout.decode(errors='replace'), stderr.decode(errors='replace'))
            return stdout.decode()

        def cli(label, *parts, success=True):
            result = json.loads(run(label, [binary, *parts, '--json', '--state-dir', state], success))
            assert result['ok'] == success, result
            return result

        def check(label, condition):
            assert condition, label
            checks.append(label)
            (report / 'checks.json').write_text(json.dumps(checks, indent=2) + '\n')
            print('PASS:', label, flush=True)

        def undo(label, job):
            cli(label, 'undo', job['id'], '--no-spawn')

        try:
            run('install', ['bash', repo / 'packaging/install.sh', '--version', tag, '--prefix', prefix])
            check('installed executable and evidence exist', binary.is_file() and (prefix / 'share/retype/fidelity.toml').is_file())
            run('version', [binary, 'version'])
            run('doctor', [binary, 'doctor', '--check', '--state-dir', state])
            heic = work / 'photo.HEIC'
            shutil.copy2(repo / 'fixtures/native-watch/photo.heic', heic)
            expected = sha(heic)
            job = cli('heic', 'convert', heic, 'jpg')['job']
            check('real HEIC disposition', job['source_disposition']['state'] == 'removed' and not heic.exists() and (work / 'photo.jpg').exists())
            undo('heic-undo', job)
            check('exact HEIC undo', sha(heic) == expected and not (work / 'photo.jpg').exists())
            png = work / 'image.png'
            shutil.copy2(repo / 'fixtures/fidelity/alpha.png', png)
            expected = sha(png)
            job = cli('multi-output', 'convert', png, 'jpg,webp+keep', '--no-spawn')['job']
            check('installed fidelity lookup', all(output['fidelity']['state'] == 'earned' for output in job['outputs']))
            undo('multi-output-undo', job)
            check('kept source and outputs after undo', sha(png) == expected and all(not Path(output['path']).exists() for output in job['outputs']))
            document = work / 'document.md'
            document.write_text('# Report\n\nA paragraph, **emphasis**, and $x^2$.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n')
            expected = sha(document)
            job = cli('pdf', 'convert', document, 'pdf', '--no-spawn')['job']
            check('selected Typst produces PDF', Path(job['engine']['candidate']).name == 'typst' and (work / 'document.pdf').read_bytes().startswith(b'%PDF'))
            undo('pdf-undo', job)
            check('document undo', sha(document) == expected and not (work / 'document.pdf').exists())
            source = work / 'archive.tar'
            with tarfile.open(source, 'w', format=tarfile.GNU_FORMAT) as package:
                member = tarfile.TarInfo('payload.txt')
                member.mode = 0o600
                member.mtime = 1700000000
                member.size = 7
                package.addfile(member, io.BytesIO(b'payload'))
            expected = sha(source)
            job = cli('archive', 'convert', source, 'zip', '--no-spawn')['job']
            undo('archive-undo', job)
            check('native archive exact undo', sha(source) == expected and not source.with_suffix('.zip').exists())
            damaged = work / 'bad.heic'
            damaged.write_bytes(b'invalid source')
            cli('decoder-refusal', 'convert', damaged, 'jpg', '--no-spawn', success=False)
            check('failure preserves source', damaged.read_bytes() == b'invalid source' and not damaged.with_suffix('.jpg').exists())
            before = sha(binary)
            run('update', ['bash', repo / 'packaging/install.sh', '--version', tag, '--prefix', prefix])
            check('update backup', sha(binary) == before and any((backup / 'retype').is_file() and sha(backup / 'retype') == before for backup in (prefix / 'share').glob('retype-backup.*')))
            (mirror / 'SHA256SUMS').write_text(f'{"0" * 64}  {archive.name}\n')
            run('checksum-refusal', ['bash', repo / 'packaging/install.sh', '--version', tag, '--prefix', prefix], False)
            check('checksum failure preserves installation', sha(binary) == before)
        finally:
            for process in processes:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            if binary.exists() and (state / 'endpoint').exists():
                run('stop', [binary, 'daemon', 'stop', '--now', '--force', '--no-spawn', '--state-dir', state], timeout=15)
                deadline = time.monotonic() + 5
                while (state / 'endpoint').exists() and time.monotonic() < deadline:
                    time.sleep(.05)
                assert not (state / 'endpoint').exists(), 'test daemon leaked'
    (report / 'result.json').write_text(json.dumps({'checks': len(checks), 'archive': str(archive), 'sha256': sha(archive)}, indent=2) + '\n')
    print(f'{len(checks)} installed-archive checks passed')


if __name__ == '__main__':
    main()
