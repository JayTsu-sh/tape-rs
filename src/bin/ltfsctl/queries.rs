//! Native operational reads and aligned ASCII columns; display names stay last.
use serde_json::{Value, json};
use std::collections::HashSet;
use tape_rs::client::fdb::FdbClient;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn print(value: Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
fn cell(value: &Value) -> String {
    let text = match value {
        Value::Null => "--".into(),
        Value::String(v) => v.clone(),
        _ => value.to_string(),
    };
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let widths = (0..headers.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].len())
                .chain(std::iter::once(headers[i].len()))
                .max()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    for row in
        std::iter::once(headers.iter().map(|s| s.to_string()).collect::<Vec<_>>()).chain(rows)
    {
        lines.push(
            row.into_iter()
                .enumerate()
                .map(|(i, v)| {
                    if i + 1 == headers.len() {
                        v
                    } else {
                        format!("{v:<width$}", width = widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  "),
        );
    }
    lines.join("\n")
}
fn no_pool(pool: Option<&str>) -> Result<()> {
    if pool.is_some() {
        Err("带库查询不接受--pool，请通过tape list按池查询".into())
    } else {
        Ok(())
    }
}
fn namespace(client: &FdbClient, library: &str) -> Result<String> {
    Ok(client.management_context(library)?["namespace"]
        .as_str()
        .ok_or("管理上下文缺少namespace")?
        .to_owned())
}
fn selected_pool(
    client: &FdbClient,
    library: &str,
    namespace: &str,
    pool: Option<&str>,
) -> Result<Option<String>> {
    pool.map(|p| {
        Ok(client.pool(library, namespace, p)?["pool"]["id"]
            .as_str()
            .ok_or("池UUID缺失")?
            .to_owned())
    })
    .transpose()
}
pub fn libraries(
    client: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    json: bool,
) -> Result<()> {
    no_pool(pool)?;
    let value = client.libraries(library)?;
    if json {
        return print(value);
    }
    let rows = value["items"]
        .as_array()
        .ok_or("带库清单缺失")?
        .iter()
        .map(|v| {
            vec![
                cell(&v["library_serial"]),
                cell(&v["active_node"]),
                cell(&v["num_of_drives"]),
                cell(&v["num_of_volumes"]),
                cell(&v["inventory_age_seconds"]),
                cell(&v["inventory"]["observed_at"]),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &[
                "LIBRARY",
                "ACTIVE_NODE",
                "DRIVES",
                "TAPES",
                "AGE(s)",
                "OBSERVED_AT"
            ],
            rows
        )
    );
    Ok(())
}
pub fn library(
    client: &FdbClient,
    library: Option<&str>,
    serial: Option<&str>,
    pool: Option<&str>,
) -> Result<()> {
    no_pool(pool)?;
    if matches!((library,serial),(Some(a),Some(b)) if a!=b) {
        return Err("show序列号与--library不符，请明确同一目标库".into());
    }
    let serial = serial
        .or(library)
        .ok_or("library show必须给出序列号或--library")?;
    let ns = namespace(client, serial)?;
    print(client.library(serial, &ns)?)
}
pub fn tapes(
    client: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let library = library.ok_or("tape list必须通过--library指定带库")?;
    let ns = namespace(client, library)?;
    let pool = selected_pool(client, library, &ns, pool)?;
    let mut after = None;
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    let mut inventory_available = false;
    loop {
        let page = client.tape_page(library, &ns, pool.as_deref(), after.as_deref())?;
        inventory_available |= page["inventory_available"] == true;
        for item in page["items"].as_array().ok_or("磁带页缺少items")? {
            if items
                .last()
                .is_some_and(|v: &Value| v["barcode"].as_str() >= item["barcode"].as_str())
            {
                return Err("跨页条码重复或无序，停止查询".into());
            }
            items.push(item.clone());
        }
        after = page["next"].as_str().map(str::to_owned);
        if let Some(cursor) = &after {
            if !seen.insert(cursor.clone()) {
                return Err("服务端重复磁带分页游标".into());
            }
        } else {
            break;
        }
    }
    let value = json!({"version":1,"library_serial":library,"namespace":ns,"pool":pool,"inventory_available":inventory_available,"items":items});
    if json_output {
        return print(value);
    }
    println!("{}", tape_table(&value));
    Ok(())
}
fn tape_table(value: &Value) -> String {
    let rows = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            vec![
                cell(&v["barcode"]),
                cell(&v["presence"]),
                cell(&v["location"]["address"]),
                cell(&v["assignment"]["online"]),
                cell(&v["assignment"]["blacklisted"]),
                cell(&v["pool"]["id"]),
                cell(&v["observation"]["observed_at"]),
                cell(&v["pool"]["name"]),
            ]
        })
        .collect();
    let body = table(
        &[
            "BARCODE",
            "PRESENCE",
            "ADDRESS",
            "ONLINE",
            "BLACKLISTED",
            "POOL_UUID",
            "OBSERVED_AT",
            "POOL_NAME",
        ],
        rows,
    );
    if value["inventory_available"] != true {
        format!("库存尚无观测；下表仅显示已有 FDB 归属。\n{body}")
    } else {
        body
    }
}

pub fn tape(
    client: &FdbClient,
    library: Option<&str>,
    pool: Option<&str>,
    barcode: &str,
) -> Result<()> {
    let library = library.ok_or("tape show必须通过--library指定带库")?;
    let ns = namespace(client, library)?;
    let pool = selected_pool(client, library, &ns, pool)?;
    print(client.tape(library, &ns, pool.as_deref(), barcode)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ascii_columns_align_and_control_characters_do_not_create_rows() {
        let out = table(
            &["BARCODE", "COUNT", "NAME"],
            vec![
                vec!["AA".into(), "18446744073709551615".into(), "归档一".into()],
                vec!["LONGER".into(), "1".into(), cell(&json!("归档\n二"))],
            ],
        );
        let lines = out.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].find("COUNT"), lines[1].find("184"));
        assert_eq!(lines[1].find("184"), lines[2].find('1'));
        assert_eq!(lines[0].find("NAME"), lines[1].find("归档"));
        assert_eq!(lines[1].find("归档"), lines[2].find("归档"));
        assert!(out.contains("归档\\n二"));
    }
    #[test]
    fn missing_inventory_is_explicit_in_default_tape_output() {
        let absent = json!({"inventory_available":false,"items":[]});
        assert!(tape_table(&absent).contains("库存尚无观测"));
        let empty = json!({"inventory_available":true,"items":[]});
        assert!(!tape_table(&empty).contains("库存尚无观测"));
        let assigned = json!({"inventory_available":false,"items":[{"barcode":"AA0001L8","presence":"unknown"}]});
        let text = tape_table(&assigned);
        assert!(text.contains("库存尚无观测") && text.contains("AA0001L8"));
    }
}
