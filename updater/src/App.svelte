<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";

  interface Host {
    name: string;
    addr: string;
    status: "idle" | "probing" | "checking" | "updating" | "up" | "down" | "ok" | "fail";
    version: string;
    error: string;
  }

  // 纯浏览器预览(无 Tauri 运行时)时的演示数据,便于 UI 调试
  const inTauri = "__TAURI_INTERNALS__" in window;
  const call = (cmd: string, args?: Record<string, unknown>): Promise<any> =>
    inTauri ? invoke(cmd, args) : Promise.resolve(mockInvoke(cmd));

  function mockInvoke(cmd: string): any {
    if (cmd === "load_hosts")
      return [
        { name: "13 Handler", addr: "192.168.1.60" },
        { name: "13 FM1", addr: "192.168.1.61" },
        { name: "14 BM", addr: "192.168.1.73" },
      ];
    if (cmd === "read_dist_version") return "0.1.0";
    return null;
  }

  let hosts = $state<Host[]>([]);
  let distDir = $state("C:\\develop\\File-Transfer\\dist");
  let distVersion = $state("");
  let overwriteConfig = $state(false);
  let force = $state(false);
  let busy = $state(false);
  let logs = $state<string[]>([]);
  let newName = $state("");
  let newAddr = $state("");
  let logBox = $state<HTMLDivElement | null>(null);

  function log(line: string) {
    const t = new Date().toLocaleTimeString("zh-CN", { hour12: false });
    logs.push(`[${t}] ${line}`);
    if (logs.length > 800) logs.splice(0, logs.length - 800);
    setTimeout(() => logBox?.scrollTo({ top: logBox.scrollHeight }), 10);
  }

  function persist() {
    if (inTauri)
      call("save_hosts", {
        hosts: hosts.map((h) => ({ name: h.name, addr: h.addr })),
      }).catch(() => {});
  }

  onMount(async () => {
    await listen<{ addr: string; line: string }>("update-log", (e) => {
      log(`${e.payload.addr} ${e.payload.line}`);
    });
    const saved: { name: string; addr: string }[] = await call("load_hosts");
    hosts = saved.map((h) => ({ ...h, status: "idle", version: "", error: "" }));
    distVersion = (await call("read_dist_version", { distDir })) ?? "";
    log(inTauri ? "更新器就绪" : "(浏览器预览模式,操作仅演示)");
  });

  async function refreshDistVersion() {
    distVersion = (await call("read_dist_version", { distDir })) ?? "";
    log(distVersion ? `本地 dist 版本: ${distVersion}` : "本地 dist 无 VERSION 文件(更新后将以重启确认)");
  }

  const statusText: Record<Host["status"], string> = {
    idle: "未检查", probing: "检查中", checking: "检查中", updating: "更新中",
    up: "在线", down: "不可达", ok: "已更新", fail: "失败",
  };
  const statusClass: Record<Host["status"], string> = {
    idle: "", probing: "spin", checking: "spin", updating: "spin",
    up: "good", down: "bad", ok: "good", fail: "bad",
  };

  async function checkOne(h: Host) {
    h.status = "checking";
    const r = await call("probe", { addr: h.addr });
    if (r.reachable) {
      h.status = "up";
      h.version = r.fts_version;
      h.error = "";
    } else {
      h.status = "down";
      h.error = r.error;
    }
  }

  async function updateOne(h: Host) {
    if (busy) return;
    busy = true;
    h.status = "updating";
    log(`—— ${h.name}(${h.addr}) 开始更新 ——`);
    try {
      const r = await call("update_host", {
        addr: h.addr, distDir, overwriteConfig, force,
      });
      if (r.ok) {
        h.status = "ok";
        h.version = r.new_version || h.version;
        log(`${h.addr} ✔ 更新成功 ${r.old_version} → ${r.new_version || "(同版本重部署)"}`);
      } else {
        h.status = "fail";
        h.error = r.detail;
        log(`${h.addr} ✘ 更新失败: ${r.detail}`);
      }
    } catch (e) {
      h.status = "fail";
      h.error = String(e);
      log(`${h.addr} ✘ 更新失败: ${e}`);
    }
    busy = false;
  }

  async function checkAll() {
    for (const h of hosts) await checkOne(h);
    log(`检查完成: ${hosts.filter((h) => h.status === "up" || h.status === "ok").length}/${hosts.length} 在线`);
  }

  async function updateAll() {
    const targets = hosts.filter(
      (h) => h.status === "up" || h.status === "fail" || (distVersion && h.version && h.version !== distVersion),
    );
    if (!targets.length) { log("没有可更新的主机(先检查)"); return; }
    log(`开始批量更新 ${targets.length} 台...`);
    for (const h of targets) await updateOne(h);
    log("批量更新结束");
  }

  function addHost() {
    if (!newAddr.trim()) return;
    hosts.push({
      name: newName.trim() || newAddr.trim(),
      addr: newAddr.trim(),
      status: "idle", version: "", error: "",
    });
    newName = ""; newAddr = "";
    persist();
  }

  function removeHost(h: Host) {
    hosts = hosts.filter((x) => x !== h);
    persist();
  }
</script>

<div class="layout">
  <header>
    <h1>fts 更新器</h1>
    <div class="dist">
      <label>新版 dist 目录</label>
      <input class="mono" bind:value={distDir} placeholder="C:\path\to\dist" />
      <button onclick={refreshDistVersion}>识别版本</button>
      {#if distVersion}<span class="chip good">dist {distVersion}</span>{:else}<span class="chip">无 VERSION</span>{/if}
    </div>
    <div class="toolbar">
      <button class="primary" disabled={busy} onclick={checkAll}>检查全部</button>
      <button class="primary" disabled={busy || !hosts.length} onclick={updateAll}>
        更新全部过期
      </button>
      <label class="opt"><input type="checkbox" bind:checked={overwriteConfig} /> 同时覆盖 config.json</label>
      <label class="opt"><input type="checkbox" bind:checked={force} /> 忽略进行中任务</label>
    </div>
  </header>

  <table>
    <thead>
      <tr><th>名称</th><th>地址 (IP[:50051])</th><th>状态</th><th>当前版本</th><th>操作</th></tr>
    </thead>
    <tbody>
      {#each hosts as h (h.addr + h.name)}
        <tr>
          <td><input class="plain" bind:value={h.name} onchange={persist} /></td>
          <td><input class="plain mono" bind:value={h.addr} onchange={persist} /></td>
          <td>
            <span class="chip {statusClass[h.status]}">{statusText[h.status]}</span>
            {#if h.error}<span class="errmsg" title={h.error}>{h.error}</span>{/if}
          </td>
          <td class="mono">{h.version || "—"}</td>
          <td class="actions">
            <button disabled={h.status === "updating"} onclick={() => checkOne(h)}>检查</button>
            <button class="accent" disabled={busy || h.status === "updating"} onclick={() => updateOne(h)}>
              更新
            </button>
            <button class="danger" onclick={() => removeHost(h)}>删除</button>
          </td>
        </tr>
      {/each}
      <tr class="addrow">
        <td><input placeholder="名称" bind:value={newName} /></td>
        <td><input placeholder="192.168.1.60" class="mono" bind:value={newAddr} onkeydown={(e) => e.key === "Enter" && addHost()} /></td>
        <td></td><td></td>
        <td><button class="accent" onclick={addHost}>+ 添加主机</button></td>
      </tr>
    </tbody>
  </table>

  <div class="logpanel">
    <div class="loghead">
      <span>更新日志</span>
      <button onclick={() => (logs = [])}>清空</button>
    </div>
    <div class="logbox mono" bind:this={logBox}>
      {#each logs as line}<div>{line}</div>{/each}
    </div>
  </div>
</div>

<style>
  .layout { height: 100%; display: flex; flex-direction: column; padding: 16px 20px; gap: 12px; }
  header { display: flex; flex-direction: column; gap: 10px; }
  h1 { font-size: 18px; font-weight: 600; }
  .dist { display: flex; align-items: center; gap: 8px; }
  .dist label { color: var(--text-dim); white-space: nowrap; }
  .dist input { flex: 1; }

  input, button {
    background: var(--bg-panel); color: var(--text);
    border: 1px solid var(--border); border-radius: 6px;
    padding: 6px 10px; font: inherit; outline: none;
  }
  input:focus, button:hover { border-color: var(--accent); }
  button { cursor: pointer; white-space: nowrap; }
  button:disabled { opacity: 0.45; cursor: not-allowed; }
  button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
  button.primary:hover:not(:disabled) { background: var(--accent-hover); }
  button.accent { border-color: var(--accent); color: var(--accent); }
  button.danger { color: var(--err); }
  button.danger:hover { border-color: var(--err); }
  .mono { font-family: var(--mono); font-size: 13px; }

  .toolbar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
  .opt { display: flex; align-items: center; gap: 5px; color: var(--text-dim); cursor: pointer; }

  .chip {
    display: inline-block; padding: 2px 10px; border-radius: 999px;
    background: var(--bg-hover); color: var(--text-dim); font-size: 12px;
  }
  .chip.good { color: var(--ok); }
  .chip.bad { color: var(--err); }
  .chip.spin { color: var(--accent); }
  .errmsg { color: var(--err); font-size: 12px; margin-left: 8px; overflow: hidden; text-overflow: ellipsis; max-width: 260px; display: inline-block; vertical-align: middle; }

  table { flex: 0 0 auto; border-collapse: collapse; }
  th { text-align: left; color: var(--text-dim); font-weight: 500; font-size: 12px; padding: 6px 10px; border-bottom: 1px solid var(--border); }
  td { padding: 6px 10px; border-bottom: 1px solid var(--border); }
  tr:hover td { background: var(--bg-hover); }
  td input { width: 100%; background: transparent; border-color: transparent; }
  td input:hover, td input:focus { border-color: var(--border); }
  .actions { white-space: nowrap; }
  .actions button { margin-right: 6px; }
  .addrow td { border-bottom: none; }
  .addrow input { background: var(--bg-panel); border-color: var(--border); }

  .logpanel { flex: 1; min-height: 140px; display: flex; flex-direction: column; background: var(--bg-panel); border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }
  .loghead { display: flex; justify-content: space-between; align-items: center; padding: 8px 12px; border-bottom: 1px solid var(--border); color: var(--text-dim); font-size: 12px; }
  .loghead button { padding: 2px 10px; font-size: 12px; }
  .logbox { flex: 1; overflow-y: auto; padding: 10px 12px; font-size: 12px; line-height: 1.7; }
</style>
