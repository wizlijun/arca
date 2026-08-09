//! # arca CLI
//!
//! 手动同步一等公民（spec §3.1）：一次性进程，如 git，无需任何 daemon。
//! 双形态：独立 `arca …` 与子命令 `git arca …`。
//!
//! CLI 纪律（spec §3.2）：
//! - plumbing / porcelain 分层：一切能力先以输出稳定、可脚本化的 plumbing 存在；
//! - Rule of Silence：成功时安静；数据走 stdout，进度与诊断走 stderr；处处可加 `--json`；
//! - 与 git 同名的动词语义必须一致：status / fetch / pull / push；`sync` = pull + push。

#![forbid(unsafe_code)]

mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arca", about = "git 仓库的二进制附件层")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 巡检一个存储根的完整性（只读，绝不修改任何文件）
    Fsck {
        /// 存储根路径（含 files/ 与 .arca/）
        root: std::path::PathBuf,
    },
    /// 在 vault 根建 `.gitarca`（若已存在则只校验、不覆盖）、装 pre-push 钩子
    Init {
        /// vault 内任意路径，默认当前目录
        path: Option<std::path::PathBuf>,
        /// 跳过 pre-push 钩子安装
        #[arg(long)]
        no_hook: bool,
    },
    /// 把一个目录登记为数据集：建 dataset.toml、更新 .gitarca、更新 .gitignore
    Register {
        /// 数据集路径，相对 vault 根
        path: String,
        /// hub 名（.gitarca 里的 `[hub.<name>]`）
        #[arg(long)]
        hub: String,
        /// hub 不存在时用它创建；已存在则必须与登记的一致
        #[arg(long = "hub-instance-id")]
        hub_instance_id: Option<String>,
        /// hub 不存在时用它创建（file:// 或裸本地路径）；已存在则更新其 url
        #[arg(long = "hub-url")]
        hub_url: Option<String>,
        /// hub 是新建的且未给 --hub-url 时，从这个路径推导 file:// 地址
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 新建 dataset.toml 时用它代替随机生成的 dataset_id——"加入一个
        /// 已经在 hub 上存在的数据集"时用它声明已知的 id（M2c Task 5，两机
        /// 端到端场景：第二台设备必须用第一台设备 adopt 时分配到的同一个
        /// dataset_id，不能各自随机生成两个互不相干的 id）
        #[arg(long = "dataset-id")]
        dataset_id: Option<String>,
    },
    /// 就地纳管一个已登记的数据集：算哈希、上传、写 .gitignore 块（文件原地不动）
    Adopt {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径（外置盘换挂载点等场景）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 显式承认"就是要在这个（当前打不开的）路径上新建一个全新存储根"
        /// ——数据集此前已被纳管过（.arca/manifest 存在）而存储根缺失时，
        /// 默认按 I11 拒绝凭空新建，必须用这个开关明确表态（评审 Critical #2）
        #[arg(long = "create-root")]
        create_root: bool,
    },
    /// 对一个已纳管的数据集跑一轮 file:// 调和闭环
    Sync {
        /// 数据集路径，相对 vault 根；省略则同步 vault 内全部已登记数据集
        /// （M2d Task 3：一个 hub 不可达只让它承载的数据集离线，不影响其余）
        path: Option<String>,
        /// 覆盖从 .gitarca 解析出的存储根路径（只在指定单个数据集路径时有效）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 比对本地与 hub，不动数据；全同步时安静，退出码 0
    Status {
        /// 数据集路径，相对 vault 根；省略则报告 vault 内全部已登记数据集
        /// （M2d Task 3）
        path: Option<String>,
        /// 覆盖从 .gitarca 解析出的存储根路径（只在指定单个数据集路径时有效）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// fixity 巡检（BLAKE3 重算对账）。file:// hub 默认就是全量 fixity
    /// （逐文件重算哈希）；http(s):// hub 默认**只校验元数据一致性**——
    /// 把 hub 声明的哈希/大小与本地基线对照，**不重算任何内容字节**，
    /// 因此发现不了 hub 上的位腐或被外部改写的内容；要真正验内容加
    /// --deep（会把整个数据集的字节从网络上拉一遍），或者更省事：在 hub
    /// 那台机器上直接对存储根跑 `arca fsck <存储根>`
    Verify {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 拉取全部内容重算 BLAKE3 与 hub 声明的哈希比对（只对 http(s)://
        /// hub 有意义；file:// 默认就已经是这一档）。代价是整个数据集的
        /// 字节都要过一遍网络
        #[arg(long)]
        deep: bool,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 查看或设置一个数据集在本机的存储角色：server（永久保留）/ client
    /// （可再生缓存，默认，M2d Task 1，spec §4.7）
    Role {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 设为 server 或 client；不带此参数则只查看当前角色
        #[arg(long)]
        set: Option<String>,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 一致性巡检：.gitarca 一致性 + 本地存在但 hub 尚无副本的文件告警
    Doctor {
        /// 覆盖从 .gitarca 解析出的存储根路径（对本次巡检的全部数据集生效）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 保留期内一条命令找回被删除的文件（spec §7）
    Restore {
        /// 数据集路径，相对 vault 根
        dataset: String,
        /// 数据集内的文件路径——恢复目标；与 --list 二选一
        file: Option<String>,
        /// 只列出回收站里的条目，不实际恢复（列出的是全部条目，最后一列
        /// within_retention 表示它是否仍在默认保留期内）
        #[arg(long)]
        list: bool,
        /// 从**本设备**工作区侧的本地回收站（<dataset>/.arca/client/trash/，
        /// server 角色下远端删除过闸门后本地副本的落点）找回，而不是默认的
        /// hub 侧回收站（<存储根>/.arca/trash/）。默认那条是"把这个文件在
        /// 整个数据集范围内找回来"（写回 hub，所有设备都会看到）；--local
        /// 是"把这台机器上被删掉的那份副本捞回来"（纯本地，不碰 hub，
        /// hub 离线也能跑）
        #[arg(long)]
        local: bool,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 物理销毁已过保留期的回收站条目（spec §7、I3）——arca 里唯一一条会
    /// 真的删掉你的字节的命令。默认是 dry-run：只出清单，什么都不销毁；
    /// 只有显式加 --yes 才会动手。绝不会被任何东西自动触发：cron 里写
    /// `arca gc` 是你自己的决定
    Gc {
        /// 数据集路径，相对 vault 根
        dataset: String,
        /// 清理**本机工作区侧**的本地回收站（<dataset>/.arca/client/trash/，
        /// server 角色下远端删除过闸门后本地副本的落点），而不是默认的
        /// hub 侧回收站（<存储根>/.arca/trash/）。纯本地操作，hub 离线也能跑
        #[arg(long)]
        local: bool,
        /// 显式承认默认行为（只出清单不销毁）。不给这个开关行为也一样——
        /// 它存在是为了让脚本能把"我确实只想预览"写出来
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// 真的物理销毁清单里的条目。没有这个开关时一个字节都不会被删
        #[arg(long)]
        yes: bool,
        /// 连**仍在保留期内**的条目也一起销毁。保留期存在的意义就是给
        /// "删错了"留一段可以反悔的时间；越过它之后这些内容在本机就再也
        /// 找不回来了（除非另一台设备或备份里还有）。必须与 --yes 同时
        /// 给出才有效，单独给它不会销毁任何东西
        #[arg(long = "include-unexpired")]
        include_unexpired: bool,
        /// 保留期天数，默认 180（spec §7）。调小它会让更多条目变成销毁
        /// 候选——同样只在 --yes 下才真的销毁
        #[arg(long = "retention-days")]
        retention_days: Option<i64>,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// 一条命令收齐诊断现场（spec §3.3，借 git 的 git bugreport）：版本、
    /// 平台、各数据集的角色与健康度、最近的 trace 落盘文件列表、本地回收站
    /// 占用与清单、.gitignore 反选块的实测结果、hub 可达性。**只收元数据、
    /// 路径与 arca 自己的诊断结论，绝不读取任何受管文件的内容**；输出直接
    /// 打到 stdout，不落盘、不上传——你在按回车之前就能把收了什么看个遍
    Bugreport,
    /// 产出站点生成器消费的 publish-map.json（spec §4.9）。**只产出映射，
    /// 绝不改写 vault 里的 md**；映射完全由清单构造，**一个 blob 都不读**——
    /// CI 可以在不下载任何二进制的前提下构建出图片可访问的静态站
    #[command(name = "publish-map")]
    PublishMap {
        /// 全量公开整个数据集。**默认只收录被 md 引用到的资源**——直接公开
        /// 整个数据集会暴露没被任何已发布笔记引用的文件，那是隐私事故的常见
        /// 来源（spec §4.9 ③）。扩大暴露面必须是显式动作
        #[arg(long)]
        all: bool,
        /// 写到这个文件；不给就打到 stdout
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// hub 端点的本机操作（目前只有 TLS 证书 pin，spec §9）
    Hub {
        #[command(subcommand)]
        action: HubCommand,
    },
    /// plumbing：hub 侧当前清单（--json 输出，格式见 PROTOCOL.md §5）
    Ls {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 目前唯一支持的输出格式；保留该开关是为了未来扩展其它格式时不破坏调用方
        #[arg(long)]
        json: bool,
    },
    /// plumbing：按内容哈希取字节，原样写 stdout
    Cat {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 内容哈希（blake3:<hex> 形式）
        hash: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// plumbing：路径 → hub 侧身份/版本
    Resolve {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 数据集内的文件路径
        file: String,
        /// 覆盖从 .gitarca 解析出的存储根路径
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// plumbing：客户端本地投影（基线）检视
    State {
        #[command(subcommand)]
        action: StateCommand,
    },
}

#[derive(Subcommand)]
enum HubCommand {
    /// 为一个 https:// hub 记录自签名证书的指纹 pin（spec §9）。不带
    /// --fingerprint 时**只打印**服务端此刻出示的指纹、不写入任何东西：
    /// arca 绝不"首次使用即信任"，请先用带外渠道核对（在 hub 那台机器上
    /// `openssl x509 -in <证书> -noout -fingerprint -sha256`），确认无误后
    /// 把同一个值显式抄回 --fingerprint。pin 一旦记录，该 hub 的证书变化
    /// 就会让 arca 拒绝连接并要你重新确认
    Trust {
        /// hub 名（.gitarca 里的 `[hub.<name>]`）
        name: String,
        /// 你已经带外核对过的指纹，形如 sha256:<64 位小写十六进制>
        #[arg(long)]
        fingerprint: Option<String>,
    },
}

#[derive(Subcommand)]
enum StateCommand {
    /// 导出当前基线（--json 输出，格式见 PROTOCOL.md §5）
    Dump {
        /// 数据集路径，相对 vault 根
        path: String,
        /// 覆盖从 .gitarca 解析出的存储根路径（basline dump 本身不需要打开
        /// 存储根，这个参数只是为了与其它 plumbing 命令的调用形状保持一致）
        #[arg(long)]
        root: Option<std::path::PathBuf>,
        /// 目前唯一支持的输出格式
        #[arg(long)]
        json: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Fsck { root } => {
            // 挂载失败（根不存在、身份读不出来等）与「巡检发现问题」是两种
            // 不同性质的结果：前者连身份都不明，退出码 2；与逃生舱脚本的
            // 约定一致（2 = 身份不明，1 = 有问题，0 = 干净）。
            let report = match arca_store::fsck::check_path(&root) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("{e}");
                    return std::process::ExitCode::from(2);
                }
            };
            // Rule of Silence（spec §3.2）：成功时安静，退出码 0 本身就是答案。
            // 摘要行是诊断，不是可脚本消费的数据，连同逐条问题一起走 stderr；
            // 干净时完全不打印任何东西。
            if report.problems.is_empty() {
                std::process::ExitCode::SUCCESS
            } else {
                for problem in &report.problems {
                    eprintln!("{problem:?}");
                }
                eprintln!(
                    "检查 {} 个文件、{} 个块，发现 {} 个问题",
                    report.checked_files,
                    report.checked_chunks,
                    report.problems.len()
                );
                std::process::ExitCode::from(1)
            }
        }
        Command::Init { path, no_hook } => commands::porcelain::init_cmd(path, no_hook),
        Command::Register {
            path,
            hub,
            hub_instance_id,
            hub_url,
            root,
            dataset_id,
        } => commands::porcelain::register_cmd(
            &path,
            &hub,
            hub_instance_id.as_deref(),
            hub_url.as_deref(),
            root.as_deref(),
            dataset_id.as_deref(),
        ),
        Command::Adopt {
            path,
            root,
            create_root,
        } => commands::porcelain::adopt_cmd(&path, root.as_deref(), create_root),
        Command::Sync { path, root } => {
            commands::porcelain::sync_cmd(path.as_deref(), root.as_deref())
        }
        Command::Status { path, root } => {
            commands::porcelain::status_cmd(path.as_deref(), root.as_deref())
        }
        Command::Verify { path, deep, root } => {
            commands::porcelain::verify_cmd(&path, deep, root.as_deref())
        }
        Command::Role { path, set, root } => {
            commands::porcelain::role_cmd(&path, set.as_deref(), root.as_deref())
        }
        Command::Doctor { root } => commands::porcelain::doctor_cmd(root.as_deref()),
        Command::Restore {
            dataset,
            file,
            list,
            local,
            root,
        } => match (list, local) {
            (true, false) => commands::porcelain::restore_list_cmd(&dataset, root.as_deref()),
            (true, true) => commands::porcelain::restore_local_list_cmd(&dataset, root.as_deref()),
            (false, _) => match file {
                Some(f) if local => {
                    commands::porcelain::restore_local_cmd(&dataset, &f, root.as_deref())
                }
                Some(f) => commands::porcelain::restore_cmd(&dataset, &f, root.as_deref()),
                None => {
                    eprintln!("`arca restore` 需要指定要找回的文件路径，或改用 --list");
                    std::process::ExitCode::from(1)
                }
            },
        },
        Command::Gc {
            dataset,
            local,
            dry_run,
            yes,
            include_unexpired,
            retention_days,
            root,
        } => {
            // `--dry-run` 与 `--yes` 同时给出是矛盾的意图——绝不"取其一"
            // 继续（I5）：这是一条会销毁数据的命令，任何一点关于用户到底
            // 想要什么的猜测都不可接受。
            if dry_run && yes {
                eprintln!(
                    "`--dry-run` 与 `--yes` 不能同时给出：前者是「只看不动」，后者是\
                     「真的销毁」，两者矛盾。已停止，什么都没做。"
                );
                return std::process::ExitCode::from(1);
            }
            commands::porcelain::gc_cmd(
                &dataset,
                local,
                yes,
                include_unexpired,
                retention_days,
                root.as_deref(),
            )
        }
        Command::PublishMap { all, out } => {
            commands::porcelain::publish_map_cmd(all, out.as_deref())
        }
        Command::Bugreport => commands::porcelain::bugreport_cmd(),
        Command::Hub { action } => match action {
            HubCommand::Trust { name, fingerprint } => {
                commands::porcelain::hub_trust_cmd(&name, fingerprint.as_deref())
            }
        },
        Command::Ls {
            path,
            root,
            json: _,
        } => commands::plumbing::ls_cmd(&path, root.as_deref()),
        Command::Cat { path, hash, root } => {
            commands::plumbing::cat_cmd(&path, &hash, root.as_deref())
        }
        Command::Resolve { path, file, root } => {
            commands::plumbing::resolve_cmd(&path, &file, root.as_deref())
        }
        Command::State { action } => match action {
            StateCommand::Dump {
                path,
                root,
                json: _,
            } => commands::plumbing::state_dump_cmd(&path, root.as_deref()),
        },
    }
}
