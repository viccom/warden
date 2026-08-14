//! TUI 渲染(纯函数,每轮 draw 从 App 读状态画全屏)。
//!
//! 布局:顶栏(连接状态/过滤输入)/ 左表格 / 右面板(详情|日志)/ 底部帮助栏。
//! 选中行用背景色高亮(非 stateful 表格,避免 TableState 生命周期)。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::{api, App, Conn, Panel};

/// 状态 → 颜色(与 Web UI 一致的语义色)。
fn state_color(name: &str) -> Color {
    match name {
        "running" => Color::Green,
        "failed" => Color::Red,
        "restarting" => Color::Yellow,
        "starting" | "stopping" => Color::Blue,
        _ => Color::Gray,
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let [top, mid, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(mid);

    draw_top(f, app, top);
    draw_table(f, app, left);
    draw_right(f, app, right);
    draw_help(f, bottom);
}

fn draw_top(f: &mut Frame, app: &App, area: Rect) {
    let dot = "●";
    let (dot_color, conn_text) = match &app.conn {
        Conn::Ok => (Color::Green, "连接正常".into()),
        Conn::Error(e) => {
            // 截断错误原因,顶栏保持一行(细化:断开 + 重连语义 + 简短原因)
            let short: String = e.chars().take(40).collect();
            (Color::Red, format!("断开,重连中({short})"))
        }
    };
    let running = app
        .services()
        .iter()
        .filter(|s| s.state_name() == "running")
        .count();
    let failed = app
        .services()
        .iter()
        .filter(|s| s.state_name() == "failed")
        .count();
    let mut line = vec![
        Span::styled(
            "warden",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" v{}  ", app.version)),
        Span::styled(dot, Style::new().fg(dot_color)),
        Span::raw(format!(
            " {}  {}  running={running} failed={failed} ",
            conn_text, app.base_url
        )),
    ];
    if app.filtering() {
        line.push(Span::styled(
            format!("/{}", app.filter()),
            Style::new()
                .fg(Color::Yellow)
                .add_modifier(Modifier::REVERSED),
        ));
    } else if !app.filter().is_empty() {
        line.push(Span::styled(
            format!("/{} ", app.filter()),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(line)), area);
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    // 客户端过滤:匹配 name 或 display_name
    let flt = app.filter().to_lowercase();
    let filtered: Vec<&api::ServiceView> = app
        .services()
        .iter()
        .filter(|s| {
            flt.is_empty()
                || s.name.to_lowercase().contains(&flt)
                || s.label().to_lowercase().contains(&flt)
        })
        .collect();

    let sel_name = app.services().get(app.selected()).map(|s| s.name.as_str());
    let header = Row::new(vec![
        Cell::from("状态"),
        Cell::from("名称"),
        Cell::from("PID"),
        Cell::from("CPU%"),
        Cell::from("内存"),
        Cell::from("重启"),
    ])
    .style(Style::new().fg(Color::DarkGray));

    let rows = filtered.iter().map(|s| {
        let st = s.state_name();
        let highlighted = sel_name == Some(s.name.as_str());
        // 选中行用背景色高亮(REVERSED 在部分终端不明显,实测方向键正常但看不出选中变化)
        let style = if highlighted {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(Span::styled("●", Style::new().fg(state_color(st)))),
            Cell::from(s.label()),
            Cell::from(s.pid().map(|p| p.to_string()).unwrap_or_default()),
            Cell::from(format!("{:.1}", s.metrics.cpu_percent)),
            Cell::from(format!("{}K", s.metrics.memory_kb)),
            Cell::from(s.restart_count.to_string()),
        ])
        .style(style)
    });

    let widths = [
        Constraint::Length(7),
        Constraint::Min(10),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(5),
    ];
    let table = Table::new(rows, widths).header(header);
    f.render_widget(
        table.block(Block::default().borders(Borders::ALL).title(" 服务 ")),
        area,
    );
}

fn draw_right(f: &mut Frame, app: &App, area: Rect) {
    match app.panel() {
        Panel::Details => draw_details(f, app, area),
        Panel::Logs => draw_logs(f, app, area),
    }
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(s) = app.services().get(app.selected()) {
        lines.push(Line::from(Span::styled(
            s.label(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(format!("name        {}", s.name)));
        lines.push(Line::raw(format!(
            "state       {} {}",
            s.state_name(),
            s.pid().map(|p| format!("pid={p}")).unwrap_or_default()
        )));
        if let Some(r) = s.state_reason() {
            lines.push(Line::raw(format!("reason      {r}")));
        }
        // 健康:色点 + 状态 + 连续失败;最近错误截断
        let (hcolor, hmark) = match s.health.status.as_str() {
            "healthy" => (Color::Green, "healthy"),
            "unhealthy" => (Color::Red, "unhealthy"),
            _ => (Color::Gray, "unknown"),
        };
        lines.push(Line::from(vec![
            Span::raw("health      "),
            Span::styled(format!("●{hmark}"), Style::new().fg(hcolor)),
            Span::raw(format!("  连续失败 {}", s.health.consecutive_failures)),
        ]));
        if let Some(e) = &s.health.last_error {
            let short: String = e.chars().take(48).collect();
            lines.push(Line::raw(format!("last_error  {short}")));
        }
        // 最近一次自然退出(崩溃/正常退出;主动 stop 不记)
        if let Some(le) = &s.last_exit {
            let at = le.at.get(11..19).unwrap_or(le.at.as_str());
            lines.push(Line::raw(format!(
                "last_exit   code={} at {at}",
                le.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "None".into())
            )));
        }
        lines.push(Line::raw(format!("auto_start  {}", s.auto_start)));
        lines.push(Line::raw(format!("auto_restart {}", s.auto_restart)));
        lines.push(Line::raw(format!("restarts    {}", s.restart_count)));
        lines.push(Line::raw(format!(
            "cpu         {:.1}%",
            s.metrics.cpu_percent
        )));
        lines.push(Line::raw(format!("memory      {} KB", s.metrics.memory_kb)));
        // 环境变量(daemon 全局已烘入,service 同名覆盖)
        if !s.environment.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "environment:",
                Style::new().fg(Color::DarkGray),
            ));
            let mut keys: Vec<&String> = s.environment.keys().collect();
            keys.sort();
            for k in keys {
                lines.push(Line::raw(format!("  {}={}", k, s.environment[k])));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "  s 启动  x 停止  r 重启",
            Style::new().fg(Color::DarkGray),
        ));
    } else {
        lines.push(Line::raw("(无服务)"));
    }
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" 详情 (l 切换) "),
    );
    f.render_widget(p, area);
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let n = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for l in app.logs().skip(app.logs().count().saturating_sub(n)) {
        let ts = l.ts.get(11..19).unwrap_or(&l.ts); // ISO → HH:MM:SS
        let color = if l.stream == "stderr" {
            Color::Red
        } else {
            Color::Reset
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{ts} "), Style::new().fg(Color::DarkGray)),
            Span::styled(&l.text, Style::new().fg(color)),
        ]));
    }
    // 标题显示当前选中服务名:选择移动的直观锚点
    let sel_label = app
        .services()
        .get(app.selected())
        .map(|s| s.label())
        .unwrap_or("-");
    let mut title = format!(" 日志: {sel_label} (l 切换) ");
    if app.logs_paused() {
        title.push_str("⏸ 暂停中 ");
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    // 底部 1 行高度:绝不能带 Block 边框(边框占满 1 行,文字被挤出只剩一条线,
    // 用户实测"快捷键提示看不见")——纯文本直接渲染。
    let help =
        " ↑/↓/j/k 选择 | s 启动 x 停止 r 重启 | a 全启 z 全停 | l 详情/日志 | p 暂停 c 清空 | / 过滤 | q 退出 ";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::new().fg(Color::DarkGray),
        ))),
        area,
    );
}
