#!/usr/bin/env python3
"""从构建产物的 --help 生成参数参考；不执行业务命令。"""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess


TOOLS = {
    "ltfsctl": "集群客户端：库、池、磁带、任务与文件操作",
    "ltfsd": "集群服务进程",
    "tape-fuse": "FUSE 文件系统客户端",
    "tape-library": "独占设备的单机逻辑库工具",
    "tape-rs": "直接 SCSI/LTFS 设备工具",
}


def children(help_text):
    commands = []
    in_commands = False
    for line in help_text.splitlines():
        if line == "Commands:":
            in_commands = True
        elif in_commands and line and not line.startswith(" "):
            in_commands = False
        elif in_commands:
            match = re.match(r"^  ([a-z][a-z0-9-]*)(?:\s|$)", line)
            if match and match[1] != "help":
                commands.append(match[1])
    return commands


def collect(binary, command=()):
    # 每次调用都以 --help 结束，clap 在分派业务/打开设备前退出。
    result = subprocess.run(
        [str(binary), *command, "--help"],
        capture_output=True,
        text=True,
        check=True,
        timeout=20,
        env={**os.environ, "NO_COLOR": "1", "COLUMNS": "100"},
    )
    yield command, "\n".join(line.rstrip() for line in result.stdout.splitlines()).rstrip()
    for child in children(result.stdout):
        yield from collect(binary, (*command, child))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "docs/reference",
    )
    args = parser.parse_args()
    # 全部采集成功后才修改文档，避免缺动态库时留下不完整参考。
    documents = {}
    for name, description in TOOLS.items():
        binary = (args.bin_dir / name).resolve(strict=True)
        sections = list(collect(binary))
        digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        lines = [
            f"# {name} 完整参数参考",
            "",
            description + "。用法说明见[命令行参考手册](../cli-reference.md)。",
            "",
            "本页由 `tools/generate-cli-reference.py` 从实际构建产物递归采集 `--help` 生成，"
            "不表示所有服务模式均支持所有命令，也不表示列出的操作均已完成硬件验收。"
            "可选命令随构建特性变化，下面的帮助为本次构建内容。",
            "",
            f"构建产物 SHA256：`{digest}`。共 {len(sections)} 个帮助入口。",
            "",
            "## 目录",
            "",
        ]
        for command, _ in sections:
            title = " ".join((name, *command))
            anchor = "-".join((name, *command))
            lines.append(f"- [{title}](#{anchor})")
        for command, help_text in sections:
            title = " ".join((name, *command))
            lines.extend(["", f"## {title}", "", "```text", help_text, "```"])
        documents[name] = "\n".join(lines) + "\n"
    args.output_dir.mkdir(parents=True, exist_ok=True)
    for name, content in documents.items():
        (args.output_dir / f"{name}.md").write_text(content, encoding="utf-8")
        print(f"{name}: {len(content.splitlines())} 行")


if __name__ == "__main__":
    main()
