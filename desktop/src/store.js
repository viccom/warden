// 全局响应式状态:节点注册(本地内嵌 + 远程)、聚合服务表、过滤与日志目标。
import { reactive, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { clientFor, showToast } from './api';

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
  theme: 'dark',       // 'dark' | 'light',搭车 warden_prefs 持久化
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

/// 取节点 token(内嵌节点走 local,远程走 nodes)。放 store 避免 api↔store 循环依赖。
export function tokenOf(url) {
  if (store.local && store.local.url === url) return store.local.token;
  return store.nodes.find(n => n.url === url)?.token || '';
}

/// 节点是否在线(最近一次 2s 轮询成功)。首轮探测完成前(undefined)按离线处理。
/// 离线节点上不可做任何任务操作(增删改/启停),只能删除节点本身或等它恢复。
export function nodeOnline(url) {
  return store.health[url]?.ok === true;
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
    if (p.theme === 'light') store.theme = 'light';
  } catch { /* 坏数据忽略,用默认 */ }
}

function savePrefs() {
  try {
    localStorage.setItem(
      PREFS_KEY,
      JSON.stringify({ nodeFilter: store.nodeFilter, group: store.group, theme: store.theme })
    );
  } catch { /* 存储满等异常忽略 */ }
}

export function applyTheme(t) {
  store.theme = t;
  if (t === 'light') document.documentElement.dataset.theme = 'light';
  else delete document.documentElement.dataset.theme;
}

export function toggleTheme() {
  applyTheme(store.theme === 'light' ? 'dark' : 'light');
}

export async function initStore() {
  loadPrefs();
  applyTheme(store.theme);
  store.local = await invoke('local_node_info');
  store.nodes = await invoke('nodes_list');
  // 先亮界面:节点探测不等(离线节点的连接超时曾把主页面阻塞数十秒)。
  // 节点列表就绪即可渲染;探测结果异步到达,离线节点以「连接失败」呈现。
  if (store.nodeFilter !== 'ALL' && !allNodes().some(n => n.url === store.nodeFilter)) {
    store.nodeFilter = 'ALL';
  }
  store.ready = true;
  watch(() => [store.nodeFilter, store.group, store.theme], savePrefs);
  setInterval(refreshOnce, 2000);
  await refreshOnce();
  // 组偏好回退依赖首轮服务数据(组名来自服务列表),须在刷新后校验
  const valid = new Set(['ALL', 'UNGROUPED', ...groupsOfVisible().map(g => g.name)]);
  if (!valid.has(store.group)) store.group = 'ALL';
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

/// 组级(或全量)启停:只作用于当前节点过滤范围内的「在线」节点,
/// 离线节点跳过并提示(否则 allSettled 静默吞掉失败,用户以为已执行)。
export async function groupAction(group, act) {
  const nodes = allNodes().filter(n =>
    store.nodeFilter === 'ALL' ? true : n.url === store.nodeFilter
  );
  const online = nodes.filter(n => nodeOnline(n.url));
  const skipped = nodes.length - online.length;
  if (skipped > 0) showToast(`已跳过离线节点 ${skipped} 个(不可操作)`);
  if (!online.length) return;
  await Promise.allSettled(
    online.map(n => {
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
