<script setup>
import { ref, onMounted } from 'vue';
import { store, refresh } from '../store';
import { clientFor, showToast } from '../api';

const form = store.serviceForm; // {mode, nodeUrl, name?}
const tokenOf = () => {
  if (store.local && store.local.url === form.nodeUrl) return store.local.token;
  return store.nodes.find(n => n.url === form.nodeUrl)?.token || '';
};

const editing = form.mode === 'edit';
const f = ref(blank());
const err = ref('');

function blank() {
  return {
    name: '',
    display_name: '',
    description: '',
    command: '',
    args: '',
    working_dir: '',
    environment: '',
    output_encoding: '',
    group: '',
    priority: 0,
    auto_start: false,
    auto_restart: false,
    graceful_timeout_secs: 10,
    health_host: '',
    health_port: '',
    ui_url: '',
  };
}

function toCfg() {
  return {
    name: f.value.name.trim(),
    display_name: f.value.display_name.trim(),
    description: f.value.description.trim(),
    command: f.value.command.trim(),
    args: f.value.args.split('\n').map(s => s.trim()).filter(Boolean),
    working_dir: f.value.working_dir.trim() || null,
    environment: parseEnv(f.value.environment),
    output_encoding: f.value.output_encoding.trim() || null,
    group: f.value.group.trim() || null,
    priority: parseInt(f.value.priority) || 0,
    auto_start: f.value.auto_start,
    auto_restart: f.value.auto_restart,
    graceful_timeout_secs: parseInt(f.value.graceful_timeout_secs) || 10,
    ui_url: f.value.ui_url.trim() || null,
    health:
      f.value.health_host.trim() && f.value.health_port
        ? { type: 'tcp', host: f.value.health_host.trim(), port: parseInt(f.value.health_port) }
        : null,
  };
}

function parseEnv(text) {
  const env = {};
  for (const line of text.split('\n')) {
    const i = line.indexOf('=');
    if (i > 0) env[line.slice(0, i).trim()] = line.slice(i + 1).trim();
  }
  return env;
}

onMounted(async () => {
  if (!editing) return;
  try {
    const cfg = await clientFor({ url: form.nodeUrl, token: tokenOf() }).config(form.name);
    f.value = {
      name: cfg.name || '',
      display_name: cfg.display_name || '',
      description: cfg.description || '',
      command: cfg.command || '',
      args: (cfg.args || []).join('\n'),
      working_dir: cfg.working_dir || '',
      environment: Object.entries(cfg.environment || {}).map(([k, v]) => k + '=' + v).join('\n'),
      output_encoding: cfg.output_encoding || '',
      group: cfg.group || '',
      priority: cfg.priority ?? 0,
      auto_start: !!cfg.auto_start,
      auto_restart: !!cfg.auto_restart,
      graceful_timeout_secs: cfg.graceful_timeout_secs ?? 10,
      health_host: cfg.health?.host || '',
      health_port: cfg.health?.port || '',
      ui_url: cfg.ui_url || '',
    };
  } catch (e) {
    err.value = '加载配置失败:' + e.message;
  }
});

async function save() {
  err.value = '';
  const cfg = toCfg();
  if (!cfg.name || !cfg.command) { err.value = '名称和可执行文件路径必填'; return; }
  if (cfg.group.includes('/')) { err.value = "分组名不能含 '/'"; return; }
  const c = clientFor({ url: form.nodeUrl, token: tokenOf() });
  try {
    if (editing) await c.updateService(form.name, cfg);
    else await c.createService(cfg);
    store.serviceForm = null;
    showToast((editing ? '已更新:' : '已创建:') + cfg.name);
    setTimeout(refresh, 300);
  } catch (e) {
    err.value = '保存失败:' + e.message;
  }
}
</script>

<template>
  <div class="overlay" @click.self="store.serviceForm = null">
    <div class="modal">
      <h3>{{ editing ? '编辑服务:' + form.name : '新增服务' }}</h3>
      <div class="row">
        <div class="field"><label>名称 *</label><input v-model="f.name" :disabled="editing" placeholder="唯一标识" /></div>
        <div class="field"><label>显示名</label><input v-model="f.display_name" /></div>
      </div>
      <div class="field"><label>可执行文件路径 *</label><input v-model="f.command" placeholder="如 E:/rsiot-field/bin/rs-iot.exe" /></div>
      <div class="field"><label>参数(每行一个)</label><textarea v-model="f.args" rows="2"></textarea></div>
      <div class="row">
        <div class="field"><label>工作目录</label><input v-model="f.working_dir" /></div>
        <div class="field"><label>优雅停止超时(秒)</label><input v-model="f.graceful_timeout_secs" type="number" min="0" /></div>
      </div>
      <div class="row">
        <div class="field"><label>分组(不含 /)</label><input v-model="f.group" placeholder="如 core" /></div>
        <div class="field"><label>启动优先级(小者先启动)</label><input v-model="f.priority" type="number" min="0" /></div>
      </div>
      <div class="field"><label>环境变量(每行 KEY=VAL)</label><textarea v-model="f.environment" rows="2"></textarea></div>
      <div class="row">
        <div class="field"><label>输出编码(gbk;空=UTF-8)</label><input v-model="f.output_encoding" /></div>
        <div class="field"><label>管理 URL(可选)</label><input v-model="f.ui_url" /></div>
      </div>
      <div class="row">
        <div class="field"><label>健康检查 host</label><input v-model="f.health_host" placeholder="留空=不启用,如 127.0.0.1" /></div>
        <div class="field"><label>健康检查端口</label><input v-model="f.health_port" type="number" placeholder="如 8790" /></div>
      </div>
      <div class="chk">
        <label><input type="checkbox" v-model="f.auto_start" /> 自动启动</label>
        <label><input type="checkbox" v-model="f.auto_restart" /> 崩溃自动重启</label>
      </div>
      <div class="err">{{ err }}</div>
      <div class="actions">
        <button @click="store.serviceForm = null">取消</button>
        <button class="primary" @click="save">保存</button>
      </div>
    </div>
  </div>
</template>
