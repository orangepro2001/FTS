"""从 proto/fts.proto 生成 Python gRPC stub。

用法(在 orch/ 目录下):
    python gen_proto.py

依赖: pip install grpcio-tools
生成 fts_pb2.py / fts_pb2_grpc.py 到本目录,供 core.py 直接 import。
"""

from __future__ import annotations

import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
PROTO = HERE.parent / "proto" / "fts.proto"


def main() -> int:
    if not PROTO.exists():
        print(f"找不到 proto: {PROTO}", file=sys.stderr)
        return 1
    subprocess.check_call(
        [
            sys.executable,
            "-m",
            "grpc_tools.protoc",
            f"-I{PROTO.parent}",
            f"--python_out={HERE}",
            f"--grpc_python_out={HERE}",
            str(PROTO),
        ]
    )
    print(f"生成完成: {HERE / 'fts_pb2.py'} / {HERE / 'fts_pb2_grpc.py'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
