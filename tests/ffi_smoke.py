# -*- coding: utf-8 -*-
"""fts.dll C ABI 冒烟测试(P3 验收:Python ctypes 调用 → 传输 → 校验退出码)。

前置:接收端在 127.0.0.1:4433 监听(如 `fts daemon --listen 0.0.0.0:4433`),
     证书指纹经 FTS_FP 环境变量传入或用第一行参数。

运行:python tests\\ffi_smoke.py <fingerprint_hex> [src_file]
"""
import ctypes
import os
import sys

DLL = os.path.join(os.path.dirname(__file__), "..", "target", "release", "fts.dll")
lib = ctypes.CDLL(os.path.abspath(DLL))

lib.fts_version.restype = ctypes.c_char_p
lib.fts_abi_version.restype = ctypes.c_uint32
lib.fts_last_error.restype = ctypes.c_char_p
lib.fts_send_file.restype = ctypes.c_int32
lib.fts_send_file.argtypes = [
    ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p,
    ctypes.c_int32, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_char_p,
]

print("fts_version    =", lib.fts_version().decode())
print("fts_abi_version=", lib.fts_abi_version())

fp = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("FTS_FP")
src = sys.argv[2] if len(sys.argv) > 2 else "testsrc.bin"
dst = "127.0.0.1:4433"

rc = lib.fts_send_file(
    src.encode("utf-8"),
    dst.encode("utf-8"),
    fp.encode("utf-8") if fp else None,
    0 if fp else 1,
    4,          # streams
    0,          # chunk_size = 默认
    b"auto",    # transport
)
err = lib.fts_last_error().decode("utf-8", "replace")
print("rc =", rc, "err =", repr(err))

# 参数错误路径:指纹与 insecure 互斥 → rc=2
rc2 = lib.fts_send_file(
    src.encode("utf-8"), dst.encode("utf-8"),
    b"00" * 32, 1, 0, 0, None,
)
print("mutual-exclusive rc =", rc2, "(expect 2)")

sys.exit(0 if rc == 0 else 1)
