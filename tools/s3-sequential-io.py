#!/usr/bin/env python3
"""按实际提交顺序写入 S3，并用持久清单按同一顺序校验读取。

可选客户端工具：需要支持条件 PUT 的 boto3；不增加服务端依赖。
每次 PUT 等待磁带提交后才发送下一次，未确认写入不自动重试。
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from urllib.parse import urlsplit

CHUNK = 1 << 20


def endpoint_url(value):
    url = urlsplit(value)
    if url.scheme not in ("http", "https") or not url.hostname or url.username or url.password:
        raise ValueError("endpoint 必须是无嵌入认证信息的 HTTP/HTTPS 地址")
    if url.query or url.fragment or url.path not in ("", "/"):
        raise ValueError("endpoint 不接受路径、查询参数或片段")
    return value.rstrip("/")


def build_client(endpoint):
    try:
        import boto3
        from botocore import UNSIGNED
        from botocore.config import Config
    except ImportError as error:
        raise RuntimeError("本工具需要单独安装 boto3；不属于服务端运行依赖") from error
    return boto3.client(
        "s3", endpoint_url=endpoint_url(endpoint), region_name="us-east-1",
        config=Config(signature_version=UNSIGNED, connect_timeout=10, read_timeout=900,
                      retries={"max_attempts": 0}, s3={"addressing_style": "path"}, proxies={}),
    )


def key_path(key):
    if not isinstance(key, str) or not key or any(ord(c) < 32 for c in key):
        raise ValueError("对象 key 无效")
    if any(part in ("", ".", "..") for part in key.split("/")) or "\x7f" in key:
        raise ValueError("对象 key 含有不安全路径分量")
    return Path(*key.split("/"))


def line_bytes(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode()


def durable_line(stream, value, digest=None):
    raw = line_bytes(value)
    stream.write(raw)
    stream.flush()
    os.fsync(stream.fileno())
    if digest is not None:
        digest.update(raw)


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def file_hash(stream):
    digest = hashlib.sha256()
    size = 0
    while chunk := stream.read(CHUNK):
        size += len(chunk)
        digest.update(chunk)
    return size, digest.digest()


def write_ordered(endpoint, bucket, input_path, manifest_path):
    endpoint = endpoint_url(endpoint)
    client = build_client(endpoint)
    input_path, manifest_path = Path(input_path), Path(manifest_path)
    if not manifest_path.parent.is_dir():
        raise ValueError("清单父目录必须已存在；请先创建并持久保存目录")
    fd = os.open(manifest_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    count, total = 0, 0
    digest = hashlib.sha256()
    with os.fdopen(fd, "wb") as manifest, input_path.open(encoding="utf-8") as inputs:
        durable_line(manifest, {"kind": "header", "version": 1, "endpoint": endpoint, "bucket": bucket}, digest)
        sync_dir(manifest_path.parent)
        for raw in inputs:
            item = json.loads(raw)
            if set(item) != {"key", "file"}:
                raise ValueError("输入每行必须只有 key 与 file")
            key_path(item["key"])
            source = input_path.parent / item["file"]
            if not source.is_file():
                raise ValueError("上传源必须是普通文件")
            with source.open("rb") as body:
                size, checksum = file_hash(body)
                body.seek(0)
                intent = {"kind": "intent", "sequence": count, "key": item["key"],
                          "bytes": size, "sha256": checksum.hex()}
                durable_line(manifest, intent, digest)
                try:
                    result = client.put_object(
                        Bucket=bucket, Key=item["key"], Body=body, ContentLength=size,
                        IfNoneMatch="*", ChecksumAlgorithm="SHA256",
                        ChecksumSHA256=base64.b64encode(checksum).decode(),
                    )
                    if result["ResponseMetadata"]["HTTPStatusCode"] != 200 or not result.get("ETag"):
                        raise RuntimeError("PUT 未返回完整提交确认")
                except Exception as error:
                    response = getattr(error, "response", {})
                    status = response.get("ResponseMetadata", {}).get("HTTPStatusCode")
                    outcome = "rejected" if status is not None and 400 <= status < 500 else "unknown"
                    if type(error).__name__ == "ParamValidationError":
                        outcome = "not_sent"
                    durable_line(manifest, {"kind": "unresolved", "sequence": count,
                                           "outcome": outcome, "exception": type(error).__name__})
                    raise RuntimeError("写入未结清；保留原清单并按原 key 核对，不自动重传") from error
                durable_line(manifest, {"kind": "committed", "sequence": count, "etag": result["ETag"]}, digest)
                count += 1
                total += size
        durable_line(manifest, {"kind": "complete", "objects": count, "sha256": digest.hexdigest()})
    return count, total


def manifest_records(stream):
    digest = hashlib.sha256()
    header, pending, sequence, complete = None, None, 0, False
    for raw in stream:
        if len(raw) > 16384 or complete:
            raise ValueError("清单行过大或结束标记之后仍有记录")
        value = json.loads(raw)
        kind = value.get("kind")
        if header is None:
            if set(value) != {"kind", "version", "endpoint", "bucket"} or kind != "header" or value["version"] != 1:
                raise ValueError("清单头部或版本无效")
            endpoint_url(value["endpoint"])
            if not isinstance(value["bucket"], str) or not value["bucket"]:
                raise ValueError("清单 bucket 无效")
            header = value
            yield header
        elif kind == "intent" and pending is None:
            if set(value) != {"kind", "sequence", "key", "bytes", "sha256"} or type(value["sequence"]) is not int or value["sequence"] != sequence:
                raise ValueError("清单写入顺序不连续")
            key_path(value["key"])
            if type(value["bytes"]) is not int or value["bytes"] < 0 or not re.fullmatch(r"[0-9a-f]{64}", value["sha256"]):
                raise ValueError("清单对象长度或摘要无效")
            pending = value
        elif kind == "committed" and pending is not None:
            if set(value) != {"kind", "sequence", "etag"} or type(value["sequence"]) is not int or value["sequence"] != sequence:
                raise ValueError("提交确认与原写入身份不符")
            etag = value["etag"]
            if not isinstance(etag, str) or not etag or len(etag) > 256 or any(ord(c) < 32 for c in etag):
                raise ValueError("清单 ETag 无效")
            yield {**pending, "etag": etag}
            pending = None
            sequence += 1
        elif kind == "complete" and pending is None:
            if set(value) != {"kind", "objects", "sha256"} or type(value["objects"]) is not int or value["objects"] != sequence or value["sha256"] != digest.hexdigest():
                raise ValueError("清单结束计数或摘要不符")
            complete = True
        else:
            raise ValueError("清单有未结清、重复或非法记录，须先核对原写入")
        if not complete:
            digest.update(raw)
    if header is None or not complete or pending is not None:
        raise ValueError("清单未完成，禁止将部分写入当成整组成功")


def read_ordered(manifest_path, output_dir, endpoint=None):
    output_dir = Path(output_dir)
    with Path(manifest_path).open("rb") as manifest:
        # Validate the entire durable ledger before any GET or output creation.
        records = manifest_records(manifest)
        header = next(records)
        for _ in records:
            pass
        manifest.seek(0)
        records = manifest_records(manifest)
        if next(records) != header:
            raise ValueError("读取期间清单头部改变")
        client = build_client(endpoint or header["endpoint"])
        output_dir.mkdir(mode=0o700)
        count, total = 0, 0
        for record in records:
            target = output_dir / key_path(record["key"])
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            result = client.get_object(Bucket=header["bucket"], Key=record["key"], IfMatch=record["etag"])
            body = result["Body"]
            fd, partial = tempfile.mkstemp(prefix=".s3-sequential-", dir=target.parent)
            try:
                with os.fdopen(fd, "wb") as output:
                    digest = hashlib.sha256()
                    size = 0
                    while chunk := body.read(CHUNK):
                        size += len(chunk)
                        if size > record["bytes"]:
                            raise ValueError("下载长度超过原提交记录")
                        output.write(chunk)
                        digest.update(chunk)
                    if size != record["bytes"] or digest.hexdigest() != record["sha256"]:
                        raise ValueError("下载长度或 SHA-256 与原提交记录不符")
                    output.flush()
                    os.fsync(output.fileno())
                os.link(partial, target)
                sync_dir(target.parent)
                os.unlink(partial)
            finally:
                body.close()
            count += 1
            total += size
    return count, total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    write = sub.add_parser("write", help="按输入 JSONL 顺序逐个确认落带并保存清单")
    write.add_argument("--endpoint", required=True)
    write.add_argument("--bucket", required=True)
    write.add_argument("--input", required=True, help='JSONL，每行 {"key":"对象键","file":"相对输入清单的本地文件"}')
    write.add_argument("--manifest", required=True, help="新清单路径；已有清单不会覆盖或自动重放")
    read = sub.add_parser("read", help="严格按原写入清单串行下载并校验，不使用 ListObjects 的排序")
    read.add_argument("--manifest", required=True)
    read.add_argument("--output-dir", required=True, help="必须不存在的新目录")
    read.add_argument("--endpoint", help="显式指定同集群的新访问入口；不修改原 bucket/ETag/顺序")
    args = parser.parse_args()
    try:
        if args.command == "write":
            count, size = write_ordered(args.endpoint, args.bucket, args.input, args.manifest)
        else:
            count, size = read_ordered(args.manifest, args.output_dir, args.endpoint)
    except Exception as error:
        parser.exit(1, f"错误: {error}\n")
    print(f"顺序{ '写入' if args.command == 'write' else '读取'}完成：{count} 个对象，{size} 字节")


if __name__ == "__main__":
    main()
