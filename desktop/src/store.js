// 全局响应式状态:节点注册(本地内嵌 + 远程)、聚合服务表、过滤与日志目标。
import { reactive, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { clientFor } from './api';

export const store = reactive({
  ready: false,
  local: null,        // {url, token} 内嵌 daemon(Rust 侧注入)
  nodes: [],          // 远程节点 [{name,url,token}]
  nodeFilter: 'ALL',  // 'ALL' 或节点 url
  health: {},         // url -> {ok, running, services, failed, version} | {ok:false, error}
  services: [],       // 聚合:[{...ServiceStatus, _node:{name,url,local}}]
  group: 'ALL',       // 'ALL' | 'UNGROUPED' | 组名
  keyword: '',
  logTarget: null,    // {nodeUrl, name} 选中看日志的服务
  nodeDialog: false,  // 添加节点对话框
  serviceForm: null,  // {mode:'create'|'edit', nodeUrl, name?}
  configEditor: null, // {nodeUrl, name} 配置文件编辑器
});

/// 本地 + 远程全部节点。
export function allNodes() {
  const list = [];
  if (store.local) {
    list.push({ name: '本机(内嵌)', url: store.local.url, token: store.local.token, local: true });
  }
  for (const n of store.nodes) list.push({ ...n, local: false });
  return list;
}

function findNode(url) {
  return allNodes().find(n => n.url === url);
}

async function refreshOnce() {
  const nodes = allNodes();
  const next = [];
  await Promise.all(nodes.map(async n => {
    const c = clientFor(n);
    try {
      const [h, svcs] = await Promise.all([c.health(), c.services()]);
      store.health[n.url] = { ok: true, ...h };
      for (const s of svcs) {
        next.push({ ...s, _node: { name: n.name, url: n.url, local: n.local } });
      }
    } catch (e) {
      store.health[n.url] = { ok: false, error: String(e.message || e) };
    }
  }));
  // 稳定排序:节点 → priority → name
  next.sort((a, b) =>
    a._node.url.localeCompare(b._node.url) ||
    (a.priority - b.priority) ||
    a.name.localeCompare(b.name)
  );
  store.services = next;
}

export async function refresh() {
  return refreshOnce();
}

// ── UI 偏好持久化(WebView2 localStorage,纯前端状态,不涉后端)──────────
// 节点注册表(资产)在 Rust 侧 nodes.json;这里只存视图偏好。
const PREFS_KEY = 'warden_prefs';

function loadPrefs() {
  try {
    const p = JSON.parse(localStorage.getItem(PREFS_KEY) || '{}');
    if (p.nodeFilter) store.nodeFilter = p.nodeFilter;
    if (p.group) store.group = p.group;
  } catch { /* 坏数据忽略,用默认 */ }
}

function savePrefs() {
  try {
    localStorage.setItem(
      PREFS_KEY,
      JSON.stringify({ nodeFilter: store.nodeFilter, group: store.group })
    );
  } catch { /* 存储满等异常忽略 */ }
}

export async function initStore() {
  loadPrefs();
  store.local = await invoke('local_node_info');
  store.nodes = await invoke('nodes_list');
  await refreshOnce();
  // 偏好引用的节点/组可能已不存在(节点被删),失效则回退
  if (store.nodeFilter !== 'ALL' && !allNodes().some(n => n.url === store.nodeFilter)) {
    store.nodeFilter = 'ALL';
  }
  const valid = new Set(['ALL', 'UNGROUPED', ...groupsOfVisible().map(g => g.name)]);
  if (!valid.has(store.group)) store.group = 'ALL';
  store.ready = true;
  watch(() => [store.nodeFilter, store.group], savePrefs);
  setInterval(refreshOnce, 2000);
}

export async function reloadNodes() {
  store.nodes = await invoke('nodes_list');
}

/// 当前过滤可见的服务(节点 + 组 + 关键词)。
export function visibleServices() {
  let list = store.services;
  if (store.nodeFilter !== 'ALL') list = list.filter(s => s._node.url === store.nodeFilter);
  if (store.group === 'UNGROUPED') list = list.filter(s => !s.group);
  else if (store.group !== 'ALL') list = list.filter(s => s.group === store.group);
  const kw = store.keyword.trim().toLowerCase();
  if (kw) {
    list = list.filter(s =>
      s.name.toLowerCase().includes(kw) ||
      (s.display_name || '').toLowerCase().includes(kw)
    );
  }
  return list;
}

/// 组 chips 数据(基于节点过滤,不含关键词/组过滤,保证 chips 全量)。
export function groupsOfVisible() {
  let list = store.services;
  if (store.nodeFilter !== 'ALL') list = list.filter(s => s._node.url === store.nodeFilter);
  const m = new Map();
  for (const s of list) if (s.group) m.set(s.group, (m.get(s.group) || 0) + 1);
  return [...m.entries()]
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

/// 组级(或全量)启停:作用于当前节点过滤范围内的全部节点。
export async function groupAction(group, act) {
  const nodes = allNodes().filter(n =>
    store.nodeFilter === 'ALL' ? true : n.url === store.nodeFilter
  );
  await Promise.allSettled(
    nodes.map(n => {
      const c = clientFor(n);
      return group === '__all__'
        ? (act === 'start' ? c.startAll() : c.stopAll())
        : c.groupAction(group, act);
    })
  );
  setTimeout(refreshOnce, 300);
}

export async function serviceAction(nodeUrl, name, act) {
  const n = findNode(nodeUrl);
  if (!n) return;
  try {
    await clientFor(n).action(name, act);
  } catch (e) {
    alert(`操作失败(${name} ${act}):${e.message}`);
  }
  setTimeout(refreshOnce, 300);
}
