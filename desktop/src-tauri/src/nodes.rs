//! 远程节点注册表:持久化到应用数据目录 nodes.json。
//!
//! 本地内嵌节点不落此文件(启动时注入,不可删);此处只管用户添加的
//! 其他 warden 节点(本机 CLI 版 / 网络远程)。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一个远程 warden 节点。
#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteNode {
    /// 显示名(如「现场主机 A」)。
    pub name: String,
    /// daemon HTTP API 地址(如 http://192.168.1.100:8789)。
    pub url: String,
    /// 鉴权 token(可空 = 目标未配鉴权)。
    #[serde(default)]
    pub token: String,
}

/// 节点注册表(磁盘文件 + 内存镜像;url 为唯一键)。
pub struct NodeRegistry {
    path: PathBuf,
    nodes: Vec<RemoteNode>,
}

#[derive(Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    nodes: Vec<RemoteNode>,
}

impl NodeRegistry {
    /// 加载(文件不存在/损坏则空表起步,损坏仅 warn 不致命)。
    pub fn load(app_data: &Path) -> Result<Self, String> {
        let path = app_data.join("nodes.json");
        let nodes = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<RegistryFile>(&s)
                .map(|f| f.nodes)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Ok(Self { path, nodes })
    }

    pub fn list(&self) -> &[RemoteNode] {
        &self.nodes
    }

    /// 添加/覆盖(url 同键更新)。校验:url 非空、去空白。
    pub fn add(&mut self, node: RemoteNode) -> Result<(), String> {
        let url = node.url.trim().to_string();
        if url.is_empty() {
            return Err("url 不能为空".into());
        }
        let node = RemoteNode {
            name: node.name.trim().to_string(),
            url,
            token: node.token.trim().to_string(),
        };
        self.nodes.retain(|n| n.url != node.url);
        self.nodes.push(node);
        self.persist()
    }

    pub fn remove(&mut self, url: &str) -> Result<(), String> {
        self.nodes.retain(|n| n.url != url);
        self.persist()
    }

    fn persist(&self) -> Result<(), String> {
        let s = serde_json::to_string_pretty(&RegistryFile {
            nodes: self.nodes.clone(),
        })
        .map_err(|e| e.to_string())?;
        std::fs::write(&self.path, s).map_err(|e| format!("写 {} 失败:{e}", self.path.display()))
    }
}
