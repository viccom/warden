//! warden 桌面版(Tauri 2)入口:单实例 + 内嵌 daemon + 托盘 + 节点注册表。

mod daemon;
mod nodes;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

/// 桌面共享状态(Tauri manage)。
struct DesktopState {
    daemon: daemon::EmbeddedDaemon,
    nodes: std::sync::Mutex<nodes::NodeRegistry>,
}

/// 前端本地节点信息(启动即调用一次)。
#[derive(Serialize)]
struct NodeInfoDto {
    url: String,
    token: String,
}

#[tauri::command]
fn local_node_info(state: tauri::State<DesktopState>) -> NodeInfoDto {
    NodeInfoDto {
        url: format!("http://127.0.0.1:{}", state.daemon.port),
        token: state.daemon.token.clone(),
    }
}

#[tauri::command]
fn nodes_list(state: tauri::State<DesktopState>) -> Vec<nodes::RemoteNode> {
    state.nodes.lock().unwrap().list().to_vec()
}

#[tauri::command]
fn nodes_add(state: tauri::State<DesktopState>, node: nodes::RemoteNode) -> Result<(), String> {
    state.nodes.lock().unwrap().add(node)
}

#[tauri::command]
fn nodes_remove(state: tauri::State<DesktopState>, url: String) -> Result<(), String> {
    state.nodes.lock().unwrap().remove(&url)
}

/// 退出:先停内嵌 daemon(逆序 stop_all + serve 收尾)再退出进程。
#[tauri::command]
async fn quit_app(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopState>,
) -> Result<(), String> {
    state.daemon.stop().await;
    app.exit(0);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例插件必须最先注册:二次启动聚焦已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            // 应用数据目录:<app_local_data>/io.warden.desktop
            let app_data = app
                .path()
                .app_local_data_dir()
                .expect("解析应用数据目录失败");
            std::fs::create_dir_all(&app_data).ok();

            let daemon = daemon::start(&app_data).expect("内嵌 daemon 启动失败");
            let nodes = nodes::NodeRegistry::load(&app_data).expect("节点注册表初始化失败");

            // 托盘:显示主窗口 / 退出
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出(停止全部子进程)", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("warden-tray")
                .icon(app.default_window_icon().expect("缺默认图标").clone())
                .tooltip("warden 桌面版")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => {
                        quit_from_tray(app);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle().clone();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            app.manage(DesktopState {
                daemon,
                nodes: std::sync::Mutex::new(nodes),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭按钮 = 最小化到托盘;真正退出走托盘菜单
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            local_node_info,
            nodes_list,
            nodes_add,
            nodes_remove,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running warden desktop");
}

/// 托盘退出:异步停 daemon 后 exit(不能阻塞菜单回调)。
fn quit_from_tray(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(state) = app.try_state::<DesktopState>() {
            state.daemon.stop().await;
        }
        app.exit(0);
    });
}
