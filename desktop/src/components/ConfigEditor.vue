<script setup>
// 配置文件编辑器:读/写节点侧 config-file API(toml/json 保存前校验,可格式化)。
import { ref, computed, onMounted } from 'vue';
import { store, tokenOf } from '../store';
import { clientFor, showToast } from '../api';

const form = store.configEditor; // {nodeUrl, name}

const path = ref('');
const format = ref('text');
const exists = ref(true);
const content = ref('');
const err = ref('');
const dirty = ref(false);
const saving = ref(false);

const client = clientFor({ url: form.nodeUrl, token: tokenOf(form.nodeUrl) });
// toml/json 由 daemon 校验+可格式化;yaml/ini/text 原样保存
const formattable = computed(() => format.value === 'toml' || format.value === 'json');

// ── 语法着色(toml/yaml/ini/json 通用的轻量正则着色器,零依赖)──────────────
// 遮罩法:高亮 <pre> 垫在透明文字的 textarea 下,滚动同步。超 200KB 关闭着色保流畅。
const HIGHLIGHT_LIMIT = 200 * 1024;

function esc(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// 分组顺序即优先级:字符串 → 注释 → 节标题 → 行首键(= 或 : 前) → 数字 → 布尔
const TOKEN_RE =
  /("(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*')|(#[^\n]*|;[^\n]*)|(^\s*\[[^\]\n]*\])|(^[ \t]*[\w.$-]+(?=[ \t]*[:=]))|(\b\d+(?:\.\d+)?\b)|(\b(?:true|false|null|yes|no|on|off)\b)/gm;

function highlight(text) {
  if (text.length > HIGHLIGHT_LIMIT) return esc(text);
  let out = '';
  let last = 0;
  for (let m; (m = TOKEN_RE.exec(text)); ) {
    out += esc(text.slice(last, m.index));
    const cls = m[1]
      ? 'tk-str'
      : m[2]
        ? 'tk-com'
        : m[3]
          ? 'tk-sec'
          : m[4]
            ? 'tk-key'
            : m[5]
              ? 'tk-num'
              : 'tk-bool';
    out += `<span class="${cls}">${esc(m[0])}</span>`;
    last = m.index + m[0].length;
  }
  return out + esc(text.slice(last));
}

const highlighted = computed(() => highlight(content.value));

// textarea 滚动同步到高亮层
const ta = ref(null);
const hl = ref(null);
function syncScroll() {
  if (hl.value && ta.value) {
    hl.value.scrollTop = ta.value.scrollTop;
    hl.value.scrollLeft = ta.value.scrollLeft;
  }
}

// Tab 键插入两个空格(配置编辑常用缩进)
function onTab(e) {
  if (e.key !== 'Tab') return;
  e.preventDefault();
  const el = e.target;
  const { selectionStart: s, selectionEnd: en } = el;
  el.value = el.value.slice(0, s) + '  ' + el.value.slice(en);
  el.selectionStart = el.selectionEnd = s + 2;
  content.value = el.value;
  dirty.value = true;
}

onMounted(async () => {
  try {
    const d = await client.configFileGet(form.name);
    path.value = d.path;
    format.value = d.format;
    exists.value = d.exists;
    content.value = d.content || '';
  } catch (e) {
    err.value = '读取失败:' + e.message;
  }
});

function close() {
  if (dirty.value && !confirm('有未保存的修改,确认关闭?')) return;
  store.configEditor = null;
}

async function save(doFormat) {
  err.value = '';
  saving.value = true;
  try {
    await client.configFilePut(form.name, content.value, doFormat);
    dirty.value = false;
    exists.value = true;
    if (doFormat) {
      // 回读格式化后的内容展示
      const d = await client.configFileGet(form.name);
      content.value = d.content || '';
    }
    showToast('已保存:' + form.name + ' 配置文件');
  } catch (e) {
    err.value = '保存失败:' + e.message;
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div class="overlay" @click.self="close()">
    <div class="modal editor">
      <div class="head">
        <h3>配置文件 · {{ form.name }}</h3>
        <span class="fmt">{{ format }}</span>
      </div>
      <div class="path" :title="path">{{ path }}</div>
      <div v-if="!exists" class="warn">文件尚不存在,保存后将创建</div>
      <div class="editor-wrap">
        <pre ref="hl" class="code hl" aria-hidden="true" v-html="highlighted"></pre>
        <textarea
          ref="ta"
          v-model="content"
          @input="dirty = true"
          @scroll="syncScroll"
          @keydown="onTab"
          spellcheck="false"
          class="code ta"
        ></textarea>
      </div>
      <div class="err">{{ err }}</div>
      <div class="actions">
        <label class="hint" v-if="!formattable">该格式不做语法校验,按纯文本保存</label>
        <button @click="close()">关闭</button>
        <button class="primary" :disabled="saving" @click="save(false)">保存</button>
        <button v-if="formattable" :disabled="saving" @click="save(true)">保存并格式化</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.editor { width: 760px; }
.head { display: flex; align-items: center; gap: 10px; }
.fmt {
  font-size: 11px;
  background: var(--bg3);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 1px 8px;
  color: var(--text-dim);
  text-transform: uppercase;
}
.path {
  font-size: 12px;
  color: var(--text-dim);
  font-family: Consolas, monospace;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  direction: rtl; /* 长路径显示尾部(文件名) */
  text-align: left;
}
.warn { font-size: 12px; color: var(--yellow); }
/* 遮罩法:高亮层与输入层同字体同度量;输入层文字透明只留光标 */
.editor-wrap { position: relative; }
.code {
  height: 52vh;
  width: 100%;
  margin: 0;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: 6px;
  font-family: Consolas, "Cascadia Mono", monospace;
  font-size: 13px;
  line-height: 1.5;
  white-space: pre;
  overflow: auto;
  tab-size: 2;
  background: var(--bg);
}
.code.hl {
  position: absolute;
  inset: 0;
  pointer-events: none;
  border-color: transparent;
  user-select: none;
  overflow: hidden; /* 不出滚动条(宽度与输入层一致),滚动由 syncScroll 驱动 */
}
.code.ta {
  position: relative;
  resize: vertical;
  color: transparent;
  caret-color: var(--text);
  background: transparent;
  user-select: text;
}
/* v-html 注入的 span 不带 scoped 属性,必须 :deep() 穿透(否则着色全丢) */
.code.hl :deep(.tk-com) { color: #6b7385; font-style: italic; }
.code.hl :deep(.tk-str) { color: #8fd18f; }
.code.hl :deep(.tk-key) { color: #7fb3ff; }
.code.hl :deep(.tk-sec) { color: #e8b339; font-weight: 600; }
.code.hl :deep(.tk-num) { color: #d9a0e0; }
.code.hl :deep(.tk-bool) { color: #e0917f; }
.actions { justify-content: flex-end; }
.hint { margin-right: auto; font-size: 12px; color: var(--text-dim); }
</style>
