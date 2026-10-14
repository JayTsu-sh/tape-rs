#!/usr/bin/env python3
"""cargo-zigbuild 兼容入口：移除 Zig 已忽略的 GNU ld -O1，不过滤诊断输出。

用 CARGO_ZIGBUILD_ZIG_PATH 指向本文件；真实 zig 仍从 PATH 查找。
C/C++ 的 -O1/-O2/-O3 保持原样。
Zig 0.16/0.17 不接受 cc-rs 的 GNU generic tuning 组合；将 x86-64 基线
组合显式映射为 Zig 的 -mcpu=baseline，不启用额外 ISA。
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
    if '-march=x86-64' in args and '-mtune=generic' in args:
        args = [arg for arg in args if arg not in ('-march=x86-64', '-mtune=generic')]
        args.append('-mcpu=baseline')
os.execv(zig, [zig, *args])
