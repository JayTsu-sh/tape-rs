//! Management and explicit ACKs for durable external notifications.
use clap::Subcommand;
use serde_json::json;
use tape_rs::client::{Client, fdb::FdbClient};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum Command {
    /// 注入有明确标记的测试通知；不修改真实磁带状态、不访问设备
    Inject {
        /// 同一次注入重试必须保持此 UUID 和内容不变
        #[arg(long)]
        operation: Uuid,
        #[arg(long)]
        event_type: String,
        /// 事件data对象的JSON，最大8KiB
        #[arg(long)]
        data: String,
    },
    /// 创建持久订阅；重复执行同配置不会重置进度
    Create {
        id: String,
        /// 回放保留的全部历史；默认从创建订阅时的位置开始
        #[arg(long)]
        from_start: bool,
        #[arg(long = "barcode")]
        barcodes: Vec<String>,
        #[arg(long = "event-type")]
        event_types: Vec<String>,
    },
    /// 列出持久消费者
    List,
    /// 查看实例标识、确认位置及积压量
    Show { id: String },
    /// 查询当前批次；不自动确认
    Read {
        id: String,
        #[arg(long)]
        generation: Uuid,
    },
    /// SSE 持续接收并自动重连；输出不代表处理成功，须另行 ACK
    Watch {
        id: String,
        #[arg(long)]
        generation: Uuid,
        /// 指定包含原订阅类型的集合；允许扩大，不允许缩小
        #[arg(long = "event-type", conflicts_with = "all_event_types")]
        event_types: Vec<String>,
        /// 扩大到全部事件类型
        #[arg(long)]
        all_event_types: bool,
    },
    /// 处理并可靠保存完整批次后显式确认
    Ack {
        id: String,
        #[arg(long)]
        generation: Uuid,
        #[arg(long)]
        delivery_id: Uuid,
        #[arg(long)]
        through: u64,
    },
    /// 删除订阅并明确放弃其未确认通知
    Delete {
        id: String,
        #[arg(long)]
        generation: Uuid,
        #[arg(long, required = true)]
        abandon_unacknowledged: bool,
    },
}

pub fn run(
    client: &mut Client,
    fdb: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    command: Command,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = super::pool::library(client, library)?;
    let value = match command {
        Command::Inject {
            operation,
            event_type,
            data,
        } => fdb.inject_notification(
            &library,
            operation,
            &event_type,
            serde_json::from_str(&data)?,
        )?,
        Command::Create {
            id,
            from_start,
            barcodes,
            event_types,
        } => {
            let pool = pool
                .map(|p| super::pool::selected(fdb, &library, p))
                .transpose()?
                .map(|(_, p)| p["id"].clone());
            fdb.create_notify_consumer(
                &library,
                &id,
                json!({"pool":pool,"barcodes":barcodes,"event_types":event_types}),
                from_start,
            )?
        }
        Command::List => {
            let mut values = Vec::new();
            let mut after = None;
            loop {
                let page = fdb.notify_consumers(&library, after.as_deref())?;
                values.extend(
                    page["consumers"]
                        .as_array()
                        .ok_or("消费者清单无效")?
                        .clone(),
                );
                after = page["next"].as_str().map(str::to_owned);
                if after.is_none() {
                    break;
                }
            }
            json!({"consumers":values})
        }
        Command::Show { id } => fdb.notify_consumer(&library, &id)?,
        Command::Read { id, generation } => fdb.notify_batch(&library, &id, generation)?,
        Command::Ack {
            id,
            generation,
            delivery_id,
            through,
        } => fdb.acknowledge_notify(&library, &id, generation, delivery_id, through)?,
        Command::Delete {
            id,
            generation,
            abandon_unacknowledged,
        } => {
            if !abandon_unacknowledged {
                return Err("删除必须确认放弃未消费通知".into());
            }
            fdb.delete_notify_consumer(&library, &id, generation)?
        }
        Command::Watch {
            id,
            generation,
            event_types,
            all_event_types,
        } => {
            use std::io::Write;
            fdb.watch_notifications(
                &library,
                &id,
                generation,
                if all_event_types || !event_types.is_empty() {
                    Some(event_types.as_slice())
                } else {
                    None
                },
                |batch| {
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "{batch}")?;
                    out.flush()?;
                    Ok(true)
                },
            )?;
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
