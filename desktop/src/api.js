// 每节点 HTTP 客户端:统一覆盖本地内嵌节点与远程节点(CLI 版 warden API)。

export function clientFor(node) {
  const base = String(node.url || '').replace(/\/+$/, '');
  const auth = node.token ? { Authorization: 'Bearer ' + node.token } : {};
  async function req(path, method = 'GET', body) {
    const r = await fetch(base + path, {
      method,
      headers: {
        ...auth,
        ...(body ? { 'Content-Type': 'application/json' } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!r.ok) {
      // 优先取响应体里的 message(如 toml 解析失败的详细位置),否则退回状态码
      let detail = r.status === 401 ? '(token 不对或未配?)' : '';
      try {
        const b = await r.json();
        if (b && b.message) detail = b.message;
      } catch { /* 非 JSON 响应 */ }
      throw new Error(`HTTP ${r.status} ${detail}`.trim());
    }
    return r.json();
  }
  const enc = encodeURIComponent;
  return {
    node,
    base,
    health: () => req('/api/v1/health'),
    services: () => req('/api/v1/services').then(d => d.services || []),
    action: (name, act) => req(`/api/v1/services/${enc(name)}/${act}`, 'POST'),
    groupAction: (group, act) => req(`/api/v1/groups/${enc(group)}/${act}`, 'POST'),
    startAll: () => req('/api/v1/services/start-all', 'POST'),
    stopAll: () => req('/api/v1/services/stop-all', 'POST'),
    logs: (name, tail = 300) =>
      req(`/api/v1/services/${enc(name)}/logs?tail=${tail}`).then(d => d.lines || []),
    config: name => req(`/api/v1/services/${enc(name)}/config`),
    configFileGet: name => req(`/api/v1/services/${enc(name)}/config-file`),
    configFilePut: (name, content, format = false) =>
      req(`/api/v1/services/${enc(name)}/config-file`, 'PUT', { content, format }),
    createService: cfg => req('/api/v1/services', 'POST', cfg),
    updateService: (name, cfg) => req(`/api/v1/services/${enc(name)}`, 'PUT', cfg),
    deleteService: name => req(`/api/v1/services/${enc(name)}`, 'DELETE'),
  };
}

export function showToast(msg, isErr = false) {
  let el = document.getElementById('toast');
  if (!el) {
    el = document.createElement('div');
    el.id = 'toast';
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.className = 'show' + (isErr ? ' err' : '');
  clearTimeout(el._t);
  el._t = setTimeout(() => (el.className = ''), 2200);
}
