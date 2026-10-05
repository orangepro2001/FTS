"""fts 编排 server —— 核心引擎。

fts 是**纯 push 模型**:数据连接永远由源端发起去连目标端,没有任何"命令接收端去拉取"的 RPC。
因此 server 不是数据中转方,而是一个 gRPC 客户端,分别指挥两端:

    Capabilities(A)  探活 + 读 A 的身份(指纹/监听端口)
    Capabilities(B)  探活 + 读 B 的身份
    ReserveInbox(B)  在 B 上预注册落盘目录 → 得到 ticket
    Submit(A)        paths=源路径, dst_addr=B:数据端口, fingerprint=B指纹, ticket=ticket → task_id
    Observe(A)       订阅 NDJSON 事件流 → 进度 / 结果
    (之后 A 自己直连 B 传数据,字节不经过 server)
"""

from __future__ import annotations

import asyncio
import json
import socket
import time
from dataclasses import dataclass, field
from typing import Any, Optional

import grpc

import fts_pb2 as pb
import fts_pb2_grpc as pbg

TERMINAL = {"COMPLETED", "FAILED", "CANCELLED"}

# 编排层自身的错误码(与 fts 传输错误码 0-7 区分开,负数)
ORCH_ERR_GENERIC = -1
ORCH_ERR_UNREACHABLE = -2
ORCH_ERR_BAD_DATA_ADDR = -3


class OrchestrationError(Exception):
    def __init__(self, message: str, code: int = ORCH_ERR_GENERIC, name: str = "ORCH_ERROR"):
        super().__init__(message)
        self.code = code
        self.name = name


def split_host_port(addr: str) -> tuple[str, str]:
    """拆分 host:port;容忍 IPv6 的 [::1]:53 形式。"""
    if addr.startswith("["):
        host, _, port = addr[1:].partition("]:")
        return host, port
    host, _, port = addr.rpartition(":")
    return host, port


def resolve_data_addr(grpc_addr: str, listen_addr: str) -> Optional[str]:
    """把 daemon 上报的 listen_addr 变成编排方可连接的数据地址。

    listen_addr 通常是 0.0.0.0:4433(通配),此时用 grpc_addr 的 host 替换,
    得到「该主机真实 IP:数据端口」。
    """
    lh, lp = split_host_port(listen_addr or "")
    if not lp:
        return None
    gh, _ = split_host_port(grpc_addr)
    host = gh if lh in ("", "0.0.0.0", "::", "[::]") else lh
    if not host:
        return None
    return f"{host}:{lp}"


def tcp_probe(host: str, port: int, timeout: float) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


@dataclass
class Endpoint:
    """一台被编排的主机(客户端)。指纹/数据端口由 Capabilities 自动发现。"""

    name: str
    grpc_addr: str
    # —— 发现结果 ——
    alive: bool = False
    control_ok: bool = False
    data_ok: bool = False
    protocol_version: str = ""
    capabilities: int = 0
    fts_version: str = ""
    fingerprint: str = ""
    listen_addr: str = ""
    data_addr: str = ""
    error: str = ""
    checked_at: float = 0.0

    def snapshot(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "grpc_addr": self.grpc_addr,
            "alive": self.alive,
            "control_ok": self.control_ok,
            "data_ok": self.data_ok,
            "protocol_version": self.protocol_version,
            "fts_version": self.fts_version,
            "fingerprint": self.fingerprint,
            "fingerprint_short": self.fingerprint[:12] + "…" if self.fingerprint else "",
            "listen_addr": self.listen_addr,
            "data_addr": self.data_addr,
            "error": self.error,
            "checked_at": self.checked_at,
        }


@dataclass
class Task:
    """一个「从源路径传到目标路径」的编排任务。"""

    id: str
    name: str
    src: str
    src_path: str
    dst: str
    dst_path: str
    transport: str = ""
    streams: int = 0
    chunk_size: int = 0
    purge: bool = False
    # —— 运行时 ——
    source_task_id: str = ""
    state: str = "PENDING"
    checksum_ok: bool = False
    purge_files: int = 0
    purge_dirs: int = 0
    cur_bytes: int = 0
    cur_total: int = 0
    done_bytes: int = 0
    files_done: int = 0
    files_total: int = 0
    current_file: str = ""
    throughput_bps: int = 0
    eta_seconds: Optional[int] = None
    error_code: int = 0
    error: str = ""
    error_message: str = ""
    warnings: list[str] = field(default_factory=list)
    started_at: float = 0.0
    ended_at: float = 0.0

    @property
    def bytes_done(self) -> int:
        return self.done_bytes + self.cur_bytes

    @property
    def bytes_total(self) -> int:
        return self.done_bytes + self.cur_total

    def snapshot(self) -> dict[str, Any]:
        total = self.bytes_total
        done = self.bytes_done
        percent = round(100.0 * done / total, 1) if total > 0 else 0.0
        return {
            "id": self.id,
            "name": self.name,
            "src": self.src,
            "src_path": self.src_path,
            "dst": self.dst,
            "dst_path": self.dst_path,
            "transport": self.transport,
            "streams": self.streams,
            "chunk_size": self.chunk_size,
            "purge": self.purge,
            "purge_files": self.purge_files,
            "purge_dirs": self.purge_dirs,
            "source_task_id": self.source_task_id,
            "state": self.state,
            "checksum_ok": self.checksum_ok,
            "percent": percent,
            "bytes_done": done,
            "bytes_total": total,
            "files_done": self.files_done,
            "files_total": self.files_total,
            "current_file": self.current_file,
            "throughput_bps": self.throughput_bps,
            "eta_seconds": self.eta_seconds,
            "error_code": self.error_code,
            "error": self.error,
            "error_message": self.error_message,
            "warnings": list(self.warnings),
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "duration_seconds": round(self.ended_at - self.started_at, 2)
            if self.ended_at
            else None,
        }


class Orchestrator:
    def __init__(self, config: dict[str, Any]):
        self.rpc_timeout: float = float(config.get("timeout_sec", 3))
        self.defaults: dict[str, Any] = config.get("defaults", {}) or {}
        self.endpoints: dict[str, Endpoint] = {}
        for e in config.get("endpoints", []):
            self.endpoints[e["name"]] = Endpoint(name=e["name"], grpc_addr=e["grpc_addr"])
        self.tasks: dict[str, Task] = {}
        self._subs: set[asyncio.Queue] = set()

    # ---------- 事件总线(Web UI 的 SSE / CLI 消费) ----------

    def subscribe(self) -> asyncio.Queue:
        q: asyncio.Queue = asyncio.Queue()
        self._subs.add(q)
        return q

    def unsubscribe(self, q: asyncio.Queue) -> None:
        self._subs.discard(q)

    def _emit(self, event: dict[str, Any]) -> None:
        for q in list(self._subs):
            q.put_nowait(event)

    def _emit_task(self, task: Task) -> None:
        self._emit({"type": "task_update", "task": task.snapshot()})

    def _emit_host(self, ep: Endpoint) -> None:
        self._emit({"type": "host_update", "host": ep.snapshot()})

    # ---------- 探活 ----------

    async def check_host(self, name: str) -> Endpoint:
        ep = self.endpoints[name]
        ep.control_ok = ep.alive = ep.data_ok = False
        ep.error = ""
        ep.checked_at = time.time()
        try:
            async with grpc.aio.insecure_channel(ep.grpc_addr) as ch:
                rep = await asyncio.wait_for(
                    pbg.FtsStub(ch).Capabilities(pb.Empty()), timeout=self.rpc_timeout
                )
        except Exception as e:  # noqa: BLE001 —— 探活要吞掉一切连接类异常
            ep.error = f"控制面不可达: {e}"
            self._emit_host(ep)
            return ep

        ep.control_ok = ep.alive = True
        ep.protocol_version = rep.protocol_version
        ep.capabilities = rep.capabilities
        ep.fts_version = rep.fts_version
        ep.fingerprint = rep.fingerprint
        ep.listen_addr = rep.listen_addr
        if not rep.fingerprint:
            ep.error = "对端未上报指纹(daemon 版本过旧,需升级后重试)"
        ep.data_addr = resolve_data_addr(ep.grpc_addr, rep.listen_addr) or ""
        if ep.data_addr:
            _, port = split_host_port(ep.data_addr)
            host, _ = split_host_port(ep.data_addr)
            loop = asyncio.get_running_loop()
            ep.data_ok = await loop.run_in_executor(
                None, tcp_probe, host, int(port), self.rpc_timeout
            )
            if not ep.data_ok:
                ep.error = ep.error or "控制面正常,但数据端口不可达(防火墙/未监听?)"
        self._emit_host(ep)
        return ep

    async def check_all(self) -> list[Endpoint]:
        return list(await asyncio.gather(*(self.check_host(n) for n in self.endpoints)))

    # ---------- 任务编排 ----------

    def new_task(self, **kw: Any) -> Task:
        task = Task(
            id=kw.get("id") or f"t{int(time.time() * 1000) % 10**8}",
            name=kw.get("name") or f"{kw['src']}→{kw['dst']}",
            src=kw["src"],
            src_path=kw["src_path"],
            dst=kw["dst"],
            dst_path=kw["dst_path"],
            transport=kw.get("transport", self.defaults.get("transport", "auto")),
            streams=int(kw.get("streams", self.defaults.get("streams", 0)) or 0),
            chunk_size=int(kw.get("chunk_size", self.defaults.get("chunk_size", 0)) or 0),
            purge=bool(kw.get("purge", False)),
        )
        self.tasks[task.id] = task
        self._emit_task(task)
        return task

    async def run_task(self, task: Task) -> Task:
        task.state = "RUNNING"
        task.started_at = time.time()
        self._emit_task(task)
        try:
            src = self.endpoints.get(task.src)
            dst = self.endpoints.get(task.dst)
            if src is None or dst is None:
                raise OrchestrationError(f"未知主机: src={task.src} dst={task.dst}")

            # 1) 两端探活(每次运行都重新确认"存在且活着")
            await self.check_host(task.src)
            await self.check_host(task.dst)
            for ep in (src, dst):
                if not ep.alive:
                    raise OrchestrationError(
                        f"主机 {ep.name} 不存活: {ep.error}",
                        ORCH_ERR_UNREACHABLE,
                        "HOST_UNREACHABLE",
                    )
            if not dst.data_addr:
                raise OrchestrationError(
                    f"主机 {dst.name} 未上报数据监听地址,无法确定传输目标",
                    ORCH_ERR_BAD_DATA_ADDR,
                    "NO_DATA_ADDR",
                )
            if not dst.data_ok:
                task.warnings.append(f"目标数据端口探测失败({dst.data_addr}),仍尝试传输")
            if not dst.fingerprint:
                raise OrchestrationError(
                    f"主机 {dst.name} 未上报证书指纹,无法 pin",
                    ORCH_ERR_BAD_DATA_ADDR,
                    "NO_FINGERPRINT",
                )

            # 2) 在目标端预注册落盘目录 → ticket
            async with grpc.aio.insecure_channel(dst.grpc_addr) as ch:
                tok = await asyncio.wait_for(
                    pbg.FtsStub(ch).ReserveInbox(
                        pb.ReserveInboxRequest(out_dir=task.dst_path, file_name="")
                    ),
                    timeout=self.rpc_timeout,
                )
            ticket = tok.token

            # 2.5) 可选:传输前清空目标目录(force-clean 部署)。
            # 仅目录模式 token 允许 purge;必须显式 force=true(fts 三重防护之一)。
            if task.purge:
                async with grpc.aio.insecure_channel(dst.grpc_addr) as ch:
                    pr = await asyncio.wait_for(
                        pbg.FtsStub(ch).PurgeInbox(pb.PurgeRequest(token=ticket, force=True)),
                        timeout=self.rpc_timeout,
                    )
                task.purge_files = int(pr.files_deleted)
                task.purge_dirs = int(pr.directories_deleted)
                task.warnings.append(
                    f"已清空目标目录 {task.dst_path}:删除 {task.purge_files} 个文件 / "
                    f"{task.purge_dirs} 个文件夹"
                )
                self._emit_task(task)

            # 3) 在源端提交发送:dst_addr 指向目标数据端口,指纹 pin 目标证书
            async with grpc.aio.insecure_channel(src.grpc_addr) as ch:
                handle = await asyncio.wait_for(
                    pbg.FtsStub(ch).Submit(
                        pb.TransferRequest(
                            paths=[task.src_path],
                            dst_addr=dst.data_addr,
                            fingerprint=dst.fingerprint,
                            ticket=ticket,
                            streams=task.streams,
                            chunk_size=task.chunk_size,
                            transport=task.transport or "",
                        )
                    ),
                    timeout=self.rpc_timeout,
                )
            task.source_task_id = handle.task_id
            self._emit_task(task)

            # 4) 订阅事件流(进度/结果)
            await self._observe(src.grpc_addr, task)
        except OrchestrationError as e:
            task.state = "FAILED"
            task.error_code = e.code
            task.error = e.name
            task.error_message = str(e)
        except grpc.aio.AioRpcError as e:
            task.state = "FAILED"
            task.error_code = ORCH_ERR_GENERIC
            task.error = "GRPC_ERROR"
            task.error_message = f"{e.details()} ({e.code().name})"
        except Exception as e:  # noqa: BLE001
            task.state = "FAILED"
            task.error_code = ORCH_ERR_GENERIC
            task.error = "ORCH_ERROR"
            task.error_message = str(e)
        finally:
            # 5) 收尾:拉一次 Status 补齐 error_message / 兜底终态(防漏事件)
            if task.source_task_id:
                await self._finalize(task)
            if task.state not in TERMINAL:
                task.state = "COMPLETED" if task.checksum_ok else "FAILED"
                if not task.checksum_ok and not task.error:
                    task.error, task.error_code = "NO_RESULT", ORCH_ERR_GENERIC
                    task.error_message = "事件流结束但未收到终态且 Status 无终态"
            task.ended_at = time.time()
            self._emit_task(task)
        return task

    async def _observe(self, grpc_addr: str, task: Task) -> None:
        """订阅 Observe 流;流不会在 result 后自动关闭,故收到终态主动断开。

        传输极快时可能订阅晚于 result → 事件全丢;因此超时轮询 Status 兜底。
        """
        async with grpc.aio.insecure_channel(grpc_addr) as ch:
            call = pbg.FtsStub(ch).Observe(pb.ObserveRequest(task_id=task.source_task_id))
            q: asyncio.Queue = asyncio.Queue()

            async def pump() -> None:
                try:
                    async for ev in call:
                        q.put_nowait(("event", ev.raw_json))
                except Exception as e:  # noqa: BLE001
                    q.put_nowait(("error", str(e)))
                finally:
                    q.put_nowait(("done", None))

            pump_task = asyncio.create_task(pump())
            try:
                while True:
                    try:
                        kind, val = await asyncio.wait_for(q.get(), timeout=2.0)
                    except asyncio.TimeoutError:
                        st = await self._status(grpc_addr, task.source_task_id)
                        if st is None or st.state in TERMINAL:
                            break
                        continue
                    if kind == "event":
                        self._handle_event(task, val)
                        if task.state in TERMINAL:
                            break
                    elif kind == "done":
                        break
                    else:  # error
                        task.warnings.append(f"事件流中断: {val}")
                        break
            finally:
                pump_task.cancel()
                call.cancel()

    def _handle_event(self, task: Task, raw_json: str) -> None:
        try:
            ev = json.loads(raw_json)
        except json.JSONDecodeError:
            return
        kind = ev.get("type")
        if kind == "task":
            task.state = ev.get("state", "RUNNING")
        elif kind == "file":
            task.files_total = int(ev.get("total") or 0)
            task.current_file = ev.get("name", "")
            task.cur_bytes = 0
            task.cur_total = 0
        elif kind == "progress":
            task.cur_bytes = int(ev.get("bytes_done") or 0)
            task.cur_total = int(ev.get("bytes_total") or 0)
            task.throughput_bps = int(ev.get("throughput_bps") or 0)
            eta = ev.get("eta_seconds")
            task.eta_seconds = int(eta) if eta is not None else None
        elif kind == "file_done":
            task.done_bytes += int(ev.get("bytes") or 0)
            task.files_done += 1
            task.cur_bytes = task.cur_total = 0
        elif kind == "degradation":
            msg = f"降级 {ev.get('from')} → {ev.get('to')}: {ev.get('reason')}"
            task.warnings.append(msg)
        elif kind == "result":
            task.state = ev.get("state", task.state)
            task.checksum_ok = bool(ev.get("checksum_ok"))
            if ev.get("error"):
                task.error = ev["error"]
                task.error_code = int(ev.get("error_code") or 0)
        elif kind == "error":
            task.warnings.append(f"事件流告警: {ev.get('message')}")
        # path / 未知类型:忽略(契约:未知事件被下游忽略)
        self._emit_task(task)

    async def _status(self, grpc_addr: str, task_id: str):
        try:
            async with grpc.aio.insecure_channel(grpc_addr) as ch:
                return await asyncio.wait_for(
                    pbg.FtsStub(ch).Status(pb.TaskRef(task_id=task_id)), timeout=self.rpc_timeout
                )
        except grpc.aio.AioRpcError as e:
            if e.code() == grpc.StatusCode.NOT_FOUND:
                return None
            return None
        except Exception:  # noqa: BLE001
            return None

    async def _finalize(self, task: Task) -> None:
        addr = self.endpoints[task.src].grpc_addr
        st = await self._status(addr, task.source_task_id)
        if st is None:
            return
        if task.state not in TERMINAL:
            task.state = st.state or "FAILED"
        task.checksum_ok = task.checksum_ok or st.checksum_ok
        if st.error_code:
            task.error_code = st.error_code
            task.error = st.error
            task.error_message = st.error_message or task.error_message

    async def cancel(self, task_id: str) -> bool:
        task = self.tasks.get(task_id)
        if task is None or not task.source_task_id:
            return False
        if task.state in TERMINAL:
            return True
        addr = self.endpoints[task.src].grpc_addr
        try:
            async with grpc.aio.insecure_channel(addr) as ch:
                await asyncio.wait_for(
                    pbg.FtsStub(ch).Cancel(pb.TaskRef(task_id=task.source_task_id)),
                    timeout=self.rpc_timeout,
                )
        except Exception as e:  # noqa: BLE001
            task.warnings.append(f"取消失败: {e}")
            return False
        # Cancel 不回发 result 事件,由编排层置终态
        task.state = "CANCELLED"
        task.error_code = 6
        task.error = "CANCELLED"
        task.ended_at = time.time()
        self._emit_task(task)
        return True


def load_config(path: str) -> dict[str, Any]:
    with open(path, "r", encoding="utf-8-sig") as f:
        return json.load(f)
