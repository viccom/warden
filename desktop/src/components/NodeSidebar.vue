<script setup>
import { computed } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { store, allNodes, reloadNodes } from '../store';

defineEmits(['quit']);

const nodes = computed(() => allNodes());

function hOf(url) {
  const h = store.health[url];
  if (!h) return { cls: 'unknown', text: '…' };
  if (!h.ok) return { cls: 'off', text: '连接失败' };
  return {
    cls: h.failed > 0 ? 'unhealthy' : 'healthy',
    text: `${h.running}/${h.services} 运行${h.failed ? ' · ' + h.failed + ' 失败' : ''}`,
  };
}

async function del(n) {
  if (!confirm(`删除节点「${n.name}」?(不影响该 warden 本身)`)) return;
  await invoke('nodes_remove', { url: n.url });
  if (store.nodeFilter === n.url) store.nodeFilter = 'ALL';
  await reloadNodes();
}
</script>

<template>
  <aside>
    <div class="head">
      <span class="logo">warden</span>
      <span class="ver">桌面版</span>
    </div>

    <div class="nodes">
      <div class="sec">节点</div>

      <div
        class="node"
        :class="{ sel: store.nodeFilter === 'ALL' }"
        @click="store.nodeFilter = 'ALL'"
      >
        <span class="hdot" :class="nodes.every(n => store.health[n.url]?.ok) ? 'healthy' : 'unhealthy'"></span>
        <div class="meta">
          <div class="nm">全部节点</div>
          <div class="sub">{{ store.services.length }} 服务</div>
        </div>
      </div>

      <div
        v-for="n in nodes"
        :key="n.url"
        class="node"
        :class="{ sel: store.nodeFilter === n.url }"
        @click="store.nodeFilter = store.nodeFilter === n.url ? 'ALL' : n.url"
      >
        <span class="hdot" :class="hOf(n.url).cls" :title="hOf(n.url).text"></span>
        <div class="meta">
          <div class="nm">
            {{ n.name }}
            <span v-if="n.local" class="tag">内嵌</span>
          </div>
          <div class="sub">{{ hOf(n.url).text }}</div>
        </div>
        <button v-if="!n.local" class="mini del" @click.stop="del(n)" title="删除节点">✕</button>
      </div>
    </div>

    <div class="foot">
      <button @click="store.nodeDialog = true">＋ 添加节点</button>
      <button class="danger" @click="$emit('quit')">退出</button>
    </div>
  </aside>
</template>

<style scoped>
aside {
  background: var(--bg2);
  border-right: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  min-height: 0;
}
.head {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 16px 16px 12px;
}
.logo { font-size: 18px; font-weight: 700; }
.ver { font-size: 12px; color: var(--text-dim); }
.nodes { flex: 1; overflow-y: auto; padding: 0 10px; display: flex; flex-direction: column; gap: 4px; }
.sec { font-size: 11px; color: var(--text-dim); padding: 4px 6px 6px; letter-spacing: 1px; }
.node {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 9px 10px;
  border-radius: 8px;
  cursor: pointer;
  border: 1px solid transparent;
}
.node:hover { background: var(--bg3); }
.node.sel { background: var(--bg3); border-color: var(--accent-dim); }
.node .meta { flex: 1; min-width: 0; }
.node .nm {
  font-size: 13px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  display: flex;
  align-items: center;
  gap: 6px;
}
.tag {
  font-size: 10px;
  background: var(--accent-dim);
  color: #cfe0ff;
  border-radius: 4px;
  padding: 0 4px;
}
.node .sub { font-size: 11px; color: var(--text-dim); margin-top: 2px; }
.mini.del {
  padding: 1px 6px;
  font-size: 11px;
  border: none;
  background: transparent;
  color: var(--text-dim);
}
.mini.del:hover { color: var(--red); }
.foot {
  padding: 12px;
  display: flex;
  gap: 8px;
  border-top: 1px solid var(--border);
}
.foot button { flex: 1; }
</style>
