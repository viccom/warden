//! 测试辅助程序(由 warden 作为被监护进程启动),向 stdout 输出 **GBK 编码**的中文行。
//!
//! 用于手动验证 warden 的 `output_encoding` 解码:
//! - warden 服务设 `output_encoding = "gbk"` → 中文正确显示
//! - 不设(默认 UTF-8)→ 中文出现替换字符/乱码(对照,证明问题与修复)
//!
//! 直接写字节到 stdout(绕过 `println!` 的 UTF-8 转换),模拟中文 Windows 控制台程序
//! (如 rs-iot)的 GBK/CP936 输出。无限循环,由 warden stop 超时后强杀。

use std::io::Write;
use std::time::Duration;

fn main() {
    // 每轮输出:含纯 ASCII 行(两种编码都正常)+ 中文行(仅解码组正常),便于肉眼对比。
    let lines = [
        "gbk_target ready",
        "启动完成:温度传感器已连接",
        "采集数据:温度 36.5C 湿度 65%",
        "警告:数据写入 lux 数据库",
    ];

    let mut out = std::io::stdout();
    loop {
        for line in &lines {
            // 编码为 GBK 字节后直接写出(非法字符以 ? 替换,此处全可表示)
            let (bytes, _, _) = encoding_rs::GBK.encode(line);
            let _ = out.write_all(&bytes);
            let _ = out.write_all(b"\r\n"); // CRLF,模拟 Windows 控制台行尾
            let _ = out.flush();
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
