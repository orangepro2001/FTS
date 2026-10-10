# -*- coding: utf-8 -*-
"""fts 文件级并发传输端到端验收(协议 v2,fts-more-fast)。

场景(全部走 CLI,QUIC + --insecure):
  T1 20 个混合大小文件(嵌套子目录)一次发送:
     - COMPLETED + checksum_ok
     - 接收树与源树逐字节一致
     - 并发生效断言:第 2 个 file 事件先于第 1 个 file_done
  T2 中断续传:传输中途杀掉 sender,重发同目录 → COMPLETED,无 .fts-part 残留
  T3 取消/中断:杀掉 sender 后 recv 会话在超时内退出

输出吞吐数字供 dev-log 记录。

运行:python tests\\multi_file_concurrency.py
前置:cargo build --release(或 target/debug/fts.exe 存在)
注意:不要用 PowerShell 改写本文件(PS5.1 会按 GBK 重编码毁掉中文)。
"""
import filecmp
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
WORK = os.path.join(HERE, "_multi_file_tmp")


def find_exe():
    cand = os.environ.get("FTS_EXE")
    for c in [cand,
              os.path.join(ROOT, "target", "release", "fts.exe"),
              os.path.join(ROOT, "target", "debug", "fts.exe")]:
        if c and os.path.isfile(c):
            return c
    print("fts.exe not found; run cargo build --release first")
    sys.exit(2)


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def make_tree(root):
    """20 个文件:嵌套子目录 + 混合大小(首个大文件给并发断言留时间窗)。"""
    os.makedirs(root, exist_ok=True)
    sizes = [8 << 20] + [0, 1, 4096, 1 << 20, 256 << 10] * 3 + [128 << 10] * 4
    for i, size in enumerate(sizes):
        sub = os.path.join(root, f"d{i % 3}")
        os.makedirs(sub, exist_ok=True)
        p = os.path.join(sub, f"f{i:02d}.bin")
        with open(p, "wb") as f:
            f.write(os.urandom(size))
    return len(sizes)


def trees_equal(a, b):
    cmp = filecmp.dircmp(a, b)
    if cmp.left_only or cmp.right_only or cmp.funny_files:
        return False, f"diff: left={cmp.left_only} right={cmp.right_only} funny={cmp.funny_files}"
    _, mismatch, errors = filecmp.cmpfiles(
        a, b, cmp.common_files, shallow=False)
    if mismatch or errors:
        return False, f"mismatch={mismatch} errors={errors}"
    for d in cmp.common_dirs:
        ok, why = trees_equal(os.path.join(a, d), os.path.join(b, d))
        if not ok:
            return False, why
    return True, ""


def total_bytes(root):
    n = 0
    for dp, _, fn in os.walk(root):
        for f in fn:
            n += os.path.getsize(os.path.join(dp, f))
    return n


def start_recv(exe, out_dir, port):
    """启动一次性 recv 会话;就绪判据 = stderr 出现"监听"行。
    不能用 TCP 探测:CLI recv 只处理一个会话,探测连接会被当成会话消费掉。"""
    p = subprocess.Popen(
        [exe, "recv", "--listen", f"127.0.0.1:{port}", "--out", out_dir,
         "--json"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    lines = []

    def pump():
        for raw in p.stderr:
            lines.append(raw.decode("utf-8", "replace"))

    threading.Thread(target=pump, daemon=True).start()
    deadline = time.time() + 10
    while time.time() < deadline:
        if any("监听" in l or "listen" in l for l in lines):
            return p
        if p.poll() is not None:
            raise AssertionError(
                f"recv 提前退出 rc={p.returncode}: {''.join(lines)[-500:]}")
        time.sleep(0.05)
    raise AssertionError(f"recv 10s 未就绪: {''.join(lines)[-500:]}")


def run_send(exe, src, port, timeout=120):
    t0 = time.time()
    proc = subprocess.Popen(
        [exe, "send", src, "--to", f"127.0.0.1:{port}",
         "--insecure", "--json"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    out, err = proc.communicate(timeout=timeout)
    events = []
    for line in out.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return proc.returncode, events, time.time() - t0, err.decode("utf-8", "replace")


def kill(proc):
    if proc.poll() is None:
        proc.kill()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def scenario_basic(exe, name):
    """T1:20 文件混合大小,一次发送。"""
    base = os.path.join(WORK, name)
    src = os.path.join(base, "src")
    dst = os.path.join(base, "dst")
    shutil.rmtree(base, ignore_errors=True)
    n_files = make_tree(src)
    port = free_port()
    recv = start_recv(exe, dst, port)
    try:
        code, events, dur, err = run_send(exe, src, port)
        total = total_bytes(src)
        result = [e for e in events if e.get("type") == "result"]
        assert code == 0, f"send 退出码 {code}: {err[-800:]}"
        assert result and result[0].get("state") == "COMPLETED", f"result={result}"
        assert result[0].get("checksum_ok") is True, f"checksum_ok={result[0]}"
        # 树逐字节一致
        ok, why = trees_equal(src, os.path.join(dst, os.path.basename(src)))
        assert ok, f"树不一致: {why}"
        # 并发生效:第 2 个 file 事件先于第 1 个 file_done
        kinds = [e.get("type") for e in events]
        assert "file" in kinds and "file_done" in kinds, f"缺 file 事件: {kinds}"
        second_file = kinds.index("file", kinds.index("file") + 1)
        first_done = kinds.index("file_done")
        assert second_file < first_done, (
            f"未并发:第 2 个 file(#{second_file})晚于第 1 个 file_done(#{first_done})")
        # job 聚合字段存在(additive;任务快于 1s 时可能无 progress tick,则跳过)
        prog = [e for e in events if e.get("type") == "progress"]
        jobs = [e.get("job") for e in prog if e.get("job")]
        if prog:
            assert jobs, f"progress 事件缺 job 附加字段: {prog[0]}"
            assert jobs[-1]["files_total"] == n_files, f"files_total={jobs[-1]['files_total']}"
        print(f"T1 [{name}] PASS  files={n_files} bytes={total} dur={dur:.2f}s "
              f"throughput={total / dur / (1 << 20):.1f} MB/s")
    finally:
        kill(recv)


def scenario_resume(exe, name):
    """T2:中途杀 sender → 重发自动续传。"""
    base = os.path.join(WORK, name)
    src = os.path.join(base, "src")
    dst = os.path.join(base, "dst")
    shutil.rmtree(base, ignore_errors=True)
    os.makedirs(src)
    for i in range(3):
        with open(os.path.join(src, f"big{i}.bin"), "wb") as f:
            f.write(os.urandom(16 << 20))
    port = free_port()
    recv = start_recv(exe, dst, port)
    try:
        # 第一轮:中途杀掉(等接收侧出现半成品再杀)
        snd = subprocess.Popen(
            [exe, "send", src, "--to", f"127.0.0.1:{port}", "--insecure", "--json"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.time() + 30
        while time.time() < deadline:
            if snd.poll() is not None:
                break
            if os.path.isdir(dst) and any(
                    f.endswith(".fts-part") for dp, _, fs in os.walk(dst) for f in fs):
                break
            time.sleep(0.1)
        time.sleep(0.3)
        kill(snd)
        kill(recv)
        assert os.path.isdir(dst), "中断前接收目录为空(第一轮没传出任何数据)"
        # 第二轮:重发 → 自动续传直到完成
        recv2 = start_recv(exe, dst, port)
        try:
            code, events, dur, err = run_send(exe, src, port)
            result = [e for e in events if e.get("type") == "result"]
            assert code == 0, f"重发退出码 {code}: {err[-800:]}"
            assert result and result[0].get("state") == "COMPLETED", f"result={result}"
            ok, why = trees_equal(src, os.path.join(dst, os.path.basename(src)))
            assert ok, f"续传后树不一致: {why}"
            leftovers = [os.path.join(dp, f)
                         for dp, _, fs in os.walk(dst) for f in fs
                         if f.endswith(".fts-part") or f.endswith(".fts-part.meta")]
            assert not leftovers, f"残留半成品: {leftovers}"
            print(f"T2 [{name}] PASS  resume completed dur={dur:.2f}s")
        finally:
            kill(recv2)
    finally:
        kill(recv)


def scenario_cancel(exe, name):
    """T3:杀 sender 后 recv 会话应退出(不挂死)。
    用 TCP 强制:进程被杀 → RST → 接收端立即感知;QUIC 静默掉线依赖 idle timeout,
    当前实现(与 v1 一致)会等到超时,不属于本验收范围。"""
    base = os.path.join(WORK, name)
    src = os.path.join(base, "src")
    dst = os.path.join(base, "dst")
    shutil.rmtree(base, ignore_errors=True)
    os.makedirs(src)
    with open(os.path.join(src, "huge.bin"), "wb") as f:
        f.write(os.urandom(64 << 20))
    port = free_port()
    recv = start_recv(exe, dst, port)
    try:
        snd = subprocess.Popen(
            [exe, "send", src, "--to", f"127.0.0.1:{port}", "--insecure",
             "--transport", "tcp"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(1.5)
        kill(snd)
        # recv(CLI 一次性会话)应在 15s 内退出
        try:
            rc = recv.wait(timeout=15)
            print(f"T3 [{name}] PASS  recv exited rc={rc}")
        except subprocess.TimeoutExpired:
            raise AssertionError("sender 被杀后 recv 15s 未退出(会话挂死)")
    finally:
        kill(recv)


def main():
    exe = find_exe()
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    scenario_basic(exe, "basic")
    scenario_resume(exe, "resume")
    scenario_cancel(exe, "cancel")
    shutil.rmtree(WORK, ignore_errors=True)
    print("ALL PASS")


if __name__ == "__main__":
    main()
