<script setup>
import { ref, onMounted } from 'vue';
import { store, refresh, tokenOf } from '../store';
import { clientFor, showToast } from '../api';

const form = store.serviceForm; // {mode, nodeUrl, name?}

const editing = form.mode === 'edit';
const f = ref(blank());
const err = ref('');
const saving = ref(false);

// 缺省与后端 RestartPolicy::default()/HealthCheck 默认对齐——空值也走 toCfg 显式回传,
// 避免未暴露字段被 serde default 静默重置(编辑丢配置的根因)。
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
    // 重启策略(崩溃自动重启开启时生效)
    max_retries: 3,
    backoff_initial_ms: 1000,
    backoff_max_ms: 60000,
    backoff_factor: 2.0,
    restart_window_secs: 60,
    // 健康检查(留空 host = 不启用)
    health_host: '',
    health_port: '',
    health_timeout_ms: 2000,
    health_interval_secs: 5,
    ui_url: '',
    config_file: '',
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
    restart: {
      max_retries: parseInt(f.value.max_retries) || 0,
      backoff_initial_ms: parseInt(f.value.backoff_initial_ms) || 1000,
      backoff_max_ms: parseInt(f.value.backoff_max_ms) || 60000,
      backoff_factor: parseFloat(f.value.backoff_factor) || 2.0,
      restart_window_secs: parseInt(f.value.restart_window_secs) || 60,
    },
    ui_url: f.value.ui_url.trim() || null,
    config_file: f.value.config_file.trim() || null,
    health:
      f.value.health_host.trim() && f.value.health_port
        ? {
            type: 'tcp',
            host: f.value.health_host.trim(),
            port: parseInt(f.value.health_port),
            timeout_ms: parseInt(f.value.health_timeout_ms) || 2000,
            interval_secs: parseInt(f.value.health_interval_secs) || 5,
          }
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
    const cfg = await clientFor({ url: form.nodeUrl, token: tokenOf(form.nodeUrl) }).config(form.name);
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
      max_retries: cfg.restart?.max_retries ?? 3,
      backoff_initial_ms: cfg.restart?.backoff_initial_ms ?? 1000,
      backoff_max_ms: cfg.restart?.backoff_max_ms ?? 60000,
      backoff_factor: cfg.restart?.backoff_factor ?? 2.0,
      restart_window_secs: cfg.restart?.restart_window_secs ?? 60,
      health_host: cfg.health?.host || '',
      health_port: cfg.health?.port || '',
      health_timeout_ms: cfg.health?.timeout_ms ?? 2000,
      health_interval_secs: cfg.health?.interval_secs ?? 5,
      ui_url: cfg.ui_url || '',
      config_file: cfg.config_file || '',
    };
  } catch (e) {
    err.value = '加载配置失败:' + e.message;
  }
});

async function save() {
  if (saving.value) return;
  err.value = '';
  saving.value = true;
  try {
    // toCfg 一并入 try:任何字段异常都浮出为可见错误,而非静默无反应
    const cfg = toCfg();
    if (!cfg.name || !cfg.command) { err.value = '名称和可执行文件路径必填'; return; }
    // toCfg 把空分组转为 null(后端 Option 语义),判斜杠前先防空
    if (cfg.group && cfg.group.includes('/')) { err.value = "分组名不能含 '/'"; return; }
    const c = clientFor({ url: form.nodeUrl, token: tokenOf(form.nodeUrl) });
    if (editing) await c.updateService(form.name, cfg);
    else await c.createService(cfg);
    store.serviceForm = null;
    showToast((editing ? '已更新:' : '已创建:') + cfg.name);
    setTimeout(refresh, 300);
  } catch (e) {
    err.value = '保存失败:' + (e.message || String(e));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="overlay" @click.self="store.serviceForm = null">
    <div class="modal props">
      <h3>{{ editing ? '编辑服务:' + form.name : '新增服务' }}</h3>

      <div class="sec">基本</div>
      <div class="row">
        <div class="field"><label>名称 *</label><input v-model="f.name" :disabled="editing" placeholder="唯一标识" /></div>
        <div class="field"><label>显示名</label><input v-model="f.display_name" /></div>
      </div>
      <div class="field"><label>描述</label><input v-model="f.description" placeholder="如 REST :8790 / lux :6390" /></div>
      <div class="field"><label>可执行文件路径 *</label><input v-model="f.command" placeholder="如 E:/rsiot-field/bin/rs-iot.exe" /></div>
      <div class="field"><label>参数(每行一个)</label><textarea v-model="f.args" rows="3"></textarea></div>
      <div class="row">
        <div class="field"><label>工作目录</label><input v-model="f.working_dir" /></div>
        <div class="field"><label>输出编码(gbk;空=UTF-8)</label><input v-model="f.output_encoding" /></div>
      </div>
      <div class="field"><label>环境变量(每行 KEY=VAL)</label><textarea v-model="f.environment" rows="3"></textarea></div>

      <div class="sec">启动</div>
      <div class="chk">
        <label><input type="checkbox" v-model="f.auto_start" /> 自动启动(daemon 启动时拉起)</label>
        <label><input type="checkbox" v-model="f.auto_restart" /> 崩溃自动重启</label>
      </div>
      <div class="row">
        <div class="field"><label>分组(不含 /)</label><input v-model="f.group" placeholder="如 core" /></div>
        <div class="field"><label>优先级(小者先启动、后停止)</label><input v-model="f.priority" type="number" min="0" /></div>
      </div>
      <div class="row">
        <div class="field"><label>优雅停止超时(秒)</label><input v-model="f.graceful_timeout_secs" type="number" min="0" /></div>
        <div class="field"><label>最大重试次数</label><input v-model="f.max_retries" type="number" min="0" :disabled="!f.auto_restart" /></div>
      </div>
      <div class="grid4" :class="{ off: !f.auto_restart }">
        <div class="field"><label>首次退避(ms)</label><input v-model="f.backoff_initial_ms" type="number" min="0" :disabled="!f.auto_restart" /></div>
        <div class="field"><label>退避上限(ms)</label><input v-model="f.backoff_max_ms" type="number" min="0" :disabled="!f.auto_restart" /></div>
        <div class="field"><label>退避乘数</label><input v-model="f.backoff_factor" type="number" step="0.1" min="1" :disabled="!f.auto_restart" /></div>
        <div class="field"><label>计数重置窗口(秒)</label><input v-model="f.restart_window_secs" type="number" min="0" :disabled="!f.auto_restart" /></div>
      </div>

      <div class="sec">健康检查(TCP)</div>
      <div class="row">
        <div class="field"><label>Host(留空=不启用)</label><input v-model="f.health_host" placeholder="如 127.0.0.1" /></div>
        <div class="field"><label>端口</label><input v-model="f.health_port" type="number" placeholder="如 8790" /></div>
      </div>
      <div class="row" :class="{ off: !f.health_host }">
        <div class="field"><label>超时(ms)</label><input v-model="f.health_timeout_ms" type="number" min="100" :disabled="!f.health_host" /></div>
        <div class="field"><label>间隔(秒)</label><input v-model="f.health_interval_secs" type="number" min="1" :disabled="!f.health_host" /></div>
      </div>

      <div class="sec">入口</div>
      <div class="field"><label>UI 入口(http 网址或程序路径;点亮卡片「打开」按钮)</label><input v-model="f.ui_url" placeholder="如 http://127.0.0.1:8790 或 E:/tools/app.exe" /></div>
      <div class="field"><label>配置文件(文本 toml/ini/yaml 等;点亮卡片「编辑」按钮)</label><input v-model="f.config_file" placeholder="如 E:/rsiot-field/config.toml" /></div>

      <div class="err">{{ err }}</div>
      <div class="actions">
        <button @click="store.serviceForm = null">取消</button>
        <button class="primary" :disabled="saving" @click="save">{{ saving ? '保存中…' : '保存' }}</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.props { width: 680px; }
.sec {
  font-size: 11px;
  color: var(--text-dim);
  letter-spacing: 1px;
  border-bottom: 1px solid var(--border);
  padding-bottom: 4px;
  margin-top: 6px;
}
.grid4 {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 10px;
}
.grid4.off,
.row.off {
  opacity: 0.55;
}
</style>
