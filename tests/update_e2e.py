"""W9 远程更新端到端冒烟:本机模拟 BeginUpdate → 自替换 → 重启。

用法(任意目录):
    python tests\\update_e2e.py

前置: target\\release\\fts.exe 已构建;python 环境有 grpcio(orch 同款)。
流程:
  1. 临时目录搭一个"安装目录"(fts.exe + config.json),fts start 启动 daemon
  2. 直接到暂存目录放"新版"文件(fts.exe + VERSION 标记),模拟更新器推送完成
  3. gRPC BeginUpdate → 轮询等待 → 校验 VERSION 落地 / result.json ok=true
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from pathlib import Path

import grpc

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "orch"))
import fts_pb2 as pb          # noqa: E402
import fts_pb2_grpc as pbg    # noqa: E402

EXE = ROOT / "target" / "release" / "fts.exe"
REST = "127.0.0.1:5060"
GRPC_PORT = 50061


def http_json(method: str, url: str, body=None):
    req = urllib.request.Request(
        url, method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=2) as r:
        return json.loads(r.read())


def rest_alive() -> bool:
    try:
        http_json("GET", f"http://{REST}/v1/capabilities")
        return True
    except Exception:
        return False


def main() -> int:
    assert EXE.exists(), f"先构建: cargo build --release({EXE} 不存在)"
    if rest_alive():
        print(f"FAIL: {REST} 已被占用(上次测试残留 daemon?),先清理再跑", file=sys.stderr)
        return 2
    work = Path(tempfile.mkdtemp(prefix="fts-update-e2e-"))
    node = work / "nodeA"
    ok = False
    try:
        _run(work, node)
        ok = True
    finally:
        # 兜底清理:按 pid 文件强杀测试 daemon;失败时保留现场供排查
        pid_file = node / ".fts" / "daemon.pid"
        if pid_file.exists():
            pid = pid_file.read_text().strip()
            subprocess.run(["taskkill", "/F", "/PID", pid],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if ok:
            shutil.rmtree(work, ignore_errors=True)
        else:
            print(f"!! 失败现场保留: {work}", file=sys.stderr)
    print("PASS: 远程自更新端到端通过")
    return 0


def _run(work: Path, node: Path) -> None:
    (node / "downloads").mkdir(parents=True)
    shutil.copy2(EXE, node / "fts.exe")
    (node / "config.json").write_text(json.dumps({
        "rest_addr": REST,
        "grpc_addr": f"0.0.0.0:{GRPC_PORT}",
        "listen_addr": "0.0.0.0:4434",
        "out_dir": "downloads",
        "cert_cache": ".fts",
        "max_sessions": 2,
    }))

    # 注意:不能用 capture_output —— daemon 孙进程会经 bInheritHandles=TRUE 继承
    # 管道写句柄,导致本进程等 EOF 永久挂起;DEVNULL + daemon 自有日志文件即可。
    subprocess.run([str(node / "fts.exe"), "start", "-c", "config.json"],
                   cwd=node, check=True,
                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    assert rest_alive(), "daemon 未就绪"
    caps0 = http_json("GET", f"http://{REST}/v1/capabilities")
    print(f"[1] daemon 就绪 version={caps0['fts_version']} work={work}")

    # "新版 dist":fts.exe + fts.dll(nodeA 原本没有 dll,落地即可证明换文件成功)。
    # 不放 VERSION:严格规则要求 dist VERSION 与二进制 fts_version 一致,否则判失败。
    newdist = work / "newdist"
    newdist.mkdir()
    shutil.copy2(EXE, newdist / "fts.exe")
    shutil.copy2(ROOT / "target" / "release" / "fts.dll", newdist / "fts.dll")
    print("[2] 新版 dist 就绪;执行 push-update ...")
    r = subprocess.run([str(EXE), "push-update", "--to", f"127.0.0.1:{GRPC_PORT}",
                        "--dist", str(newdist)],
                       stdin=subprocess.DEVNULL, capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stdout, r.stderr, file=sys.stderr)
        raise AssertionError(f"push-update 失败(exit {r.returncode})")

    deadline = time.time() + 40
    marker = node / "fts.dll"
    while time.time() < deadline and not marker.exists():
        time.sleep(0.5)
    assert marker.exists(), "更新超时:新文件未落地"
    while time.time() < deadline and not rest_alive():
        time.sleep(0.5)
    assert rest_alive(), "更新后 REST 未恢复"
    caps1 = http_json("GET", f"http://{REST}/v1/capabilities")
    result_path = node / ".fts" / "update" / "result.json"
    while time.time() < deadline and not result_path.exists():
        time.sleep(0.5)
    result = json.loads(result_path.read_text())
    print(f"[3] version={caps1['fts_version']} dll={'ok'} result={result}")

    assert result["ok"] is True, f"result.json 报失败: {result}"
    assert result["from"] == caps0["fts_version"], "from 版本记录不符"


if __name__ == "__main__":
    sys.exit(main())
