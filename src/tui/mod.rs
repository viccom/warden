//! TUI 终端客户端(Phase 3):连 warden HTTP API 的服务表格 + 实时日志。
//!
//! 事件循环:crossterm poll(100ms)处理按键 → 后台 tokio 任务经 unbounded mpsc
//! 推送服务列表刷新 / SSE 日志行 → 每轮 draw 渲染。SSE 断线由 eventsource
//! 自动重连,连接状态在顶栏指示。

pub mod api;
pub mod ui;

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::tui::api::ApiClient;

/// stdin 是否为 Windows 控制台(GetConsoleMode 成功)。
#[cfg(windows)]
fn console_input_available() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};
    let mut mode: u32 = 0;
    unsafe { GetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), &mut mode) != 0 }
}

#[cfg(not(windows))]
fn console_input_available() -> bool {
    true
}

/// 后台任务 → App 的消息。
enum Msg {
    Services(Result<Vec<api::ServiceView>, String>),
    Log(Result<api::LogLineView, String>),
}

/// 连接状态(顶栏指示)。
#[derive(Clone, PartialEq)]
pub enum Conn {
    Ok,
    Error(String),
}

/// 右侧面板。
#[derive(Clone, Copy, PartialEq)]
pub enum Panel {
    Details,
    Logs,
}

const LOG_CAPACITY: usize = 1000;

/// TUI 应用状态。
pub struct App {
    client: ApiClient,
    pub base_url: String,
    services: Vec<api::ServiceView>,
    selected: usize,
    filter: String,
    filtering: bool,
    panel: Panel,
    logs: VecDeque<api::LogLineView>,
    logs_paused: bool,
    pub conn: Conn,
    pub version: String,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    sse_task: Option<tokio::task::JoinHandle<()>>,
    err: Option<String>,
}

/// 启动 TUI(init/restore 由调用方做)。
pub async fn run(client: ApiClient, base_url: String) -> anyhow::Result<()> {
    // 键盘输入依赖 Windows 控制台(crossterm ReadConsoleInputW)。Git Bash/mintty
    // 的 stdin 是管道,界面能渲染但收不到按键(经典症状:方向键无响应)——提前警告。
    if !console_input_available() {
        eprintln!(
            "警告:当前终端不支持键盘输入(非 Windows 控制台,如 Git Bash/mintty)。\n\
             请在 Windows Terminal 或 PowerShell 中运行 `warden tui`。"
        );
        return Ok(());
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let tx_list = tx.clone();
    let client_list = client.clone();
    let mut app = App {
        client: client.clone(),
        base_url: base_url.clone(),
        services: Vec::new(),
        selected: 0,
        filter: String::new(),
        filtering: false,
        panel: Panel::Logs,
        logs: VecDeque::with_capacity(LOG_CAPACITY),
        logs_paused: false,
        conn: Conn::Ok,
        version: String::new(),
        tx,
        rx,
        sse_task: None,
        err: None,
    };

    // 后台:每 1s 刷新服务列表(错误时置连接状态)
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let r = client_list.list().await.map_err(|e| e.to_string());
            if tx_list.send(Msg::Services(r)).is_err() {
                break;
            }
        }
    });

    // 启动时拉一次 health(版本号)
    if let Ok(h) = client.health().await {
        app.version = h.version;
    }

    ratatui::init();
    // 启用键盘事件类型(Windows Terminal 支持):按住方向键的系统重复事件会带
    // KeyEventKind::Repeat,handle_key 忽略之(conhost 不支持则 Push 失败忽略,
    // 由 move_sel 的 clamp 兜底,不会在 0↔1 抖动)。
    let _ = crossterm::event::PushKeyboardEnhancementFlags(
        crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
    );
    let result = app.event_loop().await;
    ratatui::restore();
    result
}

impl App {
    async fn event_loop(&mut self) -> anyhow::Result<()> {
        use ratatui::Terminal;
        let mut terminal =
            Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
        loop {
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    if self.handle_key(k) {
                        break; // 退出
                    }
                }
            }
            self.drain_messages();
            // 过滤时表格选中保持;列表为空时 selected 归零
            if self.services.is_empty() {
                self.selected = 0;
            } else if self.selected >= self.services.len() {
                self.selected = self.services.len() - 1;
            }
            terminal.draw(|f| ui::draw(f, self))?;
        }
        Ok(())
    }

    /// 处理按键。返回 true 表示退出。
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // 忽略系统键盘重复事件(按住方向键不抖动;无 enhancement 支持的终端
        // 事件恒为 Press,由 move_sel 的 clamp 兜底)
        if key.kind == crossterm::event::KeyEventKind::Repeat {
            return false;
        }
        // 过滤输入模式:捕获字符键
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('s') => self.act_selected("start"),
            KeyCode::Char('x') => self.act_selected("stop"),
            KeyCode::Char('r') => self.act_selected("restart"),
            KeyCode::Char('a') => self.act_selected("start_all"),
            KeyCode::Char('z') => self.act_selected("stop_all"),
            KeyCode::Char('l') | KeyCode::Tab => {
                self.panel = match self.panel {
                    Panel::Details => Panel::Logs,
                    Panel::Logs => Panel::Details,
                }
            }
            KeyCode::Char('p') => self.logs_paused = !self.logs_paused,
            KeyCode::Char('c') => self.logs.clear(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc => {}
            _ => {}
        }
        false
    }

    fn move_sel(&mut self, delta: isize) {
        if self.services.is_empty() {
            return;
        }
        // 边界 clamp(不环绕):按住方向键时 OS 键盘重复会产生连续事件,
        // 环绕会让 2 个服务的列表在 0↔1 抖动(实测"一松手就还原"现象)。
        let n = self.services.len() as isize;
        self.selected = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.on_select_changed();
    }

    /// 选中变化:重建 SSE 日志任务 + 拉历史快照。
    fn on_select_changed(&mut self) {
        let name = self.selected_name().map(str::to_string);
        if let Some(n) = name {
            self.switch_logs(&n);
        }
    }

    /// 拉历史 + 起 SSE。
    fn switch_logs(&mut self, name: &str) {
        self.logs.clear();
        self.logs_paused = false;
        let client = self.client.clone();
        let tx = self.tx.clone();
        // 历史快照
        let name_hist = name.to_string();
        tokio::spawn(async move {
            match client.logs_tail(&name_hist, 100).await {
                Ok(lines) => {
                    for l in lines {
                        if tx.send(Msg::Log(Ok(l))).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Log(Err(format!("历史日志拉取失败:{e}"))));
                }
            }
        });
        // SSE 实时流(替换旧任务;eventsource 断线自动重连)
        if let Some(t) = self.sse_task.take() {
            t.abort();
        }
        let mut es = self.client.logs_stream(name);
        let tx2 = self.tx.clone();
        self.sse_task = Some(tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(item) = es.next().await {
                match item {
                    Ok(reqwest_eventsource::Event::Message(msg)) if msg.event == "log" => {
                        match serde_json::from_str::<api::LogLineView>(&msg.data) {
                            Ok(line) => {
                                if tx2.send(Msg::Log(Ok(line))).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if tx2
                                    .send(Msg::Log(Err(format!("日志解析失败:{e}"))))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(reqwest_eventsource::Event::Open) => {}
                    Err(e) if tx2.send(Msg::Log(Err(format!("日志流断线:{e}")))).is_err() => {
                        break;
                    }
                    Err(_) => {
                        // 断线:已提示,继续等重连(eventsource 内部重试)
                    }
                    _ => {}
                }
            }
        }));
    }

    fn act_selected(&mut self, act: &str) {
        let name = self.selected_name().map(String::from);
        let act = act.to_string();
        if name.is_none() && act != "start_all" && act != "stop_all" {
            return;
        }
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = match act.as_str() {
                "start_all" => client.start_all().await,
                "stop_all" => client.stop_all().await,
                _ => match name.as_deref() {
                    Some(n) => client.action(n, &act).await,
                    None => return,
                },
            };
            // 结果仅用于错误提示;成功等 1s 定时刷新即可
            if let Err(e) = r {
                let _ = tx.send(Msg::Log(Err(format!("操作 {act} 失败:{e}"))));
            }
        });
    }

    fn selected_name(&self) -> Option<&str> {
        self.services.get(self.selected).map(|s| s.name.as_str())
    }

    /// 处理后台消息。
    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Services(r) => match r {
                    Ok(list) => {
                        self.services = list;
                        self.conn = Conn::Ok;
                    }
                    Err(e) => self.conn = Conn::Error(e),
                },
                Msg::Log(r) => match r {
                    Ok(line) => {
                        if self.logs.len() >= LOG_CAPACITY {
                            self.logs.pop_front();
                        }
                        self.logs.push_back(line);
                    }
                    Err(e) => self.err = Some(e),
                },
            }
        }
    }

    // ── 供 ui 读取的访问器 ──
    pub fn services(&self) -> &[api::ServiceView] {
        &self.services
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn filtering(&self) -> bool {
        self.filtering
    }

    pub fn panel(&self) -> Panel {
        self.panel
    }

    pub fn logs(&self) -> impl Iterator<Item = &api::LogLineView> {
        self.logs.iter()
    }

    pub fn logs_paused(&self) -> bool {
        self.logs_paused
    }

    pub fn err(&self) -> Option<&str> {
        self.err.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn svc(name: &str) -> api::ServiceView {
        api::ServiceView {
            name: name.into(),
            display_name: String::new(),
            state: serde_json::json!({"state": "running", "pid": 1}),
            restart_count: 0,
            metrics: api::MetricsView::default(),
            health: api::ServiceHealthView::default(),
            last_exit: None,
            auto_start: false,
            auto_restart: false,
            group: None,
            priority: 0,
            listening_ports: Vec::new(),
            environment: Default::default(),
        }
    }

    fn app_with(selected: usize) -> App {
        let (tx, rx) = mpsc::unbounded_channel();
        App {
            client: ApiClient::new("http://127.0.0.1:1".into(), None).unwrap(),
            base_url: "http://127.0.0.1:1".into(),
            services: vec![svc("gbk-decoded"), svc("gbk-raw")],
            selected,
            filter: String::new(),
            filtering: false,
            panel: Panel::Logs,
            logs: VecDeque::new(),
            logs_paused: false,
            conn: Conn::Ok,
            version: String::new(),
            tx,
            rx,
            sse_task: None,
            err: None,
        }
    }

    /// 验证意图:selected 变化时,高亮必须跟随(用户实测"始终选第一条"的回归测试)。
    /// 渲染到 TestBackend,断言 selected=1 时唯一高亮行是第二个服务所在行。
    #[test]
    fn selected_highlight_follows_selected_index() {
        let app = app_with(1);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let width = buf.area().width as usize;
        let mut hl_rows: Vec<u16> = buf
            .content()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.style().bg == Some(Color::Cyan))
            .map(|(i, _)| (i / width) as u16)
            .collect();
        hl_rows.sort_unstable();
        hl_rows.dedup();
        assert_eq!(hl_rows.len(), 1, "应恰好一行高亮,实际 {} 行", hl_rows.len());
        let hl_row = hl_rows[0];
        let row_text: String = buf
            .content()
            .iter()
            .enumerate()
            .filter(|(i, _)| (i / width) as u16 == hl_row)
            .map(|(_, c)| c.symbol())
            .collect();
        assert!(
            row_text.contains("gbk-raw"),
            "selected=1 时高亮应落在第二行(gbk-raw),实际高亮行文本:{row_text:?}"
        );
    }

    /// 验证意图:方向键 Down 从 0 → 1(环绕逻辑)。
    #[tokio::test]
    async fn down_key_moves_selection() {
        let press = |code| KeyEvent {
            code,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        let mut app = app_with(0);
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected, 1);
        // 边界 clamp:继续 Down 停在最后一条(按住 key repeat 不抖动——"释放还原"回归)
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.selected, 1, "边界 clamp:继续 Down 应停在最后一条");
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.selected, 0);
        app.handle_key(press(KeyCode::Up));
        assert_eq!(app.selected, 0, "边界 clamp:继续 Up 应停在第一条");
        // 系统重复事件(Repeat kind)应被忽略
        app.handle_key(KeyEvent {
            code: KeyCode::Down,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Repeat,
            state: crossterm::event::KeyEventState::empty(),
        });
        assert_eq!(app.selected, 0, "Repeat 事件应被忽略");
    }
}
