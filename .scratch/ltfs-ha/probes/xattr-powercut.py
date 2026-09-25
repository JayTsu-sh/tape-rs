#!/usr/bin/env python3
"""Explicitly gated RT2502L8 HTTP xattr hard-stop probe; never stops a VM itself."""
import base64
import hashlib
import json
import os
from pathlib import Path
import sys
from urllib.parse import urlencode
from urllib.request import Request, urlopen

assert os.environ.get('TAPE_RS_XATTR_POWERCUT_BARCODE') == 'RT2502L8'
base = os.environ['TAPE_RS_XATTR_POWERCUT_ENDPOINT'].rstrip('/')
assert base in ['http://10.131.9.' + n + ':7501' for n in ('71', '72', '74')]
result = Path(__file__).parent / 'results'
path = '/rc/xattr-fault-20260925'
manifest = result / 'xattr-powercut-manifest.json'

def get(route):
    with urlopen(base + route, timeout=180) as r:
        return r.read()

def stat():
    s = json.loads(get('/stat' + path))
    assert s['barcode'] == 'RT2502L8' and s['committed']
    return s

def attrs(s):
    return {x['key']: base64.b64decode(x['value']) if x['base64'] else x['value'].encode()
            for x in s['metadata']['xattrs']}

def change(method, name, data=None, flags=0):
    query = urlencode(dict(path=path, name='user.' + name, flags=flags))
    with urlopen(Request(base + '/xattrs?' + query, data=data, method=method), timeout=180) as r:
        body = r.read().decode()
        assert r.status == 201, (r.status, body)
        print(json.dumps(dict(method=method, name=name, status=r.status, response=body)), flush=True)

mode = sys.argv[1]
if mode == 'prepare':
    assert not manifest.exists()
    before = stat()
    assert before['generation'] == 73
    a = attrs(before)
    assert a['set'] == b'\x00\xffcommitted'
    assert 'powercut.binary' not in a and 'powercut.empty' not in a
    before_data = get('/files' + path)
    assert before_data == b'xattr fault unchanged data'
    baseline = json.loads(get('/list'))
    hashes = {p: hashlib.sha256(get('/files' + p)).hexdigest() for p in baseline['files']}
    manifest.write_text(json.dumps(dict(before=before, baseline=baseline, hashes=hashes), indent=2))
    change('POST', 'powercut.binary', b'\x00\xffpowercut\x00', 1)
    change('POST', 'powercut.empty', b'', 1)
    change('DELETE', 'set')
    after = stat()
    assert after['generation'] == 76
    (result / 'xattr-powercut-ack-stat.json').write_text(json.dumps(after, indent=2))
    print('ACK: three incremental commits, generation 76', flush=True)
elif mode == 'verify':
    m = json.loads(manifest.read_text())
    s = stat()
    expected = json.loads((result / 'xattr-powercut-ack-stat.json').read_text())
    assert s['generation'] == 76 and s['version'] == expected['version'], s
    assert s['metadata'] == expected['metadata']
    a = attrs(s)
    assert a['powercut.binary'] == b'\x00\xffpowercut\x00' and a['powercut.empty'] == b'' and 'set' not in a
    assert get('/files' + path) == b'xattr fault unchanged data'
    assert s['metadata']['modify_time'] == m['before']['metadata']['modify_time']
    assert json.loads(get('/list')) == m['baseline']
    for p, sha in m['hashes'].items():
        assert hashlib.sha256(get('/files' + p)).hexdigest() == sha, p
    (result / 'xattr-powercut-recovered-stat.json').write_text(json.dumps(s, indent=2))
    print('PASS: generation, version, binary/empty/deleted xattrs, mtime, list and all file hashes', flush=True)
elif mode == 'cleanup':
    m = json.loads(manifest.read_text())
    assert attrs(stat())['powercut.binary'] == b'\x00\xffpowercut\x00'
    change('DELETE', 'powercut.binary')
    change('DELETE', 'powercut.empty')
    change('POST', 'set', b'\x00\xffcommitted', 1)
    actual = attrs(stat()); expected = attrs(m['before'])
    actual.pop('tapers.version'); expected.pop('tapers.version')
    assert actual == expected
    print('PASS: original user attributes restored')
else:
    raise SystemExit('prepare|verify|cleanup')
