<script setup>
// 自定义标题栏:窗口 decorations=false 后由前端自绘。
// data-tauri-drag-region 元素可拖动窗口;双击 = 最大化/还原(标准行为,全栏生效);
// 关闭按钮触发 Rust 侧 CloseRequested 处理器 → 隐藏到托盘(退出走托盘菜单)。
// 标题名称来自配置文件唯一数据源:[daemon] title → health 接口透出 → 此处展示,
// 并同步任务栏窗口标题(win.setTitle;权限缺失仅任务栏不跟随,栏内文字不受影响)。
import { ref, computed, watch, onMounted, onBeforeUnmount } from 'vue';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { store } from '../store';

const DEFAULT_TITLE = 'warden 桌面版';
const win = getCurrentWindow();
const maxed = ref(false);
let unlisten = null;

// 内嵌节点的 [daemon] title(health 轮询携带;首轮响应前用默认名)
const displayTitle = computed(
  () => store.health[store.local?.url]?.title || DEFAULT_TITLE
);
watch(
  displayTitle,
  t => { win.setTitle(t).catch(() => {}); },
  { immediate: true }
);

async function syncMax() {
  maxed.value = await win.isMaximized();
}

function onDbl(e) {
  // 双击标题栏切换最大化(标准窗口行为);窗口控制按钮不参与
  if (e.target.closest('.wbtn')) return;
  win.toggleMaximize();
}

onMounted(async () => {
  await syncMax();
  unlisten = await win.onResized(syncMax);
});
onBeforeUnmount(() => { if (unlisten) unlisten(); });
</script>

<template>
  <div class="titlebar" data-tauri-drag-region @dblclick="onDbl">
    <div class="brand" data-tauri-drag-region>
      <svg class="mark" data-tauri-drag-region viewBox="0 0 1024 1024" aria-hidden="true">
        <path class="shield" d="M512 84 L860 236 L860 496 C860 716 698 876 512 936 C326 876 164 716 164 496 L164 236 Z" />
        <polyline class="pulse" points="240,512 388,512 446,392 530,640 582,512 780,512" />
      </svg>
      <span class="tname" data-tauri-drag-region>{{ displayTitle }}</span>
    </div>
    <div class="wins">
      <button class="wbtn" title="最小化" @click="win.minimize()"><span class="ico-min"></span></button>
      <button class="wbtn" :title="maxed ? '向下还原' : '最大化'" @click="win.toggleMaximize()">
        <span v-if="maxed" class="ico-restore"></span><span v-else class="ico-max"></span>
      </button>
      <button class="wbtn close" title="关闭(最小化到托盘)" @click="win.close()">✕</button>
    </div>
  </div>
</template>

<style scoped>
.titlebar {
  height: 36px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  background: var(--bg2);
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 12px;
  font-size: 13px;
  font-weight: 600;
}
.mark { width: 18px; height: 18px; }
.mark .shield { fill: var(--accent); }
.mark .pulse {
  fill: none;
  stroke: #fff;
  stroke-width: 96;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.wins { display: flex; height: 100%; }
.wbtn {
  width: 44px;
  height: 100%;
  padding: 0;
  border: none;
  border-radius: 0;
  background: transparent;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 13px;
}
.wbtn:hover { background: var(--bg3); border-color: transparent; }
.wbtn.close:hover { background: #e81123; color: #fff; }
.ico-min { width: 10px; height: 1px; background: currentColor; }
.ico-max { width: 10px; height: 10px; border: 1px solid currentColor; }
.ico-restore, .ico-restore::before {
  width: 8px;
  height: 8px;
  border: 1px solid currentColor;
  position: absolute;
}
.ico-restore { margin: 3px 0 0 3px; }
.ico-restore::before { content: ''; left: -3.5px; top: -3.5px; }
.wbtn:has(.ico-restore) { position: relative; }
</style>
