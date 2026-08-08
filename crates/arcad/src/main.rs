//! # arcad
//!
//! 服务端 daemon——全系统唯一常驻进程（spec §3.1，形态参考 git：
//! 服务端有守护进程，客户端零常驻）。单二进制部署到 ARM NAS，
//! 内存占用平稳可预测（§1.1 目标 9）。
//!
//! 模块：HTTP API（RFC 9110 条件请求）· 库存储（多卷映射）· journal ·
//! 上传会话 · 认证 · GC · Git LFS 桥（可选启用）。
//! 对账与提交决策全部来自 arca-core（两端共用，不在此重写）。
//!
//! # 命令行（M2b Task 3）
//!
//! ```text
//! arcad [--config <hub.toml 路径，默认 ./hub.toml>] [--bind <地址:端口，默认 127.0.0.1:8420>]
//! arcad --check [--config <hub.toml 路径>]
//! ```
//!
//! `--check`：只对配置里的每个数据集做一次挂载检查并把结果打到 stdout，
//! **不起任何服务**——运维排障用（brief 原文）。任一数据集离线时进程以
//! 非零状态退出，供脚本/监控直接判断，但**检查本身不会因为某一个根离线
//! 就提前终止**：全部数据集都会被检查一遍再退出（spec §4.3.2 独立故障域）。
//!
//! 正常启动时同样先做一遍挂载检查（只落一条日志，不影响启动结果）——
//! **某个存储根打不开不等于启动失败**：该数据集的请求会在每次收到时被
//! 重新判定为 503（见 `storage.rs`/`api.rs`），其余数据集照常服务。

#![forbid(unsafe_code)]

mod api;
mod auth;
mod config;
mod gc;
mod journal_store;
mod lfs_bridge;
mod storage;
mod uploads;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const DEFAULT_CONFIG: &str = "hub.toml";
const DEFAULT_BIND: &str = "127.0.0.1:8420";

struct Args {
    config: PathBuf,
    bind: String,
    check_only: bool,
}

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> Result<Args, String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut bind = DEFAULT_BIND.to_string();
    let mut check_only = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--check" => check_only = true,
            "--config" => {
                config = PathBuf::from(it.next().ok_or("--config 缺少参数值")?);
            }
            "--bind" => {
                bind = it.next().ok_or("--bind 缺少参数值")?;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(Args {
        config,
        bind,
        check_only,
    })
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误：{e}");
            return ExitCode::from(2);
        }
    };

    let hub_config = match config::HubConfig::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("加载配置 {} 失败：{e}", args.config.display());
            return ExitCode::FAILURE;
        }
    };
    let registry = storage::Registry::from_config(&hub_config);

    // 无论 --check 还是正常启动，第一步都是同一份挂载检查——见模块文档。
    let results = storage::check_all(&registry);
    let mut any_offline = false;
    for r in &results {
        match &r.outcome {
            Ok(()) => eprintln!(
                "[arcad] 数据集 {} 已就绪（{}）",
                r.dataset_id,
                r.path.display()
            ),
            Err(e) => {
                any_offline = true;
                eprintln!(
                    "[arcad] 数据集 {} 当前离线（{}）：{e}",
                    r.dataset_id,
                    r.path.display()
                );
            }
        }
    }

    if args.check_only {
        return if any_offline {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }

    let state = Arc::new(registry);
    let app = api::router(state);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("创建 tokio 运行时失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    let bind = args.bind.clone();
    let result = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        eprintln!("[arcad] 监听 {bind}");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[arcad] HTTP 服务异常退出：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 优雅关闭：收到 Ctrl-C（`SIGINT`）即让 `axum::serve` 完成在途请求后退出。
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认参数() {
        let args = parse_args(std::iter::empty()).unwrap();
        assert_eq!(args.config, PathBuf::from(DEFAULT_CONFIG));
        assert_eq!(args.bind, DEFAULT_BIND);
        assert!(!args.check_only);
    }

    #[test]
    fn 解析_check_与自定义配置路径() {
        let args = parse_args(
            ["--check", "--config", "/tmp/hub.toml"]
                .into_iter()
                .map(String::from),
        )
        .unwrap();
        assert!(args.check_only);
        assert_eq!(args.config, PathBuf::from("/tmp/hub.toml"));
    }

    #[test]
    fn 解析自定义绑定地址() {
        let args = parse_args(["--bind", "0.0.0.0:9000"].into_iter().map(String::from)).unwrap();
        assert_eq!(args.bind, "0.0.0.0:9000");
    }

    #[test]
    fn 未知参数报错() {
        assert!(parse_args(["--bogus"].into_iter().map(String::from)).is_err());
    }

    #[test]
    fn 缺少参数值报错() {
        assert!(parse_args(["--config"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--bind"].into_iter().map(String::from)).is_err());
    }
}
