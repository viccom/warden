//! 诊断工具:打印 crossterm 读到的按键事件(定位 TUI 方向键无响应问题)。
//!
//! 用法:`cargo run --bin keys_probe`,5 秒内按方向键/字母/其他键,
//! 每读到一个事件打印一行;5 秒后自动结束。
//! 若按键无任何输出 → crossterm 事件读取失效(环境/终端问题);
//! 若打印 KEY: code=Up/Down → 事件读取正常,问题在 TUI 应用层。

use crossterm::event::{self, Event};
use std::time::Duration;

fn main() {
    println!("keys_probe: 5 秒内按方向键/字母/任意键,打印读到的事件(Ctrl-C 提前退出)");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if event::poll(Duration::from_millis(200)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(k)) => println!("KEY: code={:?} mods={:?}", k.code, k.modifiers),
                Ok(other) => println!("EVT: {other:?}"),
                Err(e) => println!("ERR: {e}"),
            }
        }
    }
    println!("done(5 秒到)");
}
