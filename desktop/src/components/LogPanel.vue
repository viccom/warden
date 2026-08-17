<script setup>
import { ref, watch, onBeforeUnmount, computed } from 'vue';
import { store } from '../store';
import { clientFor } from '../api';

const lines = ref([]);       // [{ts, stream, level, text}]
const kw = ref('');
const level = ref('');
const paused = ref(false);
const autoScroll = ref(true);
const MAX_LINES = 1200;

let es = null;          // EventSource(无 token 节点)
let pollTimer = null;   // 轮询降级(有 token 节点,EventSource 不能带 header)
let pollBase = '';
let pollName = '';
let lastTs = '';

const target = computed(() => store.logTarget);

function tokenOf(url) {
  if (store.local && store.local.url === url) return store.local.token;
  return store.nodes.find(n => n.url === url)?.token || '';
}

function closeStream() {
  if (es) { es.close(); es = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

function append(l) {
  if (lines.value.length >= MAX_LINES) lines.value.splice(0, lines.value.length - MAX_LINES);
  lines.value.push(l);
}

async function snapshot(c, name) {
  try {
    const ls = await c.logs(name, 300);
    lines.value = ls;
    if (ls.length) lastTs = ls[ls.length - 1].ts || '';
  } catch (e) {
    lines.value = [];
  }
}

function openStream(nodeUrl, name) {
  closeStream();
  lines.value = [];
  lastTs = '';
  const token = tokenOf(nodeUrl);
  const c = clientFor({ url: nodeUrl, token });
  snapshot(c, name);
  if (!token) {
    // SSE 实时流
    es = new EventSource(`${c.base}/api/v1/services/${encodeURIComponent(name)}/logs/stream`);
    es.onmessage = ev => {
      if (paused.value) return;
      try { append(JSON.parse(ev.data)); } catch { /* 忽略坏行 */ }
      scroll();
    };
    es.onerror = () => { /* eventsource 自动重连 */ };
  } else {
    // 轮询降级(EventSource 无法带 Authorization)
    pollBase = nodeUrl;
    pollName = name;
    pollTimer = setInterval(async () => {
      if (paused.value) return;
      try {
        const ls = await c.logs(name, 60);
        const fresh = ls.filter(l => (l.ts || '') > lastTs);
        if (fresh.length) {
          for (const l of fresh) append(l);
          lastTs = fresh[fresh.length - 1].ts || lastTs;
          scroll();
        }
      } catch { /* 节点暂时不可达 */ }
    }, 1500);
  }
}

function scroll() {
  if (!autoScroll.value) return;
  requestAnimationFrame(() => {
    const el = document.getElementById('logview');
    if (el) el.scrollTop = el.scrollHeight;
  });
}

const shown = computed(() => {
  const k = kw.value.trim().toLowerCase();
  return lines.value.filter(l => {
    if (level.value && (l.level || 'unknown') !== level.value) return false;
    if (k && !String(l.text).toLowerCase().includes(k)) return false;
    return true;
  });
});

watch(
  () => target.value && target.value.nodeUrl + '/' + target.value.name,
  v => {
    if (v) openStream(target.value.nodeUrl, target.value.name);
    else closeStream();
  },
  { immediate: true }
);

onBeforeUnmount(closeStream);

function levelClass(l) {
  return l.level || 'unknown';
}
function timeOf(l) {
  return (l.ts || '').slice(11, 19);
}
</script>

<template>
  <section class="logpanel" v-if="target">
    <div class="bar">
      <span class="title">日志 · {{ target.name }}</span>
      <input v-model="kw" placeholder="关键词过滤…" class="kw" />
      <select v-model="level" class="lv">
        <option value="">全部等级</option>
        <option value="error">error</option>
        <option value="warn">warn</option>
        <option value="info">info</option>
        <option value="debug">debug</option>
        <option value="unknown">unknown</option>
      </select>
      <label class="chk"><input type="checkbox" v-model="autoScroll" /> 自动滚动</label>
      <label class="chk"><input type="checkbox" v-model="paused" /> 暂停</label>
      <button @click="lines = []">清空</button>
      <button @click="store.logTarget = null">关闭</button>
    </div>
    <div id="logview" class="view">
      <div v-if="!shown.length" class="empty">暂无日志(等级/关键词过滤中?)</div>
      <div
        v-for="(l, i) in shown"
        :key="i"
        class="line"
        :class="[levelClass(l), l.stream === 'stderr' ? 'stderr' : 'stdout']"
      >{{ timeOf(l) }} {{ l.text }}</div>
    </div>
  </section>
</template>

<style scoped>
.logpanel {
  height: 260px;
  min-height: 120px;
  border-top: 1px solid var(--border);
  background: var(--bg2);
  display: flex;
  flex-direction: column;
}
.bar {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 8px 14px;
  border-bottom: 1px solid var(--border);
}
.title { font-size: 13px; font-weight: 600; }
.kw { width: 160px; }
.lv { width: 110px; }
.chk { display: flex; gap: 5px; align-items: center; font-size: 12px; color: var(--text-dim); }
.chk input { width: auto; }
.view {
  flex: 1;
  overflow-y: auto;
  font-family: Consolas, "Cascadia Mono", monospace;
  font-size: 12px;
  padding: 6px 14px;
  user-select: text;
}
.empty { color: var(--text-dim); padding: 20px; text-align: center; }
.line { white-space: pre-wrap; word-break: break-all; padding: 1px 0; color: var(--text); }
.line.stderr { color: var(--log-stderr); }
.line.error { color: var(--red); }
.line.warn { color: var(--yellow); }
.line.debug { color: var(--text-dim); }
.line.info { color: var(--text); }
</style>
