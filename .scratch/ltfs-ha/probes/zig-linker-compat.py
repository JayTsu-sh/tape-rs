#!/usr/bin/env python3
"""cargo-zigbuild 兼容入口：移除 Zig 已忽略的 GNU ld -O1，不过滤诊断输出。

用 CARGO_ZIGBUILD_ZIG_PATH 指向本文件；真实 zig 仍从 PATH 查找。
仅处理明确的链接器参数，C/C++ 的 -O1/-O2/-O3 保持原样。
"""
import os
import shutil
import sys

zig = shutil.which('zig')
if zig is None:
    sys.exit('PATH 中没有真实 zig')
args = sys.argv[1:]
if args and args[0] in ('cc', 'c++'):
    args = [arg for arg in args if arg != '-Wl,-O1']
os.execv(zig, [zig, *args])
