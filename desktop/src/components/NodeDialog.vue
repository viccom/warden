<script setup>
import { ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { store, reloadNodes } from '../store';
import { showToast } from '../api';

const name = ref('');
const url = ref('http://127.0.0.1:8789');
const token = ref('');
const err = ref('');

async function save() {
  err.value = '';
  if (!url.value.trim()) { err.value = '地址必填'; return; }
  try {
    await invoke('nodes_add', {
      node: { name: name.value.trim() || url.value.trim(), url: url.value.trim(), token: token.value.trim() },
    });
    await reloadNodes();
    store.nodeDialog = false;
    showToast('节点已添加:' + (name.value.trim() || url.value.trim()));
  } catch (e) {
    err.value = String(e);
  }
}
</script>

<template>
  <div class="overlay" @click.self="store.nodeDialog = false">
    <div class="modal">
      <h3>添加 warden 节点</h3>
      <div class="field">
        <label>显示名(可选)</label>
        <input v-model="name" placeholder="如:现场主机 A / 本机 CLI 版" />
      </div>
      <div class="field">
        <label>HTTP API 地址 *</label>
        <input v-model="url" placeholder="http://192.168.1.100:8789(本机 CLI 版默认 http://127.0.0.1:8789)" />
      </div>
      <div class="field">
        <label>鉴权 token(目标未配置则留空)</label>
        <input v-model="token" type="password" placeholder="daemon.auth_token" />
      </div>
      <div class="err">{{ err }}</div>
      <div class="actions">
        <button @click="store.nodeDialog = false">取消</button>
        <button class="primary" @click="save">添加</button>
      </div>
    </div>
  </div>
</template>
