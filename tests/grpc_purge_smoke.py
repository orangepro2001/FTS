# -*- coding: utf-8 -*-
"""fts gRPC W6 smoke: PurgeInbox (force guard, dir-mode only) + credential rejection.

Run: python tests\\grpc_purge_smoke.py
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
GEN = os.path.join(HERE, "_gen")
os.makedirs(GEN, exist_ok=True)
subprocess.run([sys.executable, "-m", "grpc_tools.protoc", "-I", os.path.join(ROOT, "proto"),
                "--python_out", GEN, "--grpc_python_out", GEN,
                os.path.join(ROOT, "proto", "fts.proto")], check=True)
sys.path.insert(0, GEN)

import grpc
import fts_pb2, fts_pb2_grpc  # noqa: E402

ch = grpc.insecure_channel("127.0.0.1:50051")
stub = fts_pb2_grpc.FtsStub(ch)
fails = []

# 1. credential on local path -> INVALID_ARGUMENT(且错误不含密码)
try:
    stub.ReserveInbox(fts_pb2.ReserveInboxRequest(
        out_dir=r"C:\Windows\Temp",
        credential=fts_pb2.NetworkCredential(user_name="u", password="SECRET-PW")))
    fails.append("local+credential should fail")
except grpc.RpcError as e:
    ok = e.code() == grpc.StatusCode.INVALID_ARGUMENT and "SECRET-PW" not in e.details()
    print("1. local+credential:", e.code(), "| pw leaked:", "SECRET-PW" in e.details())
    if not ok:
        fails.append("local+credential wrong error")

# 2. reserve dir token -> purge force=false -> INVALID_ARGUMENT
tk = stub.ReserveInbox(fts_pb2.ReserveInboxRequest(out_dir=os.path.join(ROOT, "downloads")))
try:
    stub.PurgeInbox(fts_pb2.PurgeRequest(token=tk.token, force=False))
    fails.append("force=false should fail")
except grpc.RpcError as e:
    print("2. force=false:", e.code())
    if e.code() != grpc.StatusCode.INVALID_ARGUMENT:
        fails.append("force=false wrong code")

# 3. file-bound token + force=true -> PERMISSION_DENIED
ftk = stub.ReserveInbox(fts_pb2.ReserveInboxRequest(
    out_dir=os.path.join(ROOT, "downloads"), file_name="x.bin"))
try:
    stub.PurgeInbox(fts_pb2.PurgeRequest(token=ftk.token, force=True))
    fails.append("file-bound purge should fail")
except grpc.RpcError as e:
    print("3. file-bound purge:", e.code(), "|", e.details())
    if e.code() != grpc.StatusCode.PERMISSION_DENIED:
        fails.append("file-bound purge wrong code")

# 4. dir token + force=true -> OK(统计);目录本身保留
probe = os.path.join(ROOT, "downloads", "purge_probe.txt")
open(probe, "w").write("x")
rep = stub.PurgeInbox(fts_pb2.PurgeRequest(token=tk.token, force=True))
print(f"4. purge ok: files={rep.files_deleted} dirs={rep.directories_deleted}")
if not (os.path.isdir(os.path.join(ROOT, "downloads")) and not os.path.exists(probe)):
    fails.append("purge did not clean dir")

# 5. nonexistent token -> NOT_FOUND
try:
    stub.PurgeInbox(fts_pb2.PurgeRequest(token="00000000-0000-0000-0000-000000000000", force=True))
    fails.append("bad token should fail")
except grpc.RpcError as e:
    print("5. bad token:", e.code())
    if e.code() != grpc.StatusCode.NOT_FOUND:
        fails.append("bad token wrong code")

print("T10:", "PASS" if not fails else "FAIL " + "; ".join(fails))
sys.exit(0 if not fails else 1)
