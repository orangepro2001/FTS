"""fts 编排 server —— Web UI(FastAPI + SSE)。

server 自身暴露的 REST/SSE 与 daemon 的 /v1/* 无关,是专给 Web UI 用的编排接口:

    GET  /                      静态页
    GET  /api/hosts             主机列表(含探活结果)
    POST /api/hosts/check       重新探活所有主机
    GET  /api/tasks             任务列表
    POST /api/tasks             新建并启动任务
    POST /api/tasks/{id}/cancel 取消
    GET  /api/events            SSE:任务/主机增量
"""

from __future__ import annotations

import asyncio
import json
import pathlib
from typing import Optional

from fastapi import FastAPI, Request
from fastapi.responses import FileResponse, JSONResponse, StreamingResponse
from pydantic import BaseModel

import core
from core import Orchestrator

STATIC = pathlib.Path(__file__).resolve().parent / "static"


class TaskReq(BaseModel):
    src: str
    src_path: str
    dst: str
    dst_path: str
    name: Optional[str] = None
    transport: str = ""
    streams: int = 0
    chunk_size: int = 0
    purge: bool = False


def create_app(config_path: str) -> FastAPI:
    cfg = core.load_config(config_path)
    orch = Orchestrator(cfg)
    inflight: set[asyncio.Task] = set()

    app = FastAPI(title="fts 编排 server")

    @app.get("/")
    def index() -> FileResponse:
        return FileResponse(STATIC / "index.html")

    @app.get("/api/hosts")
    def hosts() -> list:
        return [e.snapshot() for e in orch.endpoints.values()]

    @app.post("/api/hosts/check")
    async def check_hosts() -> list:
        return [e.snapshot() for e in await orch.check_all()]

    @app.get("/api/tasks")
    def tasks() -> list:
        return [t.snapshot() for t in orch.tasks.values()]

    @app.post("/api/tasks")
    async def create_task(req: TaskReq) -> JSONResponse:
        try:
            task = orch.new_task(**req.model_dump())
        except Exception as e:  # noqa: BLE001
            return JSONResponse({"error": str(e)}, status_code=400)
        run = asyncio.create_task(orch.run_task(task))
        inflight.add(run)
        run.add_done_callback(inflight.discard)
        return JSONResponse(task.snapshot())

    @app.post("/api/tasks/{tid}/cancel")
    async def cancel_task(tid: str) -> dict:
        return {"ok": await orch.cancel(tid)}

    @app.get("/api/events")
    async def events(request: Request) -> StreamingResponse:
        q = orch.subscribe()

        async def gen():
            try:
                while not await request.is_disconnected():
                    try:
                        ev = await asyncio.wait_for(q.get(), timeout=15)
                    except asyncio.TimeoutError:
                        yield ": keepalive\n\n"
                        continue
                    yield f"data: {json.dumps(ev, ensure_ascii=False)}\n\n"
            finally:
                orch.unsubscribe(q)

        return StreamingResponse(gen(), media_type="text/event-stream")

    return app


def serve(config_path: str, host: str = "127.0.0.1", port: int = 8080) -> None:
    import uvicorn

    print(f"fts 编排 server Web UI: http://{host}:{port}  (Ctrl+C 退出)")
    uvicorn.run(create_app(config_path), host=host, port=port, log_level="warning")
