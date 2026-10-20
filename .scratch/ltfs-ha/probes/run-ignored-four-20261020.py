#!/usr/bin/env python3
"""Run four explicitly selected ignored tests on a physical Linux lab host.

Network effects stay in anonymous network namespaces. No SCSI access is needed;
the caller also runs this script in a DevicePolicy=closed systemd cgroup.
"""
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent
UNIT = ROOT / 'unit-tests'
LARGE = ROOT / 'large-index-tests'
ADDRESS = 'daemon::vip::tests::kernel_address_lifecycle'
PROCESS = 'daemon::vip::tests::kernel_process_expiry'
EXPORT = 'ltfs::incremental::tests::export_xml_for_schema_validation'
MILLION = 'stream_parser_materializes_one_million_files'


def output(args):
    return subprocess.check_output(args, text=True)


def host_snapshot():
    return {
        'hostname': output(['hostname']).strip(),
        'kernel': output(['uname', '-r']).strip(),
        'glibc': output(['getconf', 'GNU_LIBC_VERSION']).strip(),
        'netns_inode': os.stat('/proc/self/ns/net').st_ino,
        'pid1_netns_inode': os.stat('/proc/1/ns/net').st_ino,
        'addresses': json.loads(output(['ip', '-j', 'address', 'show'])),
        'named_netns': sorted(output(['ip', 'netns', 'list']).splitlines()),
    }


def addresses(snapshot):
    return sorted((link['ifname'], a['family'], a['local'], a['prefixlen'],
                   a.get('label', ''))
                  for link in snapshot['addresses'] for a in link['addr_info'])


def run(name, args, timeout=180, env=None):
    started = time.monotonic()
    log = ROOT / (name + '.log')
    with log.open('w') as stream:
        process = subprocess.Popen(args, stdout=stream, stderr=subprocess.STDOUT,
                                   env=env, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            code = 124
    result = {'name': name, 'command': args, 'exit_code': code,
              'seconds': round(time.monotonic() - started, 6), 'log': log.name}
    print(json.dumps(result), flush=True)
    return result


def namespace_test(name):
    here = os.stat('/proc/self/ns/net').st_ino
    host = os.stat('/proc/1/ns/net').st_ino
    assert here != host, 'Refusing host network namespace'
    for args in [
        ['ip', 'link', 'set', 'lo', 'up'],
        ['ip', 'link', 'add', 'viptest0', 'type', 'veth', 'peer', 'name', 'vippeer0'],
        ['ip', 'address', 'add', '192.0.2.1/24', 'dev', 'viptest0'],
        ['ip', 'address', 'add', '192.0.2.2/24', 'dev', 'vippeer0'],
        ['ip', 'link', 'set', 'viptest0', 'up'],
        ['ip', 'link', 'set', 'vippeer0', 'up'],
    ]:
        subprocess.run(args, check=True)
    print(json.dumps({'test': name, 'netns_inode': here, 'host_netns_inode': host,
                      'addresses_before': json.loads(output(['ip', '-j', 'address']))}), flush=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith('TAPE_RS_')}
    env['TAPE_RS_VIP_NETNS_TEST'] = '1'
    completed = subprocess.run([str(UNIT), '--ignored', '--exact', name,
                                '--nocapture', '--test-threads=1'], env=env)
    remaining = json.loads(output(['ip', '-j', 'address']))
    print(json.dumps({'addresses_after': remaining}), flush=True)
    assert not any(a['local'] == '192.0.2.100'
                   for link in remaining for a in link['addr_info']), 'VIP leaked'
    raise SystemExit(completed.returncode)


def main():
    marker = ROOT / 'requested'
    marker.open('x').write(datetime.datetime.now(datetime.timezone.utc).isoformat() + '\n')
    before = host_snapshot()
    (ROOT / 'host-before.json').write_text(json.dumps(before, indent=2) + '\n')
    results = []
    summary = {'source_commit': (ROOT / 'source-commit').read_text().strip(),
               'host': before['hostname'], 'kernel': before['kernel'],
               'runtime': 'statically linked x86_64-unknown-linux-musl test harness',
               'device_access': 'systemd DevicePolicy=closed; no tape commands',
               'binaries_sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                   for p in [UNIT, LARGE]}, 'results': results}
    try:
        for short, name in [('kernel-address', ADDRESS), ('kernel-process', PROCESS)]:
            results.append(run(short, ['unshare', '--net', '--', sys.executable,
                                       str(Path(__file__).resolve()), '--netns', name]))
            if results[-1]['exit_code'] == 0:
                assert 'test result: ok. 1 passed; 0 failed; 0 ignored;' in (ROOT / (short + '.log')).read_text()
        export_dir = ROOT / 'xml-export'
        export_dir.mkdir()
        env = dict(os.environ, TMPDIR=str(export_dir))
        results.append(run('xml-export', [str(UNIT), '--ignored', '--exact', EXPORT,
                                          '--nocapture', '--test-threads=1'], env=env))
        if results[-1]['exit_code'] == 0:
            assert 'test result: ok. 1 passed; 0 failed; 0 ignored;' in (ROOT / 'xml-export.log').read_text()
            files = list(export_dir.glob('tape-rs-incremental-*.xml'))
            assert len(files) == 1
            schema = ROOT / 'ltfs-incremental-2.5.1.xsd'
            results.append(run('xml-xsd-validation', ['xmllint', '--nonet', '--noout',
                                                       '--schema', str(schema), str(files[0])]))
            invalid = export_dir / 'invalid-generation.xml'
            text, changes = re.subn(r'<generationnumber>[^<]+</generationnumber>',
                                   '<generationnumber>invalid</generationnumber>',
                                   files[0].read_text(), count=1)
            assert changes == 1
            invalid.write_text(text)
            negative = run('xml-xsd-negative-control', ['xmllint', '--nonet', '--noout',
                                                       '--schema', str(schema), str(invalid)])
            negative['expected_exit_code'] = 3
            results.append(negative)
        results.append(run('million-files', ['/usr/bin/time', '-v', '-o',
                                             str(ROOT / 'million-files-resource.txt'),
                                             str(LARGE), '--ignored', '--exact', MILLION,
                                             '--nocapture', '--test-threads=1'], timeout=300))
        if results[-1]['exit_code'] == 0:
            assert 'test result: ok. 1 passed; 0 failed; 0 ignored;' in (ROOT / 'million-files.log').read_text()
    finally:
        after = host_snapshot()
        (ROOT / 'host-after.json').write_text(json.dumps(after, indent=2) + '\n')
        summary['host_addresses_unchanged'] = addresses(before) == addresses(after)
        summary['named_netns_unchanged'] = before['named_netns'] == after['named_netns']
        summary['completed_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        summary['success'] = (len(results) == 6 and all(
            r['exit_code'] == r.get('expected_exit_code', 0) for r in results)
            and summary['host_addresses_unchanged'] and summary['named_netns_unchanged'])
        (ROOT / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    assert summary['success'], 'Tests incomplete or failed; see summary.json'
    (ROOT / 'complete').write_text(summary['completed_utc'] + '\n')


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--netns':
        assert sys.argv[2] in [ADDRESS, PROCESS]
        namespace_test(sys.argv[2])
    else:
        assert len(sys.argv) == 1
        main()
