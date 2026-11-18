"""Build and publish the reviewed compiler tool without changing its consumers."""
import argparse
import contextlib
from datetime import datetime, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tarfile
import time
import urllib.request
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
CONFIG = '.github/compiler-cache/response-files.json'
INPUTS = [CONFIG, '.github/compiler-cache/response-files.patch',
          '.github/scripts/compiler_cache_tool.py', '.github/workflows/compiler-cache-tool.yml']
REPOSITORY = 'sudoprivacy/sudocode'


def sha(contents):
    return hashlib.sha256(contents).hexdigest()


def write_json(path, value):
    Path(path).write_bytes((json.dumps(value, indent=2) + '\n').encode())


def clean_environment():
    return {key: value for key, value in os.environ.items()
            if key.upper() not in {'GH_TOKEN', 'GITHUB_TOKEN', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'}
            and not key.upper().startswith(('GIT_', 'SCCACHE_'))}


def git(*arguments):
    return subprocess.check_output(['git', '--no-replace-objects', *arguments], cwd=ROOT, env=clean_environment())


def blob(path, ref='HEAD'):
    return git('show', '--no-ext-diff', '--no-textconv', ref + ':' + path)


def configuration():
    value = json.loads(blob(CONFIG))
    assert value['schema_version'] == 1 and value['version'] == '0.19.0'
    assert value['target'] == 'x86_64-pc-windows-msvc'
    assert re.fullmatch(r'scode-responsefiles\.[1-9][0-9]*', value['variant'])
    assert value['member_root'] == 'sccache-v' + value['version'] + '-' + value['variant'] + '-' + value['target']
    assert value['archive_name'] == value['member_root'] + '.zip'
    assert value['release_tag'] == 'ci-sccache-v' + value['version'] + '-' + value['variant'].removeprefix('scode-')
    assert value['patch_path'] == '.github/compiler-cache/response-files.patch'
    for name in ['source_archive_sha256', 'patch_sha256', 'original_rust_sha256', 'patched_rust_sha256', 'baseline_archive_sha256']:
        assert re.fullmatch('[0-9a-f]{64}', value[name])
    for name in ['source_commit', 'upstream_fix_commit', 'baseline_helper_ref']:
        assert re.fullmatch('[0-9a-f]{40}', value[name])
    assert set(value['compatibility_files']) == {'tests/system.rs'}
    for pins in value['compatibility_files'].values():
        assert set(pins) == {'original_sha256', 'patched_sha256'}
        assert all(re.fullmatch('[0-9a-f]{64}', digest) for digest in pins.values())
    assert sha(blob(value['patch_path'])) == value['patch_sha256']
    return value


def input_hashes():
    result = {}
    for name in INPUTS:
        canonical = blob(name)
        actual = (ROOT / name).read_bytes()
        assert actual.replace(b'\r\n', b'\n') == canonical.replace(b'\r\n', b'\n'), name
        result[name] = sha(canonical)
    return result


def source_manifest(root):
    return {path.relative_to(root).as_posix(): sha(path.read_bytes())
            for path in sorted(root.rglob('*')) if path.is_file()}


def prepare(output):
    config = configuration()
    output.mkdir(parents=True, exist_ok=False)
    archive = output / 'original-source.tar.gz'
    url = 'https://codeload.github.com/mozilla/sccache/tar.gz/' + config['source_commit']
    with urllib.request.urlopen(url, timeout=60) as response:
        contents = response.read(32 * 1024 * 1024 + 1)
    assert len(contents) <= 32 * 1024 * 1024 and sha(contents) == config['source_archive_sha256']
    archive.write_bytes(contents)
    source = output / 'source'
    source.mkdir()
    with tarfile.open(fileobj=io.BytesIO(contents), mode='r:gz') as packed:
        members = packed.getmembers()
        prefixes = {Path(member.name).parts[0] for member in members}
        assert len(prefixes) == 1
        prefix = next(iter(prefixes))
        packed.extractall(source, filter='data')
    project = source / prefix
    before = source_manifest(project)
    assert before['src/compiler/rust.rs'] == config['original_rust_sha256']
    for name, pins in config['compatibility_files'].items():
        assert before[name] == pins['original_sha256']
    patch = output / 'response-files.patch'
    patch.write_bytes(blob(config['patch_path']))
    for label, arguments in [('check', ['--check']), ('apply', [])]:
        result = subprocess.run(['git', '-c', 'core.autocrlf=false', 'apply', '--unidiff-zero', *arguments, str(patch.resolve())],
                                cwd=project, env=clean_environment(), capture_output=True, timeout=45)
        (output / ('patch-' + label + '.stdout')).write_bytes(result.stdout)
        (output / ('patch-' + label + '.stderr')).write_bytes(result.stderr)
        assert result.returncode == 0, 'Pinned source patch ' + label + ' failed'
    after = source_manifest(project)
    assert before.keys() == after.keys()
    assert [name for name in before if before[name] != after[name]] == sorted(['src/compiler/rust.rs', *config['compatibility_files']])
    assert after['src/compiler/rust.rs'] == config['patched_rust_sha256']
    for name, pins in config['compatibility_files'].items():
        assert after[name] == pins['patched_sha256']
    result = {'project': str(project.resolve()), 'source_manifest': after,
              'source_tree_sha256': sha(json.dumps(after, sort_keys=True, separators=(',', ':')).encode()),
              'source_inputs': input_hashes(), 'producer_head': git('rev-parse', 'HEAD').decode().strip()}
    write_json(output / 'prepared.json', result)
    return result


def original_runner(config, output):
    path = '.github/scripts/local_acceptance.py'
    contents = blob(path, config['baseline_helper_ref'])
    staged = output / 'runner/.github/scripts/local_acceptance.py'
    staged.parent.mkdir(parents=True)
    staged.write_bytes(contents)
    spec = importlib.util.spec_from_file_location('reviewed_tool_runner', staged)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = ROOT
    return helper, sha(contents)


def baseline_tool(path, config):
    tool = json.loads(path.read_bytes())['compiler_cache']
    archive = Path(tool['archive'])
    assert tool['version'] == config['version'] and tool['target'] == config['target']
    assert tool['sha256'] == config['baseline_archive_sha256'] == sha(archive.read_bytes())
    assert not archive.is_symlink() and not Path(tool['executable']).is_symlink()
    with zipfile.ZipFile(archive) as packed:
        entries = [entry for entry in packed.infolist() if entry.filename == tool['member']]
        assert len(entries) == 1 and not entries[0].is_dir()
        binary = packed.read(entries[0])
    assert sha(binary) == tool['executable_sha256'] and Path(tool['executable']).read_bytes() == binary
    return tool


def normal_driver_priority():
    import psutil
    process = psutil.Process()
    before = process.nice()
    # Hosted shells do not promise Normal inheritance. Change only this driver;
    # the existing command runner still lowers actual compiler/linker workers.
    process.nice(psutil.NORMAL_PRIORITY_CLASS)
    after = process.nice()
    assert after == psutil.NORMAL_PRIORITY_CLASS
    return {'pid': process.pid, 'birth': process.create_time(), 'before': int(before), 'after': int(after)}


def qualify(args):
    import _thread
    import psutil
    import threading

    config = configuration()
    prepared = json.loads((args.output / 'prepared.json').read_bytes())
    project = Path(prepared['project'])
    evidence = args.output / 'evidence'
    evidence.mkdir()
    state = {'started_at': datetime.now(timezone.utc).isoformat(), 'complete': False, 'passed': False,
             'producer_head': prepared['producer_head'], 'source_inputs': prepared['source_inputs'],
              'runner_ref': config['baseline_helper_ref'],
             'source_tree_sha256': prepared['source_tree_sha256'], 'commands': [],
             'attempt': {'active_profile': 'compiler-cache-tool-qualification'},
             'normal_product_acceptance': False, 'whole_lifecycle_tenth_proven': False}
    stop = threading.Event()
    ambient = os.environ.copy()

    def fence():
        high = 0
        while not stop.wait(10):
            cpu = psutil.cpu_percent(interval=1)
            available = psutil.virtual_memory().available / 2**30
            high = high + 1 if cpu >= 90 else 0
            if available < 8 or high >= 3:
                state['resource_stop'] = {'cpu_percent': cpu, 'available_gib': available}
                _thread.interrupt_main()
                return

    def execute(label, arguments):
        state['phase'] = label
        write_json(evidence / 'qualification.json', state)
        log = evidence / (label + '.log')
        try:
            receipt = helper.run_command(arguments, project, log)
        finally:
            record = log.with_suffix('.command.json')
            if record.exists():
                state['commands'].append({'label': label, 'receipt': json.loads(record.read_bytes())})
            write_json(evidence / 'qualification.json', state)
        assert source_manifest(project) == prepared['source_manifest'], 'Upstream source changed during qualification'
        return receipt

    try:
        assert not any(key.upper() in {'GH_TOKEN', 'GITHUB_TOKEN'} for key in os.environ), 'Tool builds must not receive GitHub credentials'
        assert os.name == 'nt', 'This artifact qualifies only Windows x64'
        assert source_manifest(project) == prepared['source_manifest']
        assert input_hashes() == prepared['source_inputs']
        state['driver_priority'] = normal_driver_priority()
        helper, runner_hash = original_runner(config, evidence)
        state['runner_sha256'] = runner_hash
        baseline = baseline_tool(args.baseline_tool, config)
        os.environ.clear()
        os.environ.update(clean_environment_from(ambient))
        os.environ.update(RUSTUP_TOOLCHAIN=config['rust_toolchain'], CARGO_BUILD_JOBS='1',
                          SCODE_ACCEPTANCE_BUILD_JOBS='1', CARGO_INCREMENTAL='0',
                          CMAKE_BUILD_PARALLEL_LEVEL='1', NUM_JOBS='1',
                          CARGO_TARGET_DIR=str(args.target_dir.resolve()), RUSTC_WRAPPER='', RUSTC_WORKSPACE_WRAPPER='',
                          RUSTFLAGS='', CARGO_ENCODED_RUSTFLAGS='\x1f'.join([
                              '-Ctarget-feature=+crt-static', '--cfg',
                              'scode_compiler_cache_tree="' + prepared['source_tree_sha256'] + '"']))
        with helper.host_acceptance_lease(state['attempt'], prepared['producer_head']):
            if not os.environ.get('GITHUB_ACTIONS'):
                cpu = psutil.cpu_percent(interval=1)
                assert cpu < 90 and psutil.virtual_memory().available >= 8 * 2**30, 'Defer owned tool build for foreground resources'
                threading.Thread(target=fence, daemon=True).start()
            rustc = subprocess.check_output(['rustup', 'which', 'rustc', '--toolchain', config['rust_toolchain']], env=os.environ).decode().strip()
            state['baseline_control'] = runtime_probe(None, Path(baseline['executable']), Path(rustc), evidence / 'baseline-control')
            execute('format', ['cargo', 'fmt', '--', '--check'])
            target = ['--target', config['target']]
            execute('clippy', ['cargo', 'clippy', '--locked', '--all-targets', *target, '--', '-D', 'warnings',
                               '-A', 'unknown-lints', '-A', 'clippy::type_complexity', '-A', 'clippy::new-without-default'])
            execute('upstream-tests', ['cargo', 'test', '--locked', '--lib', '--bins', '--tests', *target])
            test_output = re.sub(r'\x1b\[[0-9;]*m', '', (evidence / 'upstream-tests.log').read_text(encoding='utf-8', errors='replace'))
            for name in config['required_upstream_regressions']:
                assert re.search(r'\b' + re.escape(name) + r'\s+\.\.\.\s+ok\b', test_output), 'Required real upstream test did not pass: ' + name
            state['required_upstream_regressions_passed'] = config['required_upstream_regressions']
            execute('release', ['cargo', 'build', '--locked', '--release', '--message-format=json-render-diagnostics', *target])
            records = []
            for line in (evidence / 'release.log').read_text(encoding='utf-8', errors='replace').splitlines():
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if row.get('reason') == 'compiler-artifact' and row['target']['name'] == 'sccache':
                    records.append(row)
            assert any('lib' in row['target']['kind'] and not row['fresh'] for row in records), 'Missing original fresh sccache library compile'
            binaries = [row for row in records if 'bin' in row['target']['kind'] and not row['fresh']]
            assert len(binaries) == 1
            binary = Path(binaries[0]['executable'])
            assert binary.resolve().is_relative_to(args.target_dir.resolve())
            execute('tool-version', [str(binary), '--version'])
            assert (evidence / 'tool-version.log').read_text(encoding='utf-8').strip() == 'sccache ' + config['version']
            if helper.MSVC_ENV:
                os.environ.update(clean_environment_from(helper.MSVC_ENV))
            state['runtime_probe'] = runtime_probe(binary, None, Path(rustc), evidence / 'runtime', baseline=state['baseline_control'])
            state['fresh_tool_artifacts'] = records
            state['binary_sha256'] = sha(binary.read_bytes())
            package(config, prepared, binary, state, args.output)
            state['passed'] = True
            state['outcome'] = 'passed'
    except BaseException as error:
        state['error'] = repr(error)
        state['outcome'] = ('foreground-deferred' if state.get('resource_stop') else
                            'interrupted' if isinstance(error, KeyboardInterrupt) else 'failed')
        raise
    finally:
        stop.set()
        os.environ.clear()
        os.environ.update(ambient)
        state.update(complete=True, finished_at=datetime.now(timezone.utc).isoformat())
        write_json(evidence / 'qualification.json', state)


def clean_environment_from(environment):
    return {key: value for key, value in environment.items()
            if key.upper() not in {'GH_TOKEN', 'GITHUB_TOKEN', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'}
            and not key.upper().startswith(('GIT_', 'SCCACHE_'))}


class CacheProbe:
    """Own one foreground test server and private storage, never a shared daemon."""

    def __init__(self, binary, output):
        import psutil
        self.binary, self.output = binary.resolve(), output
        self.output.mkdir(parents=True)
        self.binary_hash = sha(self.binary.read_bytes())
        self.binary_identity = self.file_identity()
        self.environment = clean_environment()
        configuration = output / 'cache.toml'
        configuration.write_bytes(('[cache.disk]\ndir = ' + json.dumps(str((output / 'objects').resolve()))
                                   + '\nsize = 2147483648\n').encode())
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            self.port = reservation.getsockname()[1]
        self.environment.update(SCCACHE_CONF=str(configuration.resolve()), SCCACHE_DIR=str((output / 'objects').resolve()),
                                SCCACHE_CACHED_CONF=str((output / 'cached-config').resolve()),
                                SCCACHE_SERVER_PORT=str(self.port), SCCACHE_IDLE_TIMEOUT='600', SCCACHE_DIRECT='false',
                                SCCACHE_CLIENT_SIDE='1', SCODE_COMPILER_TOOL_PROBE=uuid.uuid4().hex)
        self.commands = []
        self.stream = (output / 'server.log').open('wb')
        self.server = subprocess.Popen([str(self.binary)], env={**self.environment, 'SCCACHE_START_SERVER': '1', 'SCCACHE_NO_DAEMON': '1'},
                                       stdout=self.stream, stderr=subprocess.STDOUT,
                                       creationflags=psutil.BELOW_NORMAL_PRIORITY_CLASS)
        self.birth = psutil.Process(self.server.pid).create_time()
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                self.owner()
                if self.listening():
                    break
                time.sleep(0.05)
            else:
                raise RuntimeError('Private source-qualified compiler server did not listen')
        except BaseException:
            self.close()
            raise

    def file_identity(self):
        value = self.binary.stat()
        return value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns

    def owner(self, check_file=True):
        import psutil
        assert self.server.poll() is None, 'Private compiler server exited; do not contact another endpoint'
        process = psutil.Process(self.server.pid)
        assert process.create_time() == self.birth and Path(process.exe()).resolve() == self.binary
        assert process.environ().get('SCODE_COMPILER_TOOL_PROBE') == self.environment['SCODE_COMPILER_TOOL_PROBE']
        assert process.nice() == psutil.BELOW_NORMAL_PRIORITY_CLASS
        if check_file:
            assert self.file_identity() == self.binary_identity
        return process

    def listening(self):
        import psutil
        return any(connection.status == psutil.CONN_LISTEN and connection.laddr.ip == '127.0.0.1'
                   and connection.laddr.port == self.port for connection in self.owner().net_connections(kind='tcp'))

    def run(self, label, arguments, extra=None, success=True):
        import psutil
        assert self.listening()
        result = subprocess.run([str(self.binary), *map(str, arguments)], env={**self.environment, **(extra or {})},
                                capture_output=True, timeout=60, creationflags=psutil.BELOW_NORMAL_PRIORITY_CLASS)
        (self.output / (label + '.stdout')).write_bytes(result.stdout)
        (self.output / (label + '.stderr')).write_bytes(result.stderr)
        row = {'label': label, 'exit_code': result.returncode, 'stdout_sha256': sha(result.stdout), 'stderr_sha256': sha(result.stderr)}
        self.commands.append(row)
        write_json(self.output / 'commands.json', self.commands)
        if success:
            assert result.returncode == 0, (label, 'Original output retained')
        return result

    def stats(self, label):
        result = json.loads(self.run(label, ['--show-stats', '--stats-format', 'json']).stdout)
        stats = result['stats']
        return {name: sum(stats[name]['counts'].values()) for name in ['cache_hits', 'cache_misses']}

    def close(self):
        import psutil
        try:
            if self.server.poll() is None:
                process = self.owner(check_file=False)
                if self.file_identity() == self.binary_identity and self.listening():
                    self.run('stop', ['--stop-server'])
                else:
                    process.terminate()
                self.server.wait(timeout=5)
        except BaseException:
            with contextlib.suppress(psutil.NoSuchProcess):
                process = psutil.Process(self.server.pid)
                assert process.create_time() == self.birth and Path(process.exe()).resolve() == self.binary
                assert process.environ().get('SCODE_COMPILER_TOOL_PROBE') == self.environment['SCODE_COMPILER_TOOL_PROBE']
                process.terminate()
                process.wait(timeout=5)
            raise
        finally:
            self.stream.close()
        assert sha(self.binary.read_bytes()) == self.binary_hash


def runtime_probe(binary, original_binary, rustc, output, baseline=None):
    import psutil
    output.mkdir()
    source = output / 'probe.rs'
    source.write_bytes(b'pub fn value() -> (&\'static str, bool) { (env!("SCODE_CACHE_TOOL_INPUT"), cfg!(feature="enabled")) }\n')
    cfgs = [f'--cfg=feature="some-feature-{number}"' for number in range(1200)]
    common = ['--crate-name', 'response_probe', '--crate-type=rlib', '--emit=link', str(source.resolve()), *cfgs]
    assert sum(len(argument.encode('utf-16-le')) // 2 + 1 for argument in common) > 32767

    def arguments(directory, enabled=False):
        directory.mkdir(exist_ok=True)
        values = [*common, '--out-dir', str(directory.resolve()), *(['--cfg=feature="enabled"'] if enabled else [])]
        path = directory / 'compiler.args'
        path.write_bytes(('\n'.join(values) + '\n').encode())
        return path

    def consume(directory, expected, enabled, label):
        consumer = output / (label + '.rs')
        code = '''#[link(name="kernel32")]
unsafe extern "system" { fn GetCurrentProcess() -> *mut std::ffi::c_void; fn GetPriorityClass(process: *mut std::ffi::c_void) -> u32; }
fn main() {
    assert_eq!(unsafe { GetPriorityClass(GetCurrentProcess()) }, 0x20);
    assert_eq!(response_probe::value(), (EXPECTED, ENABLED));
}
'''.replace('EXPECTED', json.dumps(expected)).replace('ENABLED', str(enabled).lower())
        consumer.write_bytes(code.encode())
        executable = output / (label + '.exe')
        result = subprocess.run([str(rustc), '--edition=2024', '--crate-name', 'cache_consumer', str(consumer),
                                 '--extern', 'response_probe=' + str(directory / 'libresponse_probe.rlib'), '-o', str(executable)],
                                env=clean_environment(), capture_output=True, timeout=60,
                                creationflags=psutil.BELOW_NORMAL_PRIORITY_CLASS)
        (output / (label + '-compile.stdout')).write_bytes(result.stdout)
        (output / (label + '-compile.stderr')).write_bytes(result.stderr)
        assert result.returncode == 0, 'Real compiler-cache consumer did not compile'
        result = subprocess.run([str(executable)], env=clean_environment(), capture_output=True, timeout=30,
                                creationflags=psutil.NORMAL_PRIORITY_CLASS)
        assert result.returncode == 0, 'Real compiler-cache consumer or Normal-priority assertion failed'

    if baseline is None:
        old = CacheProbe(original_binary, output / 'official-control')
        try:
            path = arguments(output / 'official-output')
            result = old.run('original-long-command', [rustc, '@' + str(path.resolve())],
                             {'SCODE_CACHE_TOOL_INPUT': 'alpha'}, success=False)
            assert result.returncode != 0 and b'206' in result.stderr, 'Official0.19 negative control did not reproduce the long-command refusal'
            baseline = {'official_long_command_refused_206': True, 'binary_sha256': old.binary_hash,
                        'original_commands': str(old.output / 'commands.json')}
        finally:
            old.close()
    assert baseline['official_long_command_refused_206']
    if binary is None:
        return baseline
    new = CacheProbe(binary, output / 'patched')
    try:
        directory = output / 'patched-output'
        path = arguments(directory)
        new.run('cold', [rustc, '@' + str(path.resolve())], {'SCODE_CACHE_TOOL_INPUT': 'alpha'})
        consume(directory, 'alpha', False, 'cold-consumer')
        cold = new.stats('cold-stats')
        assert cold['cache_misses'] > 0
        new.run('warm', [rustc, '@' + str(path.resolve())], {'SCODE_CACHE_TOOL_INPUT': 'alpha'})
        warm = new.stats('warm-stats')
        assert warm['cache_hits'] > cold['cache_hits']
        new.run('changed-environment', [rustc, '@' + str(path.resolve())], {'SCODE_CACHE_TOOL_INPUT': 'beta'})
        consume(directory, 'beta', False, 'environment-consumer')
        changed_env = new.stats('environment-stats')
        assert changed_env['cache_misses'] > warm['cache_misses']
        path = arguments(directory, enabled=True)
        new.run('changed-feature', [rustc, '@' + str(path.resolve())], {'SCODE_CACHE_TOOL_INPUT': 'beta'})
        consume(directory, 'beta', True, 'feature-consumer')
        changed_feature = new.stats('feature-stats')
        assert changed_feature['cache_misses'] > changed_env['cache_misses']
    finally:
        new.close()
    return {'official_long_command_refused_206': True, 'patched_long_command_passed': True,
            'cold': cold, 'warm': warm, 'changed_environment': changed_env, 'changed_feature': changed_feature,
            'consumer_normal_priority_and_original_values': True, 'population_speedup_proven': False}


def package(config, prepared, binary, qualification, output):
    destination = output / 'artifacts'
    destination.mkdir()
    identity = {'schema_version': 1, 'producer_head': prepared['producer_head'], 'source_inputs': prepared['source_inputs'],
                'source_commit': config['source_commit'], 'upstream_fix_commit': config['upstream_fix_commit'],
                'patch_sha256': config['patch_sha256'], 'source_tree_sha256': prepared['source_tree_sha256'],
                'rust_toolchain': config['rust_toolchain'], 'target': config['target'],
                'binary_sha256': sha(binary.read_bytes()), 'runtime_probe': qualification['runtime_probe'],
                'required_upstream_regressions_passed': qualification['required_upstream_regressions_passed'],
                'runner_ref': qualification['runner_ref'], 'runner_sha256': qualification['runner_sha256'],
                'scope': 'Compiler-tool artifact qualification; not SudoCode source/installed/product/production acceptance.'}
    archive = destination / config['archive_name']
    license_bytes = (Path(prepared['project']) / 'LICENSE').read_bytes()
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as packed:
        prefix = config['member_root'] + '/'
        packed.writestr(prefix + 'sccache.exe', binary.read_bytes())
        packed.writestr(prefix + 'LICENSE', license_bytes)
        packed.writestr(prefix + 'source.json', (json.dumps(identity, indent=2) + '\n').encode())
    with zipfile.ZipFile(archive) as packed:
        assert packed.testzip() is None and packed.read(config['member_root'] + '/sccache.exe') == binary.read_bytes()
    shutil.copyfile(output / 'original-source.tar.gz', destination / 'original-source.tar.gz')
    shutil.copyfile(output / 'response-files.patch', destination / 'response-files.patch')
    (destination / 'LICENSE').write_bytes(license_bytes)
    files = {path.name: sha(path.read_bytes()) for path in destination.iterdir()}
    manifest = {**identity, 'archive_name': config['archive_name'], 'member_root': config['member_root'],
                'version': config['version'], 'variant': config['variant'], 'release_tag': config['release_tag'],
                'qualified': True, 'files': files}
    write_json(destination / 'manifest.json', manifest)
    files['manifest.json'] = sha((destination / 'manifest.json').read_bytes())
    (destination / 'SHA256SUMS').write_bytes(''.join(f'{digest}  {name}\n' for name, digest in sorted(files.items())).encode())


def verify_payload(folder, extras=()):
    config = configuration()
    manifest = json.loads((folder / 'manifest.json').read_bytes())
    files = {config['archive_name'], 'original-source.tar.gz', 'response-files.patch', 'LICENSE', 'manifest.json', 'SHA256SUMS'}
    assert {path.name for path in folder.iterdir()} == files | set(extras)
    assert all(path.is_file() and not path.is_symlink() for path in folder.iterdir())
    assert manifest['qualified'] and manifest['source_inputs'] == input_hashes()
    for key in ['version', 'variant', 'target', 'release_tag', 'archive_name', 'member_root', 'source_commit', 'upstream_fix_commit', 'patch_sha256', 'rust_toolchain']:
        assert manifest[key] == config[key], key
    assert manifest['runner_ref'] == config['baseline_helper_ref']
    assert manifest['required_upstream_regressions_passed'] == config['required_upstream_regressions']
    assert manifest['runner_sha256'] == sha(blob('.github/scripts/local_acceptance.py', config['baseline_helper_ref']))
    assert re.fullmatch('[0-9a-f]{40}', manifest['producer_head'])
    assert set(manifest['files']) == files - {'manifest.json', 'SHA256SUMS'}
    for name, digest in manifest['files'].items():
        assert sha((folder / name).read_bytes()) == digest, name
    assert manifest['files']['original-source.tar.gz'] == config['source_archive_sha256']
    assert manifest['files']['response-files.patch'] == config['patch_sha256']
    expected = {**manifest['files'], 'manifest.json': sha((folder / 'manifest.json').read_bytes())}
    assert (folder / 'SHA256SUMS').read_bytes() == ''.join(f'{digest}  {name}\n' for name, digest in sorted(expected.items())).encode()
    with zipfile.ZipFile(folder / config['archive_name']) as packed:
        prefix = config['member_root'] + '/'
        assert packed.namelist() == [prefix + 'sccache.exe', prefix + 'LICENSE', prefix + 'source.json']
        assert all(entry.file_size <= 128 * 1024**2 for entry in packed.infolist())
        assert sum(entry.file_size for entry in packed.infolist()) <= 256 * 1024**2
        assert packed.testzip() is None
        assert sha(packed.read(prefix + 'sccache.exe')) == manifest['binary_sha256']
        assert packed.read(prefix + 'LICENSE') == (folder / 'LICENSE').read_bytes()
        identity = json.loads(packed.read(prefix + 'source.json'))
        assert all(manifest[key] == value for key, value in identity.items())
    probe = manifest['runtime_probe']
    assert probe['official_long_command_refused_206'] and probe['patched_long_command_passed']
    assert probe['consumer_normal_priority_and_original_values']
    assert probe['warm']['cache_hits'] > probe['cold']['cache_hits']
    assert probe['changed_environment']['cache_misses'] > probe['warm']['cache_misses']
    assert probe['changed_feature']['cache_misses'] > probe['changed_environment']['cache_misses']
    with tarfile.open(folder / 'original-source.tar.gz', 'r:gz') as original:
        entry = original.getmember('sccache-' + config['source_commit'] + '/LICENSE')
        assert entry.isfile() and original.extractfile(entry).read() == (folder / 'LICENSE').read_bytes()
    return manifest


def github(endpoint):
    return json.loads(subprocess.check_output(['gh', 'api', endpoint], cwd=ROOT))


def accepted_checkout():
    current = github(f'repos/{REPOSITORY}/git/ref/heads/main')['object']['sha']
    git('fetch', '--quiet', '--no-write-fetch-head', 'origin', current)
    git('merge-base', '--is-ancestor', 'HEAD', current)
    return current


def verify_run(args):
    assert args.run_id.isdigit() and int(args.run_id) > 0
    accepted_checkout()
    run = github(f'repos/{REPOSITORY}/actions/runs/{args.run_id}')
    assert run['status'] == 'completed' and run['conclusion'] == 'success'
    assert run['path'] == '.github/workflows/compiler-cache-tool.yml'
    assert run['event'] in {'pull_request', 'workflow_dispatch'}
    assert run['head_repository']['full_name'] == REPOSITORY
    git('fetch', '--quiet', '--no-write-fetch-head', 'origin', run['head_sha'])
    git('merge-base', '--is-ancestor', run['head_sha'], 'HEAD')
    jobs = github(f'repos/{REPOSITORY}/actions/runs/{args.run_id}/attempts/{run["run_attempt"]}/jobs?per_page=100')
    assert len(jobs['jobs']) == jobs['total_count'] < 100
    original = [job for job in jobs['jobs'] if job['name'] == 'Qualify Windows compiler tool']
    assert len(original) == 1 and original[0]['conclusion'] == 'success'
    artifacts = github(f'repos/{REPOSITORY}/actions/runs/{args.run_id}/artifacts?per_page=100')
    assert len(artifacts['artifacts']) == artifacts['total_count'] < 100
    selected = [row for row in artifacts['artifacts'] if row['name'] == 'compiler-cache-tool-windows-x64']
    assert len(selected) == 1 and not selected[0]['expired']
    args.output.mkdir(parents=True, exist_ok=False)
    subprocess.run(['gh', 'run', 'download', args.run_id, '--repo', REPOSITORY,
                    '--name', 'compiler-cache-tool-windows-x64', '--dir', str(args.output)], cwd=ROOT, check=True)
    manifest = verify_payload(args.output)
    assert manifest['producer_head'] == run['head_sha']
    proof = {'run': int(args.run_id), 'attempt': run['run_attempt'], 'url': run['html_url'],
             'source': run['head_sha'], 'artifact': selected[0]['id'], 'original_job': original[0]['id'],
             'accepted_ancestor': True, 'exact_recipe_inputs': manifest['source_inputs'],
             'manifest_sha256': sha((args.output / 'manifest.json').read_bytes()),
             'scope': 'Exact qualified tool bytes; no product profile-equivalence or new source acceptance.'}
    write_json(args.output / 'verified-run.json', proof)


def publish(args):
    assert os.environ.get('GITHUB_EVENT_NAME') == 'workflow_dispatch' and os.environ.get('GITHUB_REF') == 'refs/heads/main'
    accepted_checkout()
    manifest = verify_payload(args.output, extras={'verified-run.json'})
    proof = json.loads((args.output / 'verified-run.json').read_bytes())
    assert proof['source'] == manifest['producer_head'] and proof['manifest_sha256'] == sha((args.output / 'manifest.json').read_bytes())
    run = github(f'repos/{REPOSITORY}/actions/runs/{proof["run"]}')
    assert run['head_sha'] == proof['source'] and run['status'] == 'completed' and run['conclusion'] == 'success'
    git('fetch', '--quiet', '--no-write-fetch-head', 'origin', proof['source'])
    git('merge-base', '--is-ancestor', proof['source'], 'HEAD')
    existing = subprocess.run(['gh', 'release', 'view', manifest['release_tag'], '--repo', REPOSITORY], capture_output=True)
    assert existing.returncode != 0, 'Tool release exists; do not replace its assets or tag'
    assert b'HTTP 404' in existing.stderr or existing.stderr.strip().lower() == b'release not found', 'Original release lookup refused for another reason'
    target = git('rev-parse', 'HEAD').decode().strip()
    created = subprocess.run(['gh', 'api', '--method', 'POST', f'repos/{REPOSITORY}/git/refs', '--input', '-'],
                             input=json.dumps({'ref': 'refs/tags/' + manifest['release_tag'], 'sha': target}).encode(),
                             cwd=ROOT, capture_output=True)
    (args.output / 'tag-create.stdout').write_bytes(created.stdout)
    (args.output / 'tag-create.stderr').write_bytes(created.stderr)
    assert created.returncode == 0, 'Tag creation refused; no existing tag is replaced'
    assert json.loads(created.stdout)['object']['sha'] == target
    notes = args.output / 'release-notes.txt'
    notes.write_bytes((f"Pinned sccache0.19.0 with the upstream Windows response-file fix.\n\n"
                       f"Source: mozilla/sccache {manifest['source_commit']}; backport of {manifest['upstream_fix_commit']}.\n"
                       f"Qualified producer: {manifest['producer_head']}, {proof['url']}.\n"
                       "The original formatter, Clippy and upstream tests, fresh library/binary build and real long-command/cache/consumer controls passed.\n"
                       "This is a compiler-tool artifact, not a SudoCode release or proof of population latency savings.\n").encode())
    assets = [str(path) for path in args.output.iterdir() if path.name not in {notes.name, 'tag-create.stdout', 'tag-create.stderr'}]
    subprocess.run(['gh', 'release', 'create', manifest['release_tag'], *assets, '--repo', REPOSITORY,
                    '--verify-tag', '--latest=false', '--title', 'sccache0.19.0 Windows response-file fix',
                    '--notes-file', str(notes)], cwd=ROOT, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest='mode', required=True)
    for name in ['prepare', 'qualify', 'verify-run', 'publish']:
        child = modes.add_parser(name)
        child.add_argument('--output', type=Path, required=True)
        if name == 'qualify':
            child.add_argument('--target-dir', type=Path, required=True)
            child.add_argument('--baseline-tool', type=Path, required=True)
        if name == 'verify-run':
            child.add_argument('--run-id', required=True)
    args = parser.parse_args()
    if args.mode in {'prepare', 'qualify'}:
        assert not any(key.upper() in {'GH_TOKEN', 'GITHUB_TOKEN'} for key in os.environ), 'Candidate tool code must not receive API credentials'
    if args.mode == 'prepare':
        prepare(args.output.resolve())
    elif args.mode == 'qualify':
        qualify(args)
    elif args.mode == 'verify-run':
        verify_run(args)
    else:
        publish(args)


if __name__ == '__main__':
    main()
