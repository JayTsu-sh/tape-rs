"""Opt-in modern Boto3 default-checksum interoperability; no checksum overrides."""
import base64
import sys
import zlib

import boto3
from botocore import UNSIGNED
from botocore.config import Config
from botocore.exceptions import ClientError

endpoint, bucket = sys.argv[1:]
client = boto3.client(
    "s3", endpoint_url="http://" + endpoint, region_name="us-east-1",
    config=Config(signature_version=UNSIGNED, s3={"addressing_style": "path"},
                  proxies={}, connect_timeout=3, read_timeout=30,
                  retries={"max_attempts": 0}),
)
data = b"123456789"
expected = base64.b64encode(zlib.crc32(data).to_bytes(4, "big")).decode()
result = client.put_object(Bucket=bucket, Key="sdk-crc32", Body=data, IfNoneMatch="*")
assert result["ChecksumCRC32"] == expected, (boto3.__version__, result)
for method in [client.get_object, client.head_object]:
    result = method(Bucket=bucket, Key="sdk-crc32", ChecksumMode="ENABLED")
    assert result["ChecksumCRC32"] == expected
    if "Body" in result:
        assert result["Body"].read() == data  # Also runs botocore's response validator.
for parameters, status, code in [
    (dict(Key="sdk-crc32", Body=b"other", IfNoneMatch="*"), 412, "PreconditionFailed"),
    (dict(Key="sdk-crc32-bad", Body=data, ChecksumCRC32="AAAAAA=="), 400, "BadDigest"),
]:
    try:
        client.put_object(Bucket=bucket, **parameters)
    except ClientError as error:
        assert error.response["ResponseMetadata"]["HTTPStatusCode"] == status
        assert error.response["Error"]["Code"] == code
    else:
        raise AssertionError("Invalid write succeeded")
assert client.get_object(Bucket=bucket, Key="sdk-crc32")["Body"].read() == data
client.delete_object(Bucket=bucket, Key="sdk-crc32")
print("boto3 default CRC32, validated read, condition and BadDigest verified", boto3.__version__)
