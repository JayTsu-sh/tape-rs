"""Opt-in RenameObject interoperability against the test's local FDB/Sim server."""
import sys
import uuid
import boto3
from botocore import UNSIGNED
from botocore.config import Config
from botocore.exceptions import ClientError

endpoint, bucket = sys.argv[1:]
client = boto3.client(
    "s3", endpoint_url="http://" + endpoint, region_name="us-east-1",
    config=Config(signature_version=UNSIGNED, s3={"addressing_style": "path"},
                  proxies={}, connect_timeout=3, read_timeout=15,
                  retries={"max_attempts": 0}),
)
before = client.head_object(Bucket=bucket, Key="file-1", ChecksumMode="ENABLED")
request = dict(Bucket=bucket, Key="sdk-中文%literal", RenameSource=f"{bucket}/file-1",
               ClientToken=str(uuid.uuid4()), SourceIfMatch=before["ETag"],
               DestinationIfNoneMatch="*")
assert client.rename_object(**request)["ResponseMetadata"]["HTTPStatusCode"] == 200
after = client.get_object(Bucket=bucket, Key=request["Key"], ChecksumMode="ENABLED")
assert after["Body"].read() == b"content-0"
assert after["LastModified"] == before["LastModified"]
assert after["ChecksumSHA256"] == before["ChecksumSHA256"]
assert client.rename_object(**request)["ResponseMetadata"]["HTTPStatusCode"] == 200
try:
    client.rename_object(**dict(request, Key="other"))
except ClientError as error:
    assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 400
    assert error.response["Error"]["Code"] == "IdempotencyParameterMismatch"
else:
    raise AssertionError("same token accepted changed parameters")
print("boto3 RenameObject verified", boto3.__version__)
