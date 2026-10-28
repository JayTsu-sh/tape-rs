#!/usr/bin/env python3
"""外部 S3 客户端软件验收，由 s3_external_clients 使用 SimLibrary 启动入口。"""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit

import boto3
from botocore import UNSIGNED
from botocore.config import Config
from botocore.exceptions import ClientError

endpoint, bucket = sys.argv[1:]
os.environ.update(AWS_EC2_METADATA_DISABLED="true", AWS_DEFAULT_REGION="us-east-1",
                  AWS_REQUEST_CHECKSUM_CALCULATION="WHEN_REQUIRED",
                  AWS_RESPONSE_CHECKSUM_VALIDATION="WHEN_REQUIRED")
for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
    os.environ.pop(name, None)
os.environ["NO_PROXY"] = "127.0.0.1,localhost"
client = boto3.client("s3", endpoint_url=endpoint, region_name="us-east-1",
                      config=Config(signature_version=UNSIGNED, s3={"addressing_style": "path"},
                                    read_timeout=90, retries={"max_attempts": 1}))
assert bucket in [item["Name"] for item in client.list_buckets()["Buckets"]]
client.head_bucket(Bucket=bucket)
assert client.get_bucket_location(Bucket=bucket)["LocationConstraint"] is None
with tempfile.TemporaryDirectory(prefix="tapers-s3-clients-") as root:
    root = Path(root)
    data = ("中文 S3 test\n".encode() * 10000) + b"tail"
    response = client.put_object(Bucket=bucket, Key="sdk/中文 + %.bin", Body=data,
                                 ContentLength=len(data), ContentType="application/test",
                                 Metadata={"source": "boto3"})
    assert response["ETag"] == '"' + hashlib.md5(data).hexdigest() + '"'
    head = client.head_object(Bucket=bucket, Key="sdk/中文 + %.bin")
    assert head["ContentLength"] == len(data) and head["Metadata"]["source"] == "boto3"
    assert head["ContentType"] == "application/test"
    result = client.get_object(Bucket=bucket, Key="sdk/中文 + %.bin", Range="bytes=1-99")
    assert result["Body"].read() == data[1:100]
    assert result["ResponseMetadata"]["HTTPStatusCode"] == 206
    listed = list(client.get_paginator("list_objects_v2").paginate(Bucket=bucket, PaginationConfig={"PageSize": 1}))
    assert any(item["Key"] == "sdk/中文 + %.bin" for page in listed for item in page.get("Contents", []))
    try:
        client.create_bucket(Bucket=bucket)
        raise AssertionError("CreateBucket must fail")
    except ClientError as error:
        assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 501
    print("boto3 anonymous PUT/HEAD/GET Range/list/CreateBucket refusal passed", flush=True)

    binary_dir = Path(sys.executable).parent
    def command(*args):
        completed = subprocess.run(args, capture_output=True, text=True, timeout=120,
                                   env={**os.environ, "AWS_ACCESS_KEY_ID": "", "AWS_SECRET_ACCESS_KEY": ""})
        if completed.returncode:
            raise AssertionError(f"{args[0]} failed: {completed.stdout}\n{completed.stderr}")
        return completed.stdout
    aws = os.environ.get("TAPE_RS_S3_AWS", str(binary_dir / "aws"))
    common = [aws, "--endpoint-url", endpoint, "--no-sign-request", "--region", "us-east-1", "--cli-read-timeout", "90"]
    source = root / "source.bin"
    source.write_bytes(data)
    command(*common, "s3api", "put-object", "--bucket", bucket, "--key", "aws.bin", "--body", str(source))
    downloaded = root / "aws.bin"
    command(*common, "s3api", "get-object", "--bucket", bucket, "--key", "aws.bin", str(downloaded))
    assert downloaded.read_bytes() == data
    assert bucket in command(*common, "s3", "ls")
    assert "aws.bin" in command(*common, "s3", "ls", f"s3://{bucket}/", "--recursive")
    command(*common, "s3api", "delete-object", "--bucket", bucket, "--key", "aws.bin")
    print("AWS CLI unsigned list/PUT/GET/DELETE passed", flush=True)

    # s3cmd 2.4.0 自身要求凭据配置并签名；占位符只满足客户端，不在服务端配置或校验。
    cfg = root / "s3cmd.cfg"
    host = urlsplit(endpoint).netloc
    cfg.write_text(f"[default]\naccess_key = anonymous\nsecret_key = anonymous\nhost_base = {host}\nhost_bucket = {host}\nbucket_location = us-east-1\nuse_https = False\nsignature_v2 = False\nuse_mime_magic = False\nsocket_timeout = 90\n")
    cfg.chmod(0o600)
    s3cmd = [str(binary_dir / "s3cmd"), "--config=" + str(cfg), "--disable-multipart", "--max-retries=0"]
    assert bucket in command(*s3cmd, "ls")
    command(*s3cmd, "put", str(source), f"s3://{bucket}/s3cmd.bin")
    assert "s3cmd.bin" in command(*s3cmd, "ls", f"s3://{bucket}/")
    target = root / "s3cmd.bin"
    command(*s3cmd, "get", f"s3://{bucket}/s3cmd.bin", str(target))
    assert target.read_bytes() == data
    command(*s3cmd, "del", f"s3://{bucket}/s3cmd.bin")
    print("s3cmd list/PUT/GET/DELETE with client-only placeholders passed", flush=True)

client.delete_object(Bucket=bucket, Key="sdk/中文 + %.bin")
client.delete_object(Bucket=bucket, Key="sdk/中文 + %.bin")
print("all external clients passed", flush=True)
