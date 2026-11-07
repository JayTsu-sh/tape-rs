//! Read-only object projection over imported children and published Active claims.
use super::*;
use std::collections::BTreeMap;

pub struct ObjectListRequest<'a> {
    pub pool: Uuid,
    pub prefix: &'a str,
    pub delimiter: &'a str,
    pub max_keys: usize,
    pub start_after: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectEntry {
    pub key: String,
    /// None denotes a CommonPrefixes item, which consumes one MaxKeys slot.
    pub file: Option<(Uuid, FileHead)>,
    pub modified: Option<i64>,
}

enum Work {
    Scan {
        parent: Uuid,
        path: String,
        after: Option<Vec<u8>>,
    },
    Directory {
        id: Uuid,
    },
    File {
        id: Uuid,
        head: FileHead,
        modified: Option<i64>,
    },
}

// A scan's priority is a LOWER BOUND, before any key it can still discover.
// Directory names acquire a '/' before ordering: blindly doing DFS by child
// name would incorrectly return a/x before a- or a.txt.
struct Walker<'a> {
    query: &'a ObjectListRequest<'a>,
    pending: BTreeMap<(String, u8), Work>,
    items: Vec<ObjectEntry>,
}
impl<'a> Walker<'a> {
    fn directory_relevant(&self, path: &str) -> bool {
        if !path.starts_with(self.query.prefix) && !self.query.prefix.starts_with(path) {
            return false;
        }
        if !self.query.delimiter.is_empty()
            && path.starts_with(self.query.prefix)
            && let Some(at) = path[self.query.prefix.len()..].find('/')
        {
            return &path[..self.query.prefix.len() + at + 1] > self.query.start_after;
        }
        true
    }
    fn push(&mut self, key: String, work: Work) -> Result<()> {
        if self.pending.len() >= 8192 {
            return Err(CatalogError::Incomplete);
        }
        let priority = u8::from(!matches!(work, Work::Scan { .. }));
        if self.pending.insert((key, priority), work).is_some() {
            return Err(CatalogError::Corrupt("对象遍历出现重复路径"));
        }
        Ok(())
    }
    fn file(&mut self, key: String, id: Uuid, head: FileHead, modified: Option<i64>) -> bool {
        if !key.starts_with(self.query.prefix) || key.as_str() <= self.query.start_after {
            return false;
        }
        let common = (!self.query.delimiter.is_empty())
            .then(|| key[self.query.prefix.len()..].find('/'))
            .flatten()
            .map(|i| key[..self.query.prefix.len() + i + 1].to_owned());
        let key = common.clone().unwrap_or(key);
        if key.as_str() <= self.query.start_after
            || self.items.last().is_some_and(|last| last.key == key)
        {
            return false;
        }
        if self.items.len() == self.query.max_keys {
            return true;
        }
        self.items.push(ObjectEntry {
            key,
            file: common.is_none().then_some((id, head)),
            modified: common.is_none().then_some(modified).flatten(),
        });
        false
    }
}

impl ManagedCatalog {
    /// One consistent FDB read version; no media I/O, attribute hydration,
    /// permanent flat-key projection or whole-namespace collection. Internal
    /// directory reads stay at 100; the public object page holds up to 1000.
    pub async fn list_active_objects_for_route(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        query: ObjectListRequest<'_>,
    ) -> Result<Page<ObjectEntry>> {
        control::validate_physical_serial(serial)?;
        if query.pool.is_nil()
            || query.max_keys > 1000
            || !matches!(query.delimiter, "" | "/")
            || query.prefix.len() > 2048
            || query.start_after.len() > 2048
            || query.prefix.contains('\0')
            || query.start_after.contains('\0')
        {
            return Err(CatalogError::Invalid("对象列举参数无效"));
        }
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        control::require_pool_library(&tr, namespace, query.pool, serial).await?;
        control::native_media::require_pool_readable(&tr, namespace, query.pool).await?;
        let pool_root = query.pool.to_string();
        let parent = match lookup::resolve_active_path_at_mode(
            &tr,
            namespace,
            vec![pool_root.as_bytes()],
            false,
        )
        .await?
        {
            Some(ResolvedPath::Directory { id, .. }) => id,
            None => {
                return Ok(Page {
                    items: vec![],
                    next: None,
                });
            }
            _ => return Err(CatalogError::Corrupt("池根节点不是目录")),
        };
        let mut walker = Walker {
            query: &query,
            pending: BTreeMap::new(),
            items: Vec::new(),
        };
        if query.max_keys == 0 {
            return Ok(Page {
                items: vec![],
                next: None,
            });
        }
        walker.push(
            String::new(),
            Work::Scan {
                parent,
                path: String::new(),
                after: None,
            },
        )?;
        let mut more = false;
        while let Some(((key, _), work)) = walker.pending.pop_first() {
            match work {
                Work::File { id, head, modified } => {
                    if walker.file(key, id, head, modified) {
                        more = true;
                        break;
                    }
                }
                Work::Directory { id } => {
                    if walker.directory_relevant(&key) {
                        walker.push(
                            key.clone(),
                            Work::Scan {
                                parent: id,
                                path: key,
                                after: None,
                            },
                        )?;
                    }
                }
                Work::Scan {
                    parent,
                    path,
                    after,
                } => {
                    let page = lookup::list_active_children_at(
                        &tr,
                        namespace,
                        parent,
                        after.as_deref(),
                        100,
                    )
                    .await?;
                    if let Some(after) = page.next {
                        let bound = decode_name(&after[child_prefix(namespace, parent).len()..])?;
                        let bound = std::str::from_utf8(&bound)
                            .map_err(|_| CatalogError::Invalid("对象名必须是 UTF-8"))?;
                        walker.push(
                            format!("{path}{bound}"),
                            Work::Scan {
                                parent,
                                path: path.clone(),
                                after: Some(after),
                            },
                        )?;
                    }
                    let mut lookups = Vec::with_capacity(page.items.len());
                    for child in page.items {
                        let name = std::str::from_utf8(&child.name)
                            .map_err(|_| CatalogError::Invalid("对象名必须是 UTF-8"))?;
                        let key = format!("{path}{name}");
                        if key.len() > 2048 {
                            return Err(CatalogError::Invalid("对象键超过 2048 字节"));
                        }
                        // Prune disjoint subtrees before metadata lookup.
                        if !key.starts_with(query.prefix)
                            && !query.prefix.starts_with(&format!("{key}/"))
                        {
                            continue;
                        }
                        let transaction = &tr;
                        let pool_root = &pool_root;
                        lookups.push((
                            Box::pin(async move {
                                let absolute = format!("/{pool_root}/{key}");
                                let resolved = lookup::resolve_active_path_at_mode(
                                    transaction,
                                    namespace,
                                    lookup::path_components(absolute.as_bytes())?,
                                    false,
                                )
                                .await?
                                .ok_or(CatalogError::Corrupt("对象子项缺少路径"))?;
                                Ok::<_, CatalogError>((key, resolved))
                            }),
                            None,
                        ));
                    }
                    // The directory page bounds fan-out at 100. Issue its
                    // independent proofs concurrently at the SAME read version;
                    // serial per-object round trips can exhaust FDB's 5s lifetime.
                    std::future::poll_fn(|cx| {
                        let mut pending = false;
                        for (lookup, result) in &mut lookups {
                            if result.is_none() {
                                match lookup.as_mut().poll(cx) {
                                    std::task::Poll::Ready(value) => *result = Some(value),
                                    std::task::Poll::Pending => pending = true,
                                }
                            }
                        }
                        if pending {
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(())
                        }
                    })
                    .await;
                    for (_, result) in lookups {
                        let (key, resolved) = result.expect("all page lookups completed")?;
                        match resolved {
                            ResolvedPath::Directory { id, .. } => {
                                let key = format!("{key}/");
                                if walker.directory_relevant(&key) {
                                    walker.push(key, Work::Directory { id })?;
                                }
                            }
                            ResolvedPath::File {
                                id,
                                head,
                                copy,
                                placement,
                                object_metadata,
                                ..
                            } => {
                                if placement.pool_id == query.pool
                                    && placement.library_serial == serial
                                    && !copy.media.as_ref().is_some_and(|m| m.native_symlink)
                                    && key.as_str() > query.start_after
                                {
                                    walker.push(
                                        key,
                                        Work::File {
                                            id,
                                            head,
                                            modified: object_metadata
                                                .and_then(|meta| meta.modified),
                                        },
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
        let next = if more {
            walker.items.last().map(|i| i.key.as_bytes().to_vec())
        } else {
            None
        };
        Ok(Page {
            items: walker.items,
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn object_order_is_by_full_utf8_key_and_common_prefixes_consume_slots() {
        let query = ObjectListRequest {
            pool: Uuid::new_v4(),
            prefix: "",
            delimiter: "",
            max_keys: 1000,
            start_after: "",
        };
        let mut w = Walker {
            query: &query,
            pending: BTreeMap::new(),
            items: vec![],
        };
        let head = FileHead {
            version: 1,
            size: 0,
            copy_id: Uuid::new_v4(),
        };
        for key in ["a/x", "a-", "a.txt", "a0", "z", "中文/a"] {
            w.push(
                key.into(),
                Work::File {
                    id: Uuid::new_v4(),
                    head: head.clone(),
                    modified: None,
                },
            )
            .unwrap();
        }
        while let Some(((key, _), Work::File { id, head, modified })) = w.pending.pop_first() {
            assert!(!w.file(key, id, head, modified));
        }
        assert_eq!(
            w.items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["a-", "a.txt", "a/x", "a0", "z", "中文/a"]
        );
        let query = ObjectListRequest {
            delimiter: "/",
            max_keys: 2,
            ..query
        };
        let mut w = Walker {
            query: &query,
            pending: BTreeMap::new(),
            items: vec![],
        };
        assert!(!w.file("a/x".into(), Uuid::new_v4(), head.clone(), None));
        assert!(!w.file("a/y".into(), Uuid::new_v4(), head.clone(), None));
        assert!(!w.file("b".into(), Uuid::new_v4(), head.clone(), None));
        assert!(w.file("c/x".into(), Uuid::new_v4(), head, None));
        assert_eq!(
            w.items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
            ["a/", "b"]
        );
        assert!(w.items[0].file.is_none());
    }
}
