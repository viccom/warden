//! e2e 测试垫片:模拟 lego CLI 的最小交互面(certmgr 编排所用的子集)。
//! std-only;经 [[bin]] 构建期编译,测试以 `env!("CARGO_BIN_EXE_fake-lego")`
//! 取路径——与真实 lego 同为原生可执行,进程边界/参数/输出捕获语义与生产
//! 一致(选型论证见 PLAN-CERT-ORCHESTRATOR.md §5 步骤 3)。
//!
//! 行为约定(全由 `--path` 目录内容驱动,测试零环境变量竞态):
//! - `--version` → `lego version v0.0.0-fake <os>/<arch>`,退出 0
//! - `run ...` → 把 `<--path>/preset/{cert.pem,key.pem}` 拷到
//!   `<--path>/certificates/<首个 -d 域名(`*` 替换为 `_`)>.{crt,key}`;
//!   stdout/stderr 各输出几行(验证双路行捕获)
//! - `<--path>/SLOW` 存在 → 先睡 8s(并发 409 场景)

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!(
            "lego version v0.0.0-fake {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        return;
    }
    let path = match arg_after(&args, "--path") {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("[fake-lego] 缺 --path 参数");
            std::process::exit(2);
        }
    };
    if path.join("SLOW").exists() {
        println!("[fake-lego] SLOW marker: sleep 8s");
        std::thread::sleep(std::time::Duration::from_secs(8));
    }
    println!("[fake-lego] [domain] acme: authorizations okay");
    eprintln!("[fake-lego] [info] dns: fake challenge presented");
    // 产物命名对齐真实 lego:首个 -d 域名,通配符替换为 _
    let primary = arg_after(&args, "-d")
        .map(|d| d.replace('*', "_"))
        .unwrap_or_else(|| "example.test".to_string());
    let out_dir = path.join("certificates");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("[fake-lego] 创建目录失败:{e}");
        std::process::exit(1);
    }
    for (src, ext) in [("cert.pem", "crt"), ("key.pem", "key")] {
        let dst = out_dir.join(format!("{primary}.{ext}"));
        if let Err(e) = std::fs::copy(path.join("preset").join(src), dst) {
            eprintln!("[fake-lego] 拷贝 {src} 失败:{e}(测试须先放置 preset)");
            std::process::exit(1);
        }
    }
    println!("[fake-lego] certificate saved: certificates/{primary}.crt");
}

/// 取 flag 后紧跟的参数值(如 `--path /dir` → "/dir")。
fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
