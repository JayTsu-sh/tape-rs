import base64
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("s3_sequential_io", Path(__file__).with_name("s3-sequential-io.py"))
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


class S3:
    def __init__(self):
        self.objects = {}
        self.puts = []
        self.gets = []
        self.lose_response = None
        self.corrupt_read = False

    def put_object(self, **request):
        key = request["Key"]
        self.puts.append(key)
        assert request["IfNoneMatch"] == "*"
        assert key not in self.objects
        body = request["Body"].read()
        assert request["ContentLength"] == len(body)
        assert request["ChecksumSHA256"] == base64.b64encode(hashlib.sha256(body).digest()).decode()
        etag = f'"committed-{len(self.puts)}"'
        self.objects[key] = body, etag
        if key == self.lose_response:
            raise TimeoutError("committed but response lost")
        return {"ResponseMetadata": {"HTTPStatusCode": 200}, "ETag": etag}

    def get_object(self, **request):
        self.gets.append(request["Key"])
        body, etag = self.objects[request["Key"]]
        if request["IfMatch"] != etag:
            raise RuntimeError("version condition rejected")
        if self.corrupt_read:
            body = bytes(x ^ 1 for x in body)
        return {"Body": io.BytesIO(body), "ContentLength": len(body), "ETag": etag}


class SequentialIO(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.keys = ["z-last-name", "a-first-name", "m-middle"]
        rows = []
        for i, key in enumerate(self.keys):
            path = self.root / f"input-{i}"
            path.write_bytes(bytes([i + 31]) * (4096 + i))
            rows.append({"key": key, "file": path.name})
        self.input = self.root / "input.jsonl"
        self.input.write_text("".join(json.dumps(row) + "\n" for row in rows))
        self.manifest = self.root / "order.jsonl"
        self.client = S3()
        self.patcher = patch.object(tool, "build_client", return_value=self.client)
        self.patcher.start()
        self.addCleanup(self.patcher.stop)
        self.addCleanup(self.tmp.cleanup)

    def write(self):
        return tool.write_ordered("http://127.0.0.1:7402", "bucket", self.input, self.manifest)

    def test_read_matches_actual_committed_order_not_key_sort(self):
        count, size = self.write()
        self.assertEqual(count, 3)
        self.assertNotEqual(self.keys, sorted(self.keys))
        self.assertEqual(tool.read_ordered(self.manifest, self.root / "output"), (count, size))
        self.assertEqual(self.client.puts, self.keys)
        self.assertEqual(self.client.gets, self.keys)
        for key in self.keys:
            self.assertEqual((self.root / "output" / key).read_bytes(), self.client.objects[key][0])

    def test_unknown_put_stops_without_retry_or_completed_manifest(self):
        self.client.lose_response = self.keys[1]
        with self.assertRaisesRegex(RuntimeError, "未结清"):
            self.write()
        self.assertEqual(self.client.puts, self.keys[:2])
        self.assertEqual(len(self.client.objects), 2)
        unresolved = json.loads(self.manifest.read_text().splitlines()[-1])
        self.assertEqual(unresolved["outcome"], "unknown")
        with self.assertRaises(ValueError):
            tool.read_ordered(self.manifest, self.root / "output")
        self.assertEqual(self.client.gets, [])
        self.assertFalse((self.root / "output").exists())

    def test_later_corrupt_manifest_rejected_before_any_get(self):
        self.write()
        raw = self.manifest.read_text().splitlines()
        value = json.loads(raw[-2])
        value["sequence"] = 77
        raw[-2] = json.dumps(value)
        self.manifest.write_text("\n".join(raw) + "\n")
        with self.assertRaises(ValueError):
            tool.read_ordered(self.manifest, self.root / "output")
        self.assertEqual(self.client.gets, [])
        self.assertFalse((self.root / "output").exists())

    def test_bad_body_never_published_and_later_gets_stop(self):
        self.write()
        self.client.corrupt_read = True
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            tool.read_ordered(self.manifest, self.root / "output")
        self.assertEqual(self.client.gets, self.keys[:1])
        self.assertFalse((self.root / "output" / self.keys[0]).exists())
        self.assertTrue(list((self.root / "output").glob(".s3-sequential-*")))

    def test_etag_change_cannot_silently_read_replacement(self):
        self.write()
        body, _ = self.client.objects[self.keys[0]]
        self.client.objects[self.keys[0]] = body, '"replacement"'
        with self.assertRaisesRegex(RuntimeError, "version"):
            tool.read_ordered(self.manifest, self.root / "output")
        self.assertEqual(self.client.gets, self.keys[:1])
        self.assertFalse((self.root / "output" / self.keys[0]).exists())

    def test_existing_manifest_is_preserved_without_put(self):
        self.manifest.write_bytes(b"original incomplete intent\n")
        with self.assertRaises(FileExistsError):
            self.write()
        self.assertEqual(self.manifest.read_bytes(), b"original incomplete intent\n")
        self.assertEqual(self.client.puts, [])

    def test_manifest_requires_existing_parent_before_any_put(self):
        self.manifest = self.root / "missing" / "nested" / "order.jsonl"
        with self.assertRaisesRegex(ValueError, "父目录必须已存在"):
            self.write()
        self.assertFalse((self.root / "missing").exists())
        self.assertEqual(self.client.puts, [])

    def test_unsafe_object_key_is_rejected_before_put(self):
        self.input.write_text(json.dumps({"key": "../escape", "file": "input-0"}) + "\n")
        with self.assertRaises(ValueError):
            self.write()
        self.assertEqual(self.client.puts, [])


if __name__ == "__main__":
    unittest.main()
