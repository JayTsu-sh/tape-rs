use clap::Args;
use serde_json::Value;
use tape_rs::client::Client;

#[derive(Args, Default)]
pub struct ListOutput {
    #[arg(long, conflicts_with = "jsonpp")]
    json: bool,
    #[arg(long)]
    jsonpp: bool,
    #[arg(long)]
    no_header: bool,
}

#[derive(Args, Default)]
pub struct ShowOutput {
    /// 只显示指定属性
    #[arg(short = 'a', long)]
    pub attribute: Option<String>,
    #[arg(long, conflicts_with = "jsonpp")]
    json: bool,
    #[arg(long)]
    jsonpp: bool,
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::String(v) => v.replace(['\n', '\r', '\t'], " "),
        other => other.to_string(),
    }
}

fn json(value: &Value, compact: bool, pretty: bool) -> bool {
    if compact {
        println!("{value}");
    } else if pretty {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    }
    compact || pretty
}

pub fn libraries(value: Value, output: ListOutput) {
    if json(&value, output.json, output.jsonpp) {
        return;
    }
    let rows: Vec<_> = value["libraries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| ["name", "model", "serial", "active_node"].map(|key| text(&row[key])))
        .collect();
    print!(
        "{}",
        super::table::render(
            ["Library name", "Model", "Serial number", "Active node"],
            &rows,
            &[3],
            !output.no_header
        )
    );
}

pub fn show(value: Value, output: ShowOutput) -> Result<(), Box<dyn std::error::Error>> {
    let payload = value.get("payload").unwrap_or(&value);
    let value = if let Some(attribute) = output.attribute {
        let field = payload.get(&attribute).ok_or("未知资源属性")?;
        serde_json::json!({attribute:field})
    } else {
        payload.clone()
    };
    if !json(&value, output.json, output.jsonpp) {
        let rows: Vec<_> = value
            .as_object()
            .ok_or("资源详情不是对象")?
            .iter()
            .map(|(key, value)| [key.clone(), text(value)])
            .collect();
        print!("{}", super::table::render(["", ""], &rows, &[], false));
    }
    Ok(())
}

pub fn tapes(
    client: &mut Client,
    fdb: &tape_rs::client::fdb::FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    output: ListOutput,
    unit: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if unit.is_some() && (output.json || output.jsonpp) {
        return Err("--unit 不能与 JSON 输出同时使用".into());
    }
    let binary = unit.as_deref() == Some("binary");
    if unit
        .as_deref()
        .is_some_and(|s| s != "decimal" && s != "binary")
    {
        return Err("--unit 仅支持 decimal 或 binary".into());
    }
    let serial = if library.is_some() || pool.is_some() {
        Some(super::pool::library(client, library)?)
    } else {
        None
    };
    let pool = if let Some(selector) = pool {
        let (_, p) = super::pool::selected(fdb, serial.as_deref().unwrap(), selector)?;
        Some(
            p["id"]
                .as_str()
                .ok_or("池 UUID 缺失")?
                .parse::<uuid::Uuid>()?,
        )
    } else {
        None
    };
    let mut value = match serial.as_deref() {
        Some(serial) => fdb.tape_inventory_page(serial, pool, 1000, None)?,
        None => client.admin_get("/admin/tapes?limit=1000")?,
    };
    let mut rows = value["payload"]["tapes"]
        .as_array()
        .ok_or("磁带清单无效")?
        .clone();
    let mut next = value["payload"]["next"].as_str().map(str::to_owned);
    while let Some(token) = next {
        let page = match serial.as_deref() {
            Some(serial) => fdb.tape_inventory_page(serial, pool, 1000, Some(&token))?,
            None => client.admin_get(&format!("/admin/tapes?limit=1000&after={token}"))?,
        };
        rows.extend(
            page["payload"]["tapes"]
                .as_array()
                .ok_or("磁带续页无效")?
                .iter()
                .cloned(),
        );
        next = page["payload"]["next"].as_str().map(str::to_owned);
        if next.as_deref() == Some(token.as_str()) {
            return Err("磁带续页游标未推进".into());
        }
    }
    value["payload"]["tapes"] = Value::Array(rows);
    value["payload"]["next"] = Value::Null;
    if json(&value, output.json, output.jsonpp) {
        return Ok(());
    }
    let headers = [
        "Tape ID".into(),
        "Status".into(),
        "State".into(),
        format!("Usable({})", if binary { "GiB" } else { "GB" }),
        "Used".into(),
        "Available".into(),
        "Reclaimable%".into(),
        "Pool".into(),
        "Library".into(),
        "Location".into(),
        "Task ID".into(),
    ];
    let capacity = |v: &Value| {
        v.as_u64()
            .map(|n| {
                format!(
                    "{:.3}",
                    n as f64 / if binary { 1073741824.0 } else { 1000000000.0 }
                )
            })
            .unwrap_or("-".into())
    };
    let rows: Vec<_> = value["payload"]["tapes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            [
                text(&row["tapeid"]),
                text(&row["status"]),
                text(&row["state"]),
                capacity(&row["usable"]),
                capacity(&row["used"]),
                capacity(&row["available"]),
                text(&row["reclaimable"]),
                text(&row["poolname"]),
                text(&row["library"]),
                text(&row["location"]),
                text(&row["task_id"]),
            ]
        })
        .collect();
    print!(
        "{}",
        super::table::render(headers, &rows, &[3, 4, 5, 6], !output.no_header)
    );
    Ok(())
}

pub fn pools(value: Value, output: ListOutput) {
    if json(&value, output.json, output.jsonpp) {
        return;
    }
    let rows: Vec<_> = value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| ["name", "id", "library_serial", "file_limit"].map(|key| text(&row[key])))
        .collect();
    print!(
        "{}",
        super::table::render(
            ["Pool name", "Pool UUID", "Library", "File limit"],
            &rows,
            &[3],
            !output.no_header
        )
    );
}

pub fn tape(
    client: &mut Client,
    fdb: &tape_rs::client::fdb::FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    barcode: &str,
    output: ShowOutput,
) -> Result<(), Box<dyn std::error::Error>> {
    let serial = super::pool::library(client, library)?;
    let pool = if let Some(selector) = pool {
        let (_, p) = super::pool::selected(fdb, &serial, selector)?;
        Some(
            p["id"]
                .as_str()
                .ok_or("池 UUID 缺失")?
                .parse::<uuid::Uuid>()?,
        )
    } else {
        None
    };
    show(fdb.tape_inventory_entry(&serial, pool, barcode)?, output)
}
