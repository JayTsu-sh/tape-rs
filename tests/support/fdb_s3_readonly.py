"""Explicit isolated FDB + simulated media S3 interoperability, no hardware."""
import sys
import json
import hashlib
import base64
from datetime import datetime, timezone
import boto3
from botocore import UNSIGNED
from botocore.config import Config
from botocore.exceptions import ClientError

endpoint, bucket, other, pool, count = sys.argv[1:]
s3 = boto3.client('s3', endpoint_url='http://' + endpoint, region_name='us-east-1',
                  config=Config(signature_version=UNSIGNED, s3={'addressing_style': 'path'},
                                proxies={},
                                connect_timeout=3, read_timeout=15, retries={'max_attempts': 0}))
assert {v['Name'] for v in s3.list_buckets()['Buckets']} == {bucket, other}
assert s3.head_bucket(Bucket=bucket)['ResponseMetadata']['HTTPStatusCode'] == 200
assert s3.get_bucket_location(Bucket=bucket)['LocationConstraint'] in (None, '')
pages = list(s3.get_paginator('list_objects_v2').paginate(Bucket=bucket))
keys = [v['Key'] for page in pages for v in page.get('Contents', [])]
assert len(keys) == int(count) and len(set(keys)) == len(keys)
assert keys == sorted(keys, key=lambda k: k.encode('utf-8'))
assert pages[0]['KeyCount'] == 1000 and len(pages) == 2
assert all(isinstance(v['LastModified'], datetime) for page in pages for v in page.get('Contents', []))
h = s3.head_object(Bucket=bucket, Key='file-0', ChecksumMode='ENABLED')
assert h['ContentLength'] == 9
assert h['ChecksumSHA256'] == base64.b64encode(hashlib.sha256(b'content-0').digest()).decode()
r = s3.get_object(Bucket=bucket, Key='file-0', IfMatch=h['ETag'], ChecksumMode='ENABLED')
assert r['Body'].read() == b'content-0' and r['ETag'] == h['ETag']
r = s3.get_object(Bucket=bucket, Key='file-0', Range='bytes=2-5')
assert r['Body'].read() == b'nten' and r['ContentRange'] == 'bytes 2-5/9'
assert s3.get_object(Bucket=bucket, Key='k0000')['Body'].read() == b'object'
imported = s3.head_object(Bucket=bucket, Key='k0000')
assert imported['ContentType'] == 'text/plain' and imported['CacheControl'] == 'max-age=60'
assert imported['Metadata'] == {'origin':'fdb-test'}
assert isinstance(imported['LastModified'], datetime)
assert s3.head_object(Bucket=bucket, Key='file-0', IfMatch=h['ETag'], IfUnmodifiedSince=datetime(1970,1,1,tzinfo=timezone.utc))['ContentLength'] == 9
assert all(not key.startswith(pool + '/') for key in keys)
delimited = s3.list_objects_v2(Bucket=bucket, Delimiter='/', MaxKeys=2)
assert delimited['KeyCount'] == 2 and delimited['CommonPrefixes'][0]['Prefix'] == 'a-/'
assert s3.list_objects(Bucket=bucket, MaxKeys=1)['IsTruncated']
for method, args, status in [
    ('get_object', {'Bucket':bucket,'Key':'file-0','IfNoneMatch':h['ETag']},304),
    ('get_object', {'Bucket':bucket,'Key':'file-0','IfMatch':'"wrong"'},412),
    ('get_object', {'Bucket':bucket,'Key':'file-0','IfModifiedSince':h['LastModified']},304),
    ('get_object', {'Bucket':bucket,'Key':'file-0','IfUnmodifiedSince':datetime(1970,1,1,tzinfo=timezone.utc)},412),
    ('get_object', {'Bucket':bucket,'Key':'file-0','Range':'bytes=999-'},416),
    ('get_object', {'Bucket':other,'Key':'file-0'},404),
    ('put_object', {'Bucket':bucket,'Key':'never-created','Body':b'abc'},405),
    ('delete_object', {'Bucket':bucket,'Key':'file-0'},405),
]:
    try:
        getattr(s3, method)(**args)
    except ClientError as error:
        assert error.response['ResponseMetadata']['HTTPStatusCode'] == status, (method,error.response)
    else:
        raise AssertionError((method, 'unexpected success'))
print(json.dumps({'client':'boto3','version':boto3.__version__,'objects':len(keys),'pages':len(pages),'passed':True}))
