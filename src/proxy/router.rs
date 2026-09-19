//! Host 路由匹配核心(纯函数,无 IO,单测覆盖所有分支)。
//!
//! 设计见 PLAN-REVERSE-PROXY.md §4.3。匹配顺序:
//! 1. 精确 host 命中 → 该路由
//! 2. 通配 host(单层)命中 → 最长后缀胜出
//! 3. auto:<sub>.<domain> 且 sub 命中某 proxy=true 服务的 subdomain → AutoService
//! 4. 全不中 → NotFound(调用方返回 421)

use std::sync::Arc;

use crate::config::ProxyConfig;
use crate::proxy::SharedProxyConfig;
use crate::supervisor::Supervisor;

/// 路由解析结果。
pub enum Decision {
    /// 显式路由命中(上游为直接 URI)。
    Route { to: String, preserve_host: bool },
    /// 显式路由命中(上游引用被监护服务,转发层经 snapshot 解析 ui_url)。
    RouteService {
        service: String,
        preserve_host: bool,
    },
    /// auto 路由命中(服务名;上游 = 该服务 ui_url,状态非 Running 由转发层 503)。
    AutoService { name: String },
    /// 全不中(调用方返回 421)。
    NotFound,
}

/// Host 路由器:显式路由表(共享配置,热更新)+ auto 分支(经 Supervisor 实时查询)。
pub struct HostRouter {
    cfg: SharedProxyConfig,
    supervisor: Arc<Supervisor>,
}

impl HostRouter {
    pub fn new(cfg: SharedProxyConfig, supervisor: Arc<Supervisor>) -> Self {
        Self { cfg, supervisor }
    }

    /// 当前配置快照(读锁拷 Arc,每请求一次,开销可忽略)。
    fn cfg(&self) -> Arc<ProxyConfig> {
        self.cfg.read().expect("proxy 配置锁中毒").clone()
    }

    /// 规范化 host:转小写、剥离 `:port`。
    pub fn normalize_host(host: &str) -> String {
        let host = host.to_lowercase();
        match host.rfind(':') {
            Some(i) => host[..i].to_string(),
            None => host,
        }
    }

    /// 解析路由决策(host 须已 normalize;未 normalize 会被先规范化)。
    pub fn resolve(&self, host: &str) -> Decision {
        let cfg = self.cfg();
        let host = Self::normalize_host(host);
        match Self::resolve_explicit(&cfg, &host) {
            Decision::NotFound => Self::resolve_auto(
                &host,
                cfg.domain.as_deref().unwrap_or(""),
                &self.exposed_services(),
            )
            .map(|name| Decision::AutoService { name })
            .unwrap_or(Decision::NotFound),
            d => d,
        }
    }

    /// 全局 preserve_host(auto 路由无路由级覆盖,取全局配置)。
    pub fn global_preserve_host(&self) -> bool {
        self.cfg().preserve_host
    }

    /// supervisor 引用(转发层解析服务引用路由的 ui_url/状态)。
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// supervisor 快照中 proxy=true 服务的 (label, name) 表;label = subdomain
    /// (小写已由配置规范化保证)缺省 name 的小写。
    fn exposed_services(&self) -> Vec<(String, String)> {
        self.supervisor
            .list()
            .into_iter()
            .filter(|s| s.proxy)
            .map(|s| {
                let label = s.subdomain.clone().unwrap_or_else(|| s.name.to_lowercase());
                (label.to_lowercase(), s.name)
            })
            .collect()
    }

    /// 显式路由匹配(纯函数):精确优先,通配次之(单层,最长后缀)。
    pub fn resolve_explicit(cfg: &ProxyConfig, host: &str) -> Decision {
        let preserve = |r: &crate::config::ProxyRoute| r.preserve_host.unwrap_or(cfg.preserve_host);
        // 1. 精确 host(线性扫;路由表为配置级小表)
        for r in &cfg.routes {
            if r.host == host {
                return match (&r.to, &r.service) {
                    (Some(to), None) => Decision::Route {
                        to: to.clone(),
                        preserve_host: preserve(r),
                    },
                    (None, Some(service)) => Decision::RouteService {
                        service: service.clone(),
                        preserve_host: preserve(r),
                    },
                    // to/service 二选一由配置校验保证;双双缺省/同时配置的坏项
                    // warn 但保留(校验哲学),此处兜底为 NotFound 不致误转发
                    _ => Decision::NotFound,
                };
            }
        }
        // 2. 通配 `*.<suffix>`:单层(剥后缀剩余部分不含 '.')、最长后缀胜出
        let mut best: Option<(&crate::config::ProxyRoute, &str)> = None;
        for r in &cfg.routes {
            let Some(suffix) = r.host.strip_prefix("*.") else {
                continue;
            };
            let Some(sub) = host.strip_suffix(suffix) else {
                continue;
            };
            // strip_suffix 后须为 `<sub>.` 形态:剥去尾点,剩余非空且单层
            let Some(label) = sub.strip_suffix('.') else {
                continue;
            };
            if label.is_empty() || label.contains('.') {
                continue;
            }
            if best.map_or(true, |(b, _)| r.host.len() > b.host.len()) {
                best = Some((r, label));
            }
        }
        match best {
            Some((r, _)) => match (&r.to, &r.service) {
                (Some(to), None) => Decision::Route {
                    to: to.clone(),
                    preserve_host: preserve(r),
                },
                (None, Some(service)) => Decision::RouteService {
                    service: service.clone(),
                    preserve_host: preserve(r),
                },
                _ => Decision::NotFound,
            },
            None => Decision::NotFound,
        }
    }

    /// auto 路由匹配(纯函数):host 须形如 `<sub>.<domain>` 且 sub 命中
    /// exposed 表中的 label → 服务名;sub 含多层(点)不中。
    pub fn resolve_auto(host: &str, domain: &str, exposed: &[(String, String)]) -> Option<String> {
        let suffix = format!(".{domain}");
        let sub = host.strip_suffix(&suffix)?;
        if sub.is_empty() || sub.contains('.') {
            return None;
        }
        exposed
            .iter()
            .find(|(label, _)| label == sub)
            .map(|(_, name)| name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyRoute;

    fn router(domain: &str, routes: &[(&str, &str)]) -> HostRouter {
        let cfg = ProxyConfig {
            domain: Some(domain.into()),
            http_bind: None,
            https_bind: None,
            connect_timeout_ms: 5000,
            preserve_host: false,
            cert_file: None,
            key_file: None,
            upstream_ca_file: None,
            acme: Default::default(),
            routes: routes
                .iter()
                .map(|(h, t)| ProxyRoute {
                    host: (*h).into(),
                    to: Some((*t).into()),
                    service: None,
                    preserve_host: None,
                })
                .collect(),
        };
        HostRouter::new(
            crate::proxy::shared_from(cfg),
            Arc::new(Supervisor::new(std::path::PathBuf::from(""))),
        )
    }

    #[test]
    fn normalize_strips_port_and_lowercases() {
        assert_eq!(HostRouter::normalize_host("A.X.com:8443"), "a.x.com");
        assert_eq!(HostRouter::normalize_host("Plain.Host"), "plain.host");
    }

    #[test]
    fn exact_host_wins() {
        let r = router("x.com", &[("a.x.com", "http://1")]);
        assert!(matches!(r.resolve("a.x.com"), Decision::Route { .. }));
    }

    #[test]
    fn wildcard_single_label_matches() {
        let r = router("x.com", &[("*.x.com", "http://1")]);
        assert!(matches!(r.resolve("a.x.com"), Decision::Route { .. }));
    }

    #[test]
    fn wildcard_rejects_multilevel() {
        let r = router("x.com", &[("*.x.com", "http://1")]);
        assert!(
            matches!(r.resolve("a.b.x.com"), Decision::NotFound),
            "通配只匹配单层,a.b.x.com 不应命中 *.x.com"
        );
    }

    #[test]
    fn exact_overrides_wildcard() {
        let r = router(
            "x.com",
            &[("*.x.com", "http://wild"), ("a.x.com", "http://exact")],
        );
        match r.resolve("a.x.com") {
            Decision::Route { to, .. } => assert_eq!(to, "http://exact"),
            _ => panic!("精确应胜出"),
        }
    }

    #[test]
    fn longest_wildcard_suffix_wins() {
        let r = router(
            "x.com",
            &[("*.x.com", "http://short"), ("*.sub.x.com", "http://long")],
        );
        match r.resolve("a.sub.x.com") {
            Decision::Route { to, .. } => assert_eq!(to, "http://long"),
            _ => panic!("最长后缀应胜出"),
        }
    }

    #[test]
    fn unknown_host_returns_not_found() {
        let r = router("x.com", &[("a.x.com", "http://1")]);
        assert!(matches!(r.resolve("b.x.com"), Decision::NotFound));
    }

    #[test]
    fn service_route_returns_route_service() {
        let cfg = ProxyConfig {
            domain: Some("x.com".into()),
            http_bind: None,
            https_bind: None,
            connect_timeout_ms: 5000,
            preserve_host: false,
            cert_file: None,
            key_file: None,
            upstream_ca_file: None,
            acme: Default::default(),
            routes: vec![ProxyRoute {
                host: "a.x.com".into(),
                to: None,
                service: Some("rs-iot".into()),
                preserve_host: None,
            }],
        };
        match HostRouter::resolve_explicit(&cfg, "a.x.com") {
            Decision::RouteService { service, .. } => assert_eq!(service, "rs-iot"),
            _ => panic!("service 引用路由应返回 RouteService"),
        }
    }

    /// 意图:auto 分支纯函数——<sub>.<domain> 单层标签命中 exposed 表 → 服务名。
    #[test]
    fn auto_branch_matches_subdomain_label() {
        let exposed = vec![("fs".to_string(), "fs-svc".to_string())];
        assert_eq!(
            HostRouter::resolve_auto("fs.x.com", "x.com", &exposed),
            Some("fs-svc".to_string())
        );
        // 多层 sub 不中
        assert_eq!(
            HostRouter::resolve_auto("a.fs.x.com", "x.com", &exposed),
            None
        );
        // 标签未列出(proxy=false 的服务不进表)不中
        assert_eq!(
            HostRouter::resolve_auto("other.x.com", "x.com", &exposed),
            None
        );
        // 完全不同的域不中
        assert_eq!(
            HostRouter::resolve_auto("fs.y.com", "x.com", &exposed),
            None
        );
    }

    /// 意图:auto 分支在 supervisor 无已暴露服务时整体不中(空表 → NotFound)。
    #[test]
    fn auto_ignores_service_not_exposed() {
        let r = router("x.com", &[]);
        assert!(matches!(r.resolve("anything.x.com"), Decision::NotFound));
    }
}
