use clap::Subcommand;
use tape_rs::client::fdb::FdbClient;
use tape_rs::logical_library::{
    ChangeLogicalLibrary, CreateLogicalLibrary, LogicalLibraryResources, ReplaceLogicalLibrary,
};
use uuid::Uuid;

#[derive(clap::Args)]
pub struct Resources {
    /// 驱动器序列号，可重复指定
    #[arg(long = "drive")]
    drives: Vec<String>,
    /// 物理库存中的存储元素地址，可重复指定
    #[arg(long = "storage-slot")]
    storage_slots: Vec<u16>,
    /// 物理库存中的 I/E 元素地址，可重复指定
    #[arg(long = "ie-slot")]
    ie_slots: Vec<u16>,
}

impl From<Resources> for LogicalLibraryResources {
    fn from(resources: Resources) -> Self {
        Self {
            drives: resources.drives,
            storage_slots: resources.storage_slots,
            ie_slots: resources.ie_slots,
        }
    }
}

#[derive(clap::Args)]
pub struct Mutation {
    id: Uuid,
    #[arg(long)]
    namespace: Uuid,
    /// 原配置的修订号
    #[arg(long)]
    revision: u64,
    /// 保存此 UUID，按原操作查询或重试
    #[arg(long)]
    operation: Uuid,
}

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
        #[command(flatten)]
        resources: Resources,
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
    /// 按当前修订号提交完整名称和资源配置
    Set {
        #[command(flatten)]
        target: Mutation,
        #[arg(long)]
        name: String,
        #[command(flatten)]
        resources: Resources,
    },
    /// 删除无引用的逻辑带库并释放空闲资源
    Delete(Mutation),
    /// 按原操作 UUID 查询修改或删除回执
    ChangeStatus {
        id: Uuid,
        #[arg(long)]
        namespace: Uuid,
        #[arg(long)]
        operation: Uuid,
    },
}

impl Mutation {
    fn submit(
        self,
        client: &FdbClient,
        library: &str,
        replacement: Option<ReplaceLogicalLibrary>,
    ) -> tape_rs::client::Result<serde_json::Value> {
        client.change_logical_library(
            library,
            self.namespace,
            self.id,
            ChangeLogicalLibrary {
                version: 1,
                operation: self.operation,
                revision: self.revision,
                replacement,
            },
        )
    }
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
            resources,
        } => client.create_logical_library(
            library,
            namespace,
            &CreateLogicalLibrary {
                version: 1,
                id,
                name,
                resources: resources.into(),
            },
        )?,
        Command::List { namespace } => client.list_logical_libraries(library, namespace)?,
        Command::Show {
            selector,
            namespace,
        } => client.logical_library(library, namespace, &selector)?,
        Command::Set {
            target,
            name,
            resources,
        } => target.submit(
            client,
            library,
            Some(ReplaceLogicalLibrary {
                name,
                resources: resources.into(),
            }),
        )?,
        Command::Delete(target) => target.submit(client, library, None)?,
        Command::ChangeStatus {
            id,
            namespace,
            operation,
        } => client.logical_library_change_status(library, namespace, id, operation)?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
