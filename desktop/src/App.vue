<script setup>
import { onMounted } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { store, initStore } from './store';
import NodeSidebar from './components/NodeSidebar.vue';
import ServiceArea from './components/ServiceArea.vue';
import LogPanel from './components/LogPanel.vue';
import NodeDialog from './components/NodeDialog.vue';
import ServiceForm from './components/ServiceForm.vue';
import ConfigEditor from './components/ConfigEditor.vue';

onMounted(async () => {
  try {
    await initStore();
  } catch (e) {
    store.ready = true;
    alert('初始化失败:' + e);
  }
});

async function quit() {
  if (!confirm('退出 warden 桌面版?内嵌 daemon 管理的运行中子进程将被优雅停止。')) return;
  await invoke('quit_app');
}
</script>

<template>
  <div class="app" v-if="store.ready">
    <NodeSidebar @quit="quit" />
    <main>
      <ServiceArea />
      <LogPanel />
    </main>
    <NodeDialog v-if="store.nodeDialog" />
    <ServiceForm v-if="store.serviceForm" />
    <ConfigEditor v-if="store.configEditor" />
  </div>
  <div class="booting" v-else>
    <div class="spinner"></div>
    <div>正在启动内嵌 daemon…</div>
  </div>
</template>

<style scoped>
.booting {
  height: 100%;
  display: flex;
  flex-direction: column;
  gap: 14px;
  align-items: center;
  justify-content: center;
  color: var(--text-dim);
}
.spinner {
  width: 28px;
  height: 28px;
  border: 3px solid var(--border);
  border-top-color: var(--accent);
  border-radius: 50%;
  animation: spin 0.9s linear infinite;
}
@keyframes spin { to { transform: rotate(360deg); } }
</style>
