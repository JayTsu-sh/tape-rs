use clap::Subcommand;
use tape_rs::client::fdb::FdbClient;
use tape_rs::logical_library::{CreateLogicalLibrary, LogicalLibraryResources};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum Command {
    /// 为逻辑带库分配物理资源
    Create {
        name: String,
        #[arg(long)]
        namespace: Uuid,
        /// 预先生成并保存此 UUID，结果未知时使用相同请求重试
        #[arg(long)]
        id: Uuid,
        /// 驱动器序列号，可重复指定
        #[arg(long = "drive")]
        drives: Vec<String>,
        /// 物理库存中的存储元素地址，可重复指定
        #[arg(long = "storage-slot")]
        storage_slots: Vec<u16>,
        /// 物理库存中的 I/E 元素地址，可重复指定
        #[arg(long = "ie-slot")]
        ie_slots: Vec<u16>,
    },
    /// 列出逻辑带库和未分配资源
    List {
        #[arg(long)]
        namespace: Uuid,
    },
    /// 查看逻辑带库的配置和库存观测
    Show {
        selector: String,
        #[arg(long)]
        namespace: Uuid,
    },
}

pub(super) fn run(
    client: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = library.ok_or("逻辑带库管理必须通过 --library 指定物理带库序列号")?;
    if pool.is_some() {
        return Err("逻辑带库管理不接受 --pool".into());
    }
    let value = match command {
        Command::Create {
            name,
            namespace,
            id,
            drives,
            storage_slots,
            ie_slots,
        } => client.create_logical_library(
            library,
            namespace,
            &CreateLogicalLibrary {
                version: 1,
                id,
                name,
                resources: LogicalLibraryResources {
                    drives,
                    storage_slots,
                    ie_slots,
                },
            },
        )?,
        Command::List { namespace } => client.list_logical_libraries(library, namespace)?,
        Command::Show {
            selector,
            namespace,
        } => client.logical_library(library, namespace, &selector)?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
