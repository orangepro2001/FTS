"""fts 编排 server 的命令行入口(headless,便于脚本/CI/文档复现)。

用法(在 orch/ 目录下):
    # 1) 探活:确认两端存在且活着
    python cli.py --config config.json --check

    # 2) 并行编排多任务:从 A1→B1 / A2→B2 / A3→B3
    python cli.py --config config.json \
        --pairs "A1:B1,A2:B2,A3:B3" \
        --src-path C:\\data\\payload.bin --dst-path D:\\recv

    # 3) 运行 config.json 里的 tasks 列表
    python cli.py --config config.json --run-config-tasks

    # 4) 启动 Web UI(浏览器操作)
    python cli.py --config config.json --serve --host 127.0.0.1 --port 8080
"""

from __future__ import annotations

import argparse
import asyncio
import sys

import core
from core import Orchestrator, OrchestrationError, TERMINAL


def _fmt_bytes(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.1f}{unit}" if unit != "B" else f"{n}B"
        n /= 1024.0
    return f"{n}B"


async def cmd_check(orch: Orchestrator) -> int:
    print("== 主机探活(控制面 gRPC + 数据面 TCP)==\n")
    eps = await orch.check_all()
    ok = True
    for ep in eps:
        mark = "OK " if (ep.control_ok and ep.data_ok) else ("CTL" if ep.control_ok else "XX ")
        if not (ep.control_ok and ep.data_ok):
            ok = False
        print(
            f"[{mark}] {ep.name:<4} {ep.grpc_addr:<20} "
            f"指纹={ep.fingerprint[:12] or '-':<12} "
            f"数据口={ep.data_addr or '-':<20} "
            f"protocol={ep.protocol_version or '-':<3} fts={ep.fts_version or '-'}"
        )
        if ep.error:
            print(f"        ! {ep.error}")
    print()
    return 0 if ok else 1


async def _watch(orch: Orchestrator) -> None:
    """订阅事件总线,按状态/进度变化打印紧凑行。"""
    q = orch.subscribe()
    last: dict[str, tuple] = {}
    try:
        while True:
            ev = await q.get()
            if ev.get("type") != "task_update":
                continue
            t = ev["task"]
            key = (t["state"], int(t["percent"] // 10))
            if last.get(t["id"]) == key:
                continue
            last[t["id"]] = key
            eta = f" ETA {t['eta_seconds']}s" if t["eta_seconds"] is not None else ""
            extra = ""
            if t["state"] in TERMINAL and t["state"] != "COMPLETED":
                extra = f"  [{t['error']}:{t['error_code']}] {t['error_message']}"
            print(
                f"  {t['name']:<10} {t['state']:<9} "
                f"{t['percent']:5.1f}%  {_fmt_bytes(t['bytes_done'])}/{_fmt_bytes(t['bytes_total'])}"
                f"  {t['files_done']}/{t['files_total']} 文件{eta}{extra}"
            )
    except asyncio.CancelledError:
        pass
    finally:
        orch.unsubscribe(q)


async def _run_tasks(orch: Orchestrator, tasks: list[core.Task]) -> int:
    print(f"== 并行编排 {len(tasks)} 个任务 ==\n")
    watcher = asyncio.create_task(_watch(orch))
    await asyncio.sleep(0)  # 让 watcher 先订阅
    results = await asyncio.gather(*(orch.run_task(t) for t in tasks))
    await asyncio.sleep(0.2)
    watcher.cancel()

    print("\n== 结果汇总 ==\n")
    all_ok = True
    for t in results:
        passed = t.state == "COMPLETED" and t.checksum_ok
        all_ok = all_ok and passed
        tag = "PASS" if passed else "FAIL"
        print(
            f"[{tag}] {t.name:<10} {t.src}->{t.dst}  {t.state:<9} "
            f"checksum_ok={t.checksum_ok}  {_fmt_bytes(t.bytes_done)}  "
            f"{t.ended_at - t.started_at:.2f}s"
        )
        if t.warnings:
            for w in t.warnings:
                print(f"        ~ {w}")
        if not passed and t.error_message:
            print(f"        ! {t.error}({t.error_code}): {t.error_message}")
    print()
    return 0 if all_ok else 1


def _build_tasks_from_pairs(orch: Orchestrator, pairs: str, src_path: str, dst_path: str,
                            transport: str, streams: int, chunk_size: int,
                            purge: bool = False) -> list[core.Task]:
    tasks: list[core.Task] = []
    for i, pair in enumerate(pairs.split(","), start=1):
        pair = pair.strip()
        if not pair:
            continue
        src, _, dst = pair.partition(":")
        if not src or not dst:
            raise OrchestrationError(f"--pairs 格式错误(应为 A1:B1,A2:B2): {pair!r}")
        task = orch.new_task(
            name=f"{src}→{dst}",
            src=src.strip(),
            src_path=src_path,
            dst=dst.strip(),
            dst_path=dst_path,
            transport=transport,
            streams=streams,
            chunk_size=chunk_size,
            purge=purge,
        )
        task.id = f"{task.id}-{i}"
        tasks.append(task)
    return tasks


async def _amain(args: argparse.Namespace) -> int:
    cfg = core.load_config(args.config)
    orch = Orchestrator(cfg)

    if args.check:
        return await cmd_check(orch)
    if args.pairs:
        if not args.src_path or not args.dst_path:
            print("错误: --pairs 需要同时提供 --src-path 与 --dst-path", file=sys.stderr)
            return 2
        tasks = _build_tasks_from_pairs(
            orch, args.pairs, args.src_path, args.dst_path, args.transport, args.streams,
            args.chunk_size, args.purge,
        )
        return await _run_tasks(orch, tasks)
    if args.run_config_tasks:
        tasks = [orch.new_task(**t) for t in cfg.get("tasks", [])]
        if not tasks:
            print("config 中没有 tasks", file=sys.stderr)
            return 2
        return await _run_tasks(orch, tasks)

    print("未指定动作:请用 --check / --pairs / --run-config-tasks / --serve", file=sys.stderr)
    return 2


def main() -> int:
    p = argparse.ArgumentParser(description="fts 编排 server(命令行)")
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true", help="探活所有主机")
    g.add_argument("--pairs", help="并行编排,如 \"A1:B1,A2:B2,A3:B3\"")
    g.add_argument("--run-config-tasks", action="store_true", help="运行 config 里的 tasks")
    g.add_argument("--serve", action="store_true", help="启动 Web UI")
    p.add_argument("--config", default="config.json")
    p.add_argument("--src-path", default="", help="--pairs 用:源文件路径")
    p.add_argument("--dst-path", default="", help="--pairs 用:目标目录")
    p.add_argument("--transport", default="", help="auto|quic|tcp(空=config 默认)")
    p.add_argument("--streams", type=int, default=0)
    p.add_argument("--chunk-size", type=int, default=0)
    p.add_argument("--purge", action="store_true",
                   help="--pairs 用:传输前清空目标目录(危险,不可恢复)")
    p.add_argument("--host", default="127.0.0.1", help="--serve 用")
    p.add_argument("--port", type=int, default=8080, help="--serve 用")
    args = p.parse_args()

    import os
    if not os.path.exists(args.config):
        print(
            f"错误: 配置文件不存在: {args.config}\n"
            f"config.json 不是自带的,请从样例复制后填入你的主机地址:\n"
            f"    copy orch\\config.example.json orch\\config.json\n"
            f"字段说明见 orch\\README.md(endpoints 只需 name + grpc_addr,指纹/端口自动发现)",
            file=sys.stderr,
        )
        return 2

    # --serve 不能在 asyncio.run 内调用(uvicorn.run 自带事件循环)
    if args.serve:
        from app import serve

        serve(args.config, host=args.host, port=args.port)
        return 0
    try:
        return asyncio.run(_amain(args))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
