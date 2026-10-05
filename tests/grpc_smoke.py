# -*- coding: utf-8 -*-
"""fts gRPC end-to-end smoke test (P5 W4 acceptance: T9).

Flow: Capabilities -> ReserveInbox -> Submit(2 files) -> Observe(stream) -> Status.

Run: python tests\\grpc_smoke.py <fingerprint_hex>
"""
import os
import subprocess
import sys
import time

import grpc

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# generate pb2 from proto/
GEN = os.path.join(HERE, "_gen")
os.makedirs(GEN, exist_ok=True)
subprocess.run(
    [sys.executable, "-m", "grpc_tools.protoc", "-I", os.path.join(ROOT, "proto"),
     "--python_out", GEN, "--grpc_python_out", GEN,
     os.path.join(ROOT, "proto", "fts.proto")],
    check=True,
)
sys.path.insert(0, GEN)

import fts_pb2, fts_pb2_grpc  # noqa: E402

FP = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("FTS_FP", "")
SRC = sys.argv[2:] or ["f1.bin", "f2.bin"]

ch = grpc.insecure_channel("127.0.0.1:50051")
stub = fts_pb2_grpc.FtsStub(ch)

caps = stub.Capabilities(fts_pb2.Empty())
print("capabilities:", caps.protocol_version, caps.fts_version, hex(caps.capabilities))

tk = stub.ReserveInbox(fts_pb2.ReserveInboxRequest(out_dir=os.path.join(ROOT, "inbox_g")))
print("inbox token:", tk.token[:8], "expires_in", tk.expires_in, "s")

h = stub.Submit(fts_pb2.TransferRequest(
    paths=SRC,
    dst_addr="127.0.0.1:4433",
    fingerprint=FP,
    ticket=tk.token,
    streams=4,
))
print("task:", h.task_id)

progress_seen = False
final = None
for ev in stub.Observe(fts_pb2.ObserveRequest(task_id=h.task_id)):
    import json
    e = json.loads(ev.raw_json)
    t = e.get("type")
    if t == "progress":
        progress_seen = True
    elif t in ("file_done", "result"):
        print(" ", ev.raw_json)
    if t == "result":
        final = e
        break

st = stub.Status(fts_pb2.TaskRef(task_id=h.task_id))
print("status:", st.state, "checksum_ok =", st.checksum_ok)
print("progress events seen:", progress_seen)

ok = (final and final.get("state") == "COMPLETED" and final.get("checksum_ok")
      and st.state == "COMPLETED" and st.checksum_ok and progress_seen)
print("T9:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
