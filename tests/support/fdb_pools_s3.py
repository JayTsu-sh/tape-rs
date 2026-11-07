"""Read-only boto3 acceptance for pools created through the native FDB API."""
import datetime
import json
import sys

import boto3
from botocore import UNSIGNED
from botocore.config import Config

endpoint, expected_json = sys.argv[1:]
expected = json.loads(expected_json)
client = boto3.client(
    "s3", endpoint_url=endpoint, region_name="us-east-1",
    config=Config(signature_version=UNSIGNED, s3={"addressing_style": "path"},
                  proxies={}, connect_timeout=3, read_timeout=15,
                  retries={"max_attempts": 0}),
)
rows = client.list_buckets()["Buckets"]
assert len(rows) == len(expected), rows
for row in rows:
    created = row["CreationDate"]
    assert isinstance(created, datetime.datetime), row
    actual = created.astimezone(datetime.timezone.utc).isoformat(timespec="milliseconds")
    assert actual.replace("+00:00", "Z") == expected[row["Name"]], row
    client.head_bucket(Bucket=row["Name"])
    listed = client.list_objects_v2(Bucket=row["Name"], MaxKeys=1000)
    assert listed["KeyCount"] == 0 and not listed.get("Contents"), listed
print(json.dumps({"client": "boto3", "version": boto3.__version__, "created_dates": len(rows), "passed": True}))
