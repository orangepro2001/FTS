# fts-updater 设计:远程更新器(填 IP 一键换版)

> W9 工作包。状态:**已实现并端到端验证**(2026-10-06)。
> 关联:plan/fts-api-spec.md(Capabilities)、plan/dev-log.md(最新条目在最上方)。

## 1. 要解决的问题

客户端要部署到几十台主机。现状(旧流程):拷贝整个 `dist\` 到目标机 → 手工停 daemon
(杀进程,否则 fts.exe 被占用无法覆盖)→ 覆盖文件 → 重启 → 逐台验证。几十台重复劳动。

目标:在开发机/跳板机上一个 GUI,**填可达 IP 即可把新版 dist 推过去**,自动完成
"停 daemon → 覆盖 → 重启 → 验证",支持批量。

## 2. 总体架构

```
┌────────────────────┐  ①Capabilities(版本/指纹/started_at)   ┌──────────────┐
│  fts-updater GUI   │──────────────────────────────────────▶ │ 目标机 daemon │
│  (Tauri2+Svelte5)  │  ②ReserveInbox(暂存目录)               │  fts.exe     │
│  updater\          │◀────────────token─────────────────────│              │
│                    │  ③Submit/fts 传输(QUIC 优先,pin 指纹)  │              │
│                    │  ④BeginUpdate(暂存目录,files,force)   │              │
│                    │  ⑤轮询 Capabilities(started_at 变化)   │  apply-update│
│                    │                                        │  (新版 exe)  │
└────────────────────┘                                        └──────────────┘
```

- **数据面**:新版二进制经 fts 自有传输(ReserveInbox → Submit,QUIC 优先 TCP 回退,
  BLAKE3 校验)推到目标机暂存目录 `.fts\update\stage-<ts>\`。
- **控制面**:新增 gRPC `BeginUpdate`(proto W9)。daemon 校验后以 DETACHED 拉起
  **暂存区里的新版 fts.exe** 执行 `apply-update`,自己随后 `process::exit(0)`。
- **自替换**(src/update.rs,运行体是暂存副本):
  1. 等旧 pid 退出(200ms 轮询,15s 超时后 taskkill /F 兜底);
  2. 备份将被覆盖文件 → `<cert_cache>\update\backup-<ts>\`;
  3. 复制暂存文件覆盖安装目录(默认不动 config.json);
  4. **拉起安装目录的 `fts.exe start`**(不能调 ctl_start —— current_exe 是暂存副本,
     否则 daemon 会以暂存区为宿主,暂存目录日后被清理会连坐);
  5. 失败回滚(恢复备份重试启动);结果写 `<cert_cache>\update\result.json`,过程写 apply.log。

## 3. 关键设计决策与踩坑

| 决策/坑 | 说明 |
|---|---|
| 为什么不 WinRM/SSH | 每台主机要开服务/防火墙/凭据管理;daemon 的 gRPC 已监听 0.0.0.0(orch 在用),复用它零部署成本。代价:首次需手工部署一次带 BeginUpdate 的新版(一次性引导)。 |
| `started_at` 确认重启 | 本地回环上"退出→换文件→重启"全程 <2s,靠"连接空窗"判断重启是**竞态**(实测 1s 轮询整个窗口都被错过)。给 CapabilitiesReply 加 `uint64 started_at = 6`(进程启动 unix 秒),重启确认无竞态。 |
| `output()` 死锁 | apply-update 用 `Command::output()` 捕获 `fts start` 输出会**永久卡死**:daemon 孙进程经 `bInheritHandles=TRUE` 继承管道写句柄,EOF 永不到来。修法:stdio 直写 apply.log 文件 + `status()`。Python e2e 同理(fts start 不能 capture_output)。 |
| Start 子命令短参 | `fts start` 的 config 只有 `-c` 没有 `--config`,apply-update 重启时传 `-c`(曾因传 --config 回滚)。 |
| 版本判定规则 | dist\VERSION 存在且 != 旧版本:必须看到新 fts_version 才算成功(重启后仍旧 = 已回滚);VERSION == 旧版本(同版本重部署):started_at 变化即成功;无 VERSION:started_at 变化即成功。 |
| config.json 不覆盖 | 各机配置不同;GUI 勾选"同时覆盖 config.json"才推。 |
| 安全边界 | gRPC 控制面无鉴权(现状),BeginUpdate 等于远程换二进制,与无鉴权 Submit 同级风险,**仅限可信内网**。apply-update 只接受纯文件名(拒绝路径分隔符/`..`)。 |

## 4. 文件结构(本次新增/修改)

```
proto/fts.proto            # +BeginUpdate RPC、UpdateRequest;CapabilitiesReply +started_at(tag6)
src/update.rs              # 核心:apply_update(自替换)、push_update(全链路推送)、swap/restore、
                           #   connect_client/capabilities(pub,GUI 复用)
src/daemon.rs              # DaemonCfg/Registry +config_path/started_at;resolve_config/cert_dir/
                           #   process_alive 改 pub(crate);capabilities(REST) +started_at
src/grpc.rs                # begin_update 实现(校验→拉起→延迟自退)
src/main.rs                # CLI:+ApplyUpdate(隐藏)、+PushUpdate;config_path 透传
tests/update_e2e.py        # 全链路端到端:起 daemon→push-update→验证换文件+重启
updater/                   # Tauri2+Svelte5 GUI(独立子项目)
  src-tauri/src/main.rs    # 命令:probe/update_host/read_dist_version/load_hosts/save_hosts
  src/App.svelte           # 主机表格、批量更新、日志面板;浏览器预览 mock
  tools/make_icon.py       # 占位图标生成器(BMP 格式 ICO,需 BITMAPINFOHEADER)
orch/fts_pb2*.py           # gen_proto.py 重新生成(纯增量,orch 代码零改动)
build.ps1                  # dist\VERSION;+ -Updater 开关(npm install + tauri build)
```

## 5. 使用方式

一次性引导(每台主机仅一次):按老方式把**带 W9 能力的 dist** 拷到目标机并重启 daemon。
之后:

- GUI:`build.ps1 -Updater` 构建 → `updater\src-tauri\target\release\fts-updater.exe`
  → 填 IP(默认 :50051)→ 检查/更新,支持批量。
- CLI:`fts push-update --to <host[:50051]> --dist <目录> [--overwrite-config] [--force]`。

## 6. 遗留事项

- result.json 仅落在目标机本地(REST 仅本地监听),GUI 无法远程读取;回滚场景只能靠
  版本判定 + 人工查看 apply.log。如需精确回传可加 GetUpdateStatus RPC(未做)。
- BeginUpdate 未做鉴权/限流(与现状一致);若未来加 TLS/凭据,更新链路需同步接入。
- 图标为程序生成的占位图,可换正式美术资源(替换 updater/src-tauri/icons/icon.ico)。
