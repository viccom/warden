<script setup>
import { computed } from 'vue';
import { store, visibleServices, groupsOfVisible, groupAction, serviceAction } from '../store';
import { clientFor, showToast } from '../api';

const groups = computed(() => groupsOfVisible());
const services = computed(() => visibleServices());

const stateName = s => s?.state?.state || 'unknown';
const isRun = s => stateName(s) === 'running';

function portsText(s) {
  return (s.listening_ports || []).map(p => `${p.proto}:${p.local_port}`).join(' ');
}

function selectLog(s) {
  store.logTarget =
    store.logTarget && store.logTarget.nodeUrl === s._node.url && store.logTarget.name === s.name
      ? null
      : { nodeUrl: s._node.url, name: s.name };
}
const logging = s =>
  store.logTarget && store.logTarget.nodeUrl === s._node.url && store.logTarget.name === s.name;

async function edit(s) {
  store.serviceForm = { mode: 'edit', nodeUrl: s._node.url, name: s.name };
}
async function remove(s) {
  if (!confirm(`删除服务「${s.name}」(节点 ${s._node.name})?(运行中不可删)`)) return;
  try {
    await clientFor({ url: s._node.url, token: tokenOf(s._node.url) }).deleteService(s.name);
    showToast('已删除:' + s.name);
  } catch (e) {
    showToast('删除失败:' + e.message, true);
  }
}
function tokenOf(url) {
  const n = store.local && store.local.url === url
    ? { token: store.local.token }
    : store.nodes.find(n => n.url === url);
  return n?.token || '';
}

function metaLine(s) {
  return [
    s.group ? '[' + s.group + ']' : '',
    s.priority ? 'P' + s.priority : '',
    s.state?.pid ? 'PID ' + s.state.pid : '',
    'CPU ' + (s.metrics ? s.metrics.cpu_percent.toFixed(1) : '0.0') + '%',
    '内存 ' + (s.metrics ? Math.round(s.metrics.memory_kb / 1024) : 0) + ' MB',
    '重启 ' + s.restart_count,
    portsText(s),
  ].filter(Boolean).join(' · ');
}

async function createService() {
  if (!store.nodes.length && !store.local) return;
  store.serviceForm = { mode: 'create', nodeUrl: store.nodeFilter !== 'ALL' ? store.nodeFilter : (store.local?.url || store.nodes[0].url) };
}
</script>

<template>
  <section class="area">
    <div class="toolbar">
      <div class="groups">
        <div
          class="chip"
          :class="{ on: store.group === 'ALL' }"
          @click="store.group = 'ALL'"
        >全部</div>
        <div
          class="chip"
          :class="{ on: store.group === 'UNGROUPED' }"
          @click="store.group = 'UNGROUPED'"
        >未分组</div>
        <div
          v-for="g in groups"
          :key="g.name"
          class="chip group"
          :class="{ on: store.group === g.name }"
        >
          <span class="gname" @click="store.group = g.name">{{ g.name }} <i>{{ g.count }}</i></span>
          <span class="gops">
            <button class="mini" title="组内全部启动(按优先级)" @click="groupAction(g.name, 'start')">▶</button>
            <button class="mini" title="组内全部停止(逆序)" @click="groupAction(g.name, 'stop')">■</button>
          </span>
        </div>
      </div>
      <div class="ops">
        <input v-model="store.keyword" placeholder="搜索服务名…" class="search" />
        <button @click="createService">＋ 新增服务</button>
        <button @click="groupAction('__all__', 'start')">全部启动</button>
        <button @click="groupAction('__all__', 'stop')">全部停止</button>
      </div>
    </div>

    <div class="cards">
      <div v-if="!services.length" class="empty">
        无匹配服务 —— 可在左侧添加节点,或点「＋ 新增服务」
      </div>
      <div
        v-for="s in services"
        :key="s._node.url + '/' + s.name"
        class="card"
        :class="{ sel: logging(s) }"
        @click="selectLog(s)"
      >
        <div class="top">
          <div class="nm" :title="s.name">
            <span class="dname">{{ s.display_name || s.name }}</span>
            <span class="node-tag" :title="s._node.url">{{ s._node.name }}</span>
          </div>
          <div class="right">
            <span class="badge" :class="stateName(s)"><span class="d"></span>{{ stateName(s) }}</span>
            <span
              class="hdot"
              :class="(s.health?.status) || 'unknown'"
              :title="'健康:' + ((s.health?.status) || 'unknown')"
            ></span>
          </div>
        </div>
        <div class="meta" :title="metaLine(s)">{{ metaLine(s) }}</div>
        <div class="acts" @click.stop>
          <button @click="serviceAction(s._node.url, s.name, isRun(s) ? 'stop' : 'start')">
            {{ isRun(s) ? '停止' : '启动' }}
          </button>
          <button @click="serviceAction(s._node.url, s.name, 'restart')">重启</button>
          <button @click="edit(s)">编辑</button>
          <button class="danger" @click="remove(s)">删除</button>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.area { flex: 1; min-height: 0; display: flex; flex-direction: column; padding: 12px 14px 6px; }
.toolbar { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; margin-bottom: 10px; }
.groups { display: flex; gap: 6px; flex-wrap: wrap; flex: 1; min-width: 300px; }
.chip {
  padding: 4px 12px;
  border-radius: 999px;
  background: var(--bg2);
  border: 1px solid var(--border);
  font-size: 12px;
  cursor: pointer;
  display: flex;
  align-items: center;
  gap: 6px;
}
.chip.on { border-color: var(--accent); color: var(--accent); }
.chip.group .gname i { font-style: normal; color: var(--text-dim); font-size: 11px; }
.chip .gops { display: none; gap: 2px; }
.chip.group:hover .gops { display: inline-flex; }
.mini { padding: 0 5px; font-size: 10px; line-height: 16px; }
.ops { display: flex; gap: 8px; align-items: center; }
.search { width: 180px; }

.cards { flex: 1; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; }
.empty { color: var(--text-dim); grid-column: 1 / -1; text-align: center; padding: 48px 0; }
.card {
  background: var(--bg2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  cursor: pointer;
}
.card:hover { border-color: var(--accent-dim); }
.card.sel { border-color: var(--accent); }
.card .top { display: flex; justify-content: space-between; align-items: center; gap: 8px; }
.card .nm { min-width: 0; display: flex; align-items: center; gap: 8px; }
.dname { font-weight: 600; font-size: 14px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.node-tag {
  font-size: 10px;
  color: var(--text-dim);
  background: var(--bg3);
  padding: 1px 6px;
  border-radius: 4px;
  white-space: nowrap;
}
.card .right { display: flex; align-items: center; gap: 8px; }
.card .meta { font-size: 12px; color: var(--text-dim); min-height: 16px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.card .acts { display: flex; gap: 6px; }
.card .acts button { flex: 1; padding: 3px 0; font-size: 12px; }
</style>
