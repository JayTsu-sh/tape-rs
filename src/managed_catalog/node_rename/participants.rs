//! The original complete participant set. Sealing is not media admission.
use super::*;
use std::{collections::VecDeque, sync::Arc};
const MEMBERS_PER_PAGE: usize = 32;
const MAX_RECORD: usize = 64_000;
#[path = "preparation.rs"]
mod preparation;
pub(in crate::managed_catalog) use preparation::{
    require_unreferenced_recipe_at, require_unsealed_at,
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRenameParticipant {
    #[serde(with = "uuid_text")]
    volume: Uuid,
    source_files: u64,
    target: bool,
    anchor: bool,
    recipe: [u8; 32],
}
impl NodeRenameParticipant {
    pub fn volume(&self) -> Uuid {
        self.volume
    }
    pub fn source_files(&self) -> u64 {
        self.source_files
    }
    pub fn includes_target(&self) -> bool {
        self.target
    }
    pub fn is_anchor(&self) -> bool {
        self.anchor
    }
    pub fn recipe_fingerprint(&self) -> [u8; 32] {
        self.recipe
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRenameParticipantsSummary {
    pub volumes: u64,
    pub source_files: u64,
    pub target_volumes: u64,
    pub anchors: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageFacts {
    version: u8,
    logical: [u8; 32],
    manifest: [u8; 32],
    index: u64,
    previous: [u8; 32],
    members: Vec<NodeRenameParticipant>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipantPage {
    facts: PageFacts,
    creator: u64,
    checksum: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalHeader {
    version: u8,
    logical: [u8; 32],
    manifest: [u8; 32],
    #[serde(with = "optional_uuid_text")]
    anchor: Option<Uuid>,
    #[serde(with = "optional_uuid_text")]
    target: Option<Uuid>,
    creator: u64,
    pages: u64,
    summary: NodeRenameParticipantsSummary,
    checksum: [u8; 32],
}
#[derive(Clone)]
pub struct NodeRenameGlobalPlan {
    manifest: Arc<NodeRenameManifest>,
    header: GlobalHeader,
}
impl NodeRenameGlobalPlan {
    pub fn summary(&self) -> &NodeRenameParticipantsSummary {
        &self.header.summary
    }
    pub fn creator_owner_epoch(&self) -> u64 {
        self.header.creator
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        self.header.summary.sha256
    }
    pub fn request(&self) -> &NodeRenameRequest {
        &self.manifest.request
    }
}
#[derive(Clone)]
pub struct NodeRenameParticipantsCapture {
    manifest: Arc<NodeRenameManifest>,
    anchor: Option<Uuid>,
    target: Option<Uuid>,
    target_original: Option<Uuid>,
    anchor_pending: Option<Uuid>,
    cursor: Option<Vec<u8>>,
    pending: VecDeque<NodeRenameProjectionVolume>,
    source_end: bool,
    last: Option<Uuid>,
    creator: u64,
    pages: u64,
    summary: NodeRenameParticipantsSummary,
    complete: bool,
}
impl NodeRenameParticipantsCapture {
    pub fn summary(&self) -> &NodeRenameParticipantsSummary {
        &self.summary
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
}
fn bad_participants() -> CatalogError {
    CatalogError::Corrupt("rename 原全局参与卷清单不符")
}
fn owner_valid(owner: u64) -> bool {
    owner > 0 && owner <= 0xFFFF_FFFF_FFFF
}
fn global_key(task: &NodeMutationTask) -> Vec<u8> {
    let mut k = prefix(task);
    k.push(3);
    k
}
fn participant_key(task: &NodeMutationTask, index: u64) -> Vec<u8> {
    let mut k = prefix(task);
    k.push(4);
    k.extend_from_slice(&index.to_be_bytes());
    k
}
fn target_volume(baseline: &NodeRenameBaseline) -> Option<Uuid> {
    match &baseline.target {
        Some(ResolvedPath::File { copy, .. }) => Some(copy.volume_id),
        _ => None,
    }
}
fn initial(manifest: &NodeRenameManifest, anchor: Option<Uuid>, target: Option<Uuid>) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-participants/1")
        .chain_update(manifest.request.fingerprint())
        .chain_update(manifest.header.summary.sha256)
        .chain_update(anchor.unwrap_or(Uuid::nil()).as_bytes())
        .chain_update(target.unwrap_or(Uuid::nil()).as_bytes())
        .finalize()
        .into()
}
fn encode_value<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = node_mutation::canonical(value)?;
    if bytes.len() > MAX_RECORD {
        return Err(bad_participants());
    }
    Ok(bytes)
}
fn decode_value<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_RECORD {
        return Err(bad_participants());
    }
    let value: T = serde_json::from_slice(bytes).map_err(|_| bad_participants())?;
    if encode_value(&value)? != bytes {
        return Err(bad_participants());
    }
    Ok(value)
}
impl ParticipantPage {
    fn new(facts: PageFacts, creator: u64) -> Result<Self> {
        if facts.version != 1
            || !owner_valid(creator)
            || facts.members.is_empty()
            || facts.members.len() > MEMBERS_PER_PAGE
        {
            return Err(bad_participants());
        }
        let mut last = None;
        for member in &facts.members {
            if member.volume.is_nil()
                || last.is_some_and(|id| id >= member.volume)
                || member.source_files == 0 && !member.target && !member.anchor
                || member.anchor && (member.source_files != 0 || member.target)
            {
                return Err(bad_participants());
            }
            last = Some(member.volume);
        }
        let checksum = Sha256::new()
            .chain_update(b"tape-rs/node-rename-participant-record/1")
            .chain_update(encode_value(&(&facts, creator))?)
            .finalize()
            .into();
        Ok(Self {
            facts,
            creator,
            checksum,
        })
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode_value(bytes)?;
        if Self::new(value.facts.clone(), value.creator)? != value {
            return Err(bad_participants());
        }
        Ok(value)
    }
    fn chain(&self) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-participant-page/1")
            .chain_update(encode_value(&self.facts)?)
            .finalize()
            .into())
    }
}
impl GlobalHeader {
    fn checksum(&self) -> Result<[u8; 32]> {
        Ok(Sha256::new()
            .chain_update(b"tape-rs/node-rename-global-header/1")
            .chain_update(encode_value(&(
                self.version,
                self.logical,
                self.manifest,
                self.anchor.map(|id| id.to_string()),
                self.target.map(|id| id.to_string()),
                self.creator,
                self.pages,
                &self.summary,
            ))?)
            .finalize()
            .into())
    }
    fn require(&self, manifest: &NodeRenameManifest) -> Result<()> {
        self.require_values(
            manifest.request.fingerprint(),
            manifest.header.summary.sha256,
            manifest.header.summary.files,
        )
    }
    fn require_values(&self, logical: [u8; 32], canonical: [u8; 32], files: u64) -> Result<()> {
        let target = self.target;
        if self.version != 1
            || !owner_valid(self.creator)
            || self.pages == 0
            || self.summary.volumes == 0
            || self.pages != self.summary.volumes.div_ceil(MEMBERS_PER_PAGE as u64)
            || self.logical != logical
            || self.manifest != canonical
            || self.summary.source_files != files
            || self.summary.target_volumes != u64::from(target.is_some())
            || self.summary.anchors != u64::from(self.anchor.is_some())
            || self.anchor.is_some_and(|a| a.is_nil())
            || self.target.is_some_and(|a| a.is_nil())
            || self.summary.volumes
                > self
                    .summary
                    .source_files
                    .saturating_add(self.summary.target_volumes)
                    .saturating_add(self.summary.anchors)
            || self.anchor.is_some() != (files == 0 && target.is_none())
            || self.checksum()? != self.checksum
        {
            return Err(bad_participants());
        }
        Ok(())
    }
    fn decode(bytes: &[u8], manifest: &NodeRenameManifest) -> Result<Self> {
        let value: Self = decode_value(bytes)?;
        value.require(manifest)?;
        Ok(value)
    }
}
fn add(
    summary: &mut NodeRenameParticipantsSummary,
    page: &ParticipantPage,
    last: &mut Option<Uuid>,
    anchor: Option<Uuid>,
    target: Option<Uuid>,
) -> Result<()> {
    for member in &page.facts.members {
        if last.is_some_and(|id| id >= member.volume)
            || member.anchor != (anchor == Some(member.volume))
            || member.target != (target == Some(member.volume))
        {
            return Err(bad_participants());
        }
        summary.volumes = summary
            .volumes
            .checked_add(1)
            .ok_or_else(bad_participants)?;
        summary.source_files = summary
            .source_files
            .checked_add(member.source_files)
            .ok_or_else(bad_participants)?;
        summary.target_volumes += u64::from(member.target);
        summary.anchors += u64::from(member.anchor);
        *last = Some(member.volume);
    }
    summary.sha256 = page.chain()?;
    Ok(())
}
fn next_member(
    source: Option<&NodeRenameProjectionVolume>,
    target: &mut Option<Uuid>,
    anchor: &mut Option<Uuid>,
) -> Option<NodeRenameParticipant> {
    let volume = [source.map(|v| v.volume), *target, *anchor]
        .into_iter()
        .flatten()
        .min()?;
    let source_files = source.filter(|v| v.volume == volume).map_or(0, |v| v.files);
    let includes_target = *target == Some(volume);
    let is_anchor = *anchor == Some(volume);
    if includes_target {
        *target = None;
    }
    if is_anchor {
        *anchor = None;
    }
    Some(NodeRenameParticipant {
        volume,
        source_files,
        target: includes_target,
        anchor: is_anchor,
        recipe: [0; 32],
    })
}
async fn require_original(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    manifest: &NodeRenameManifest,
) -> Result<()> {
    node_tasks::require_rename_prepared_at(tr, route, owner, &manifest.task, &manifest.request)
        .await?;
    require_header_at(tr, &manifest.task, &manifest.header).await
}
impl ManagedCatalog {
    /// Replay the original pages. An empty source recovers its anchor from
    /// the original first page, including the crash window before the header.
    pub async fn resume_node_rename_participants(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        projection: &mut NodeRenameProjectionSpool,
    ) -> Result<NodeRenameParticipantsCapture> {
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        let baseline = revalidate_at(&tr, route, owner, &manifest.request).await?;
        let anchor = if manifest.header.summary.files == 0 && target_volume(&baseline).is_none() {
            let raw = match tr.get(&participant_key(&manifest.task, 0), false).await? {
                Some(raw) => raw,
                None => {
                    preparation::require_no_saved_participants(&tr, manifest).await?;
                    return Err(CatalogError::Incomplete);
                }
            };
            let page = ParticipantPage::decode(&raw)?;
            let [member] = page.facts.members.as_slice() else {
                return Err(bad_participants());
            };
            if page.creator > owner
                || page.facts.index != 0
                || page.facts.logical != manifest.request.fingerprint()
                || page.facts.manifest != manifest.header.summary.sha256
                || !member.anchor
                || member.target
                || member.source_files != 0
                || page.facts.previous != initial(manifest, Some(member.volume), None)
            {
                return Err(bad_participants());
            }
            Some(member.volume)
        } else {
            if tr
                .get(&participant_key(&manifest.task, 0), false)
                .await?
                .is_none()
            {
                preparation::require_no_saved_participants(&tr, manifest).await?;
            }
            None
        };
        drop(tr);
        self.begin_node_rename_participants(route, owner, manifest, projection, anchor)
            .await
    }
    pub async fn begin_node_rename_participants(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        projection: &mut NodeRenameProjectionSpool,
        anchor: Option<Uuid>,
    ) -> Result<NodeRenameParticipantsCapture> {
        projection.require_manifest(&manifest.request, &manifest.header.summary)?;
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        let baseline = revalidate_at(&tr, route, owner, &manifest.request).await?;
        let target = target_volume(&baseline);
        if anchor.is_some_and(|a| a.is_nil())
            || anchor.is_some() != (manifest.header.summary.files == 0 && target.is_none())
        {
            return Err(invalid(
                "rename 空子树须指定一个原合格锚卷，非空子树不能增加锚卷",
            ));
        }
        if let Some(raw) = tr.get(&global_key(&manifest.task), false).await? {
            let header = GlobalHeader::decode(&raw, manifest)?;
            if header.creator > owner || header.anchor != anchor || header.target != target {
                return Err(bad_participants());
            }
        }
        Ok(NodeRenameParticipantsCapture {
            manifest: Arc::new(manifest.clone()),
            anchor,
            target,
            target_original: target,
            anchor_pending: anchor,
            cursor: None,
            pending: VecDeque::new(),
            source_end: false,
            last: None,
            creator: 0,
            pages: 0,
            summary: NodeRenameParticipantsSummary {
                volumes: 0,
                source_files: 0,
                target_volumes: 0,
                anchors: 0,
                sha256: initial(manifest, anchor, target),
            },
            complete: false,
        })
    }
    pub async fn next_node_rename_participants_page(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &mut NodeRenameParticipantsCapture,
        projection: &mut NodeRenameProjectionSpool,
    ) -> Result<Vec<NodeRenameParticipant>> {
        let manifest = &capture.manifest;
        projection.require_manifest(&manifest.request, &manifest.header.summary)?;
        if capture.complete {
            let tr = self.db.create_trx()?;
            require_original(&tr, route, owner, manifest).await?;
            return Ok(Vec::new());
        }
        let (mut next, members) = preparation::candidate_page(capture, projection)?;
        if members.is_empty() {
            let tr = self.db.create_trx()?;
            require_original(&tr, route, owner, manifest).await?;
            *capture = next;
            return Ok(members);
        }
        let index = capture.pages;
        let previous = capture.summary.sha256;
        let record = self
            .db
            .run(|tr, _| {
                let candidates = members.clone();
                async move {
                    requests::retry_result(
                        async {
                            require_original(&tr, route, owner, manifest).await?;
                            let key = participant_key(&manifest.task, index);
                            let old = tr
                                .get(&key, false)
                                .await?
                                .map(|raw| ParticipantPage::decode(&raw))
                                .transpose()?;
                            if old.is_none() {
                                preparation::require_fresh_page_at(
                                    &tr, manifest, owner, index, previous,
                                )
                                .await?;
                            }
                            let mut verified = Vec::new();
                            for mut member in candidates {
                                let raw = tr
                                    .get(&volume_plan_key(&manifest.task, member.volume), false)
                                    .await?
                                    .ok_or_else(|| {
                                        if old.is_some() {
                                            bad_participants()
                                        } else {
                                            CatalogError::Incomplete
                                        }
                                    })?;
                                let recipe = volume_plan::Record::decode(&raw)?;
                                require_volume_plan_scope(&recipe, manifest, owner)?;
                                let binding = recipe.facts.binding()?;
                                if binding.source.volume_uuid != member.volume
                                    || recipe.facts.source_files() != member.source_files
                                {
                                    return Err(bad_participants());
                                }
                                member.recipe = recipe.facts.fingerprint();
                                if old.is_none()
                                    && live_guard::binding_at(&tr, &manifest.request, member.volume)
                                        .await?
                                        != binding
                                {
                                    return Err(invalid("rename 原参与卷绑定已改变"));
                                }
                                verified.push(member);
                            }
                            let facts = PageFacts {
                                version: 1,
                                logical: manifest.request.fingerprint(),
                                manifest: manifest.header.summary.sha256,
                                index,
                                previous,
                                members: verified,
                            };
                            if let Some(saved) = old {
                                if saved.creator > owner || saved.facts != facts {
                                    return Err(bad_participants());
                                }
                                return Ok(saved);
                            }
                            let record = ParticipantPage::new(facts, owner)?;
                            tr.set(&key, &encode_value(&record)?);
                            Ok(record)
                        }
                        .await,
                    )
                }
            })
            .await??;
        if next.pages == 0 {
            next.creator = record.creator;
        }
        add(
            &mut next.summary,
            &record,
            &mut next.last,
            capture.anchor,
            capture.target_original,
        )?;
        next.pages = next.pages.checked_add(1).ok_or_else(bad_participants)?;
        let items = record.facts.members;
        *capture = next;
        Ok(items)
    }
    pub async fn finish_node_rename_participants(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &NodeRenameParticipantsCapture,
    ) -> Result<NodeRenameGlobalPlan> {
        if !capture.complete {
            return Err(CatalogError::Incomplete);
        }
        let manifest = &capture.manifest;
        let mut header = GlobalHeader {
            version: 1,
            logical: manifest.request.fingerprint(),
            manifest: manifest.header.summary.sha256,
            anchor: capture.anchor,
            target: capture.target_original,
            creator: capture.creator,
            pages: capture.pages,
            summary: capture.summary.clone(),
            checksum: [0; 32],
        };
        header.checksum = header.checksum()?;
        header.require(manifest)?;
        self.verify_participant_pages(route, owner, manifest, &header)
            .await?;
        let header = self
            .db
            .run(|tr, _| {
                let header = header.clone();
                async move {
                    requests::retry_result(
                        async {
                            require_original(&tr, route, owner, manifest).await?;
                            let key = global_key(&manifest.task);
                            if let Some(raw) = tr.get(&key, false).await? {
                                let saved = GlobalHeader::decode(&raw, manifest)?;
                                if saved != header {
                                    return Err(bad_participants());
                                }
                                return Ok(saved);
                            }
                            preparation::require_unsealed_at(&tr, manifest).await?;
                            revalidate_at(&tr, route, owner, &manifest.request).await?;
                            let raw = tr
                                .get(&participant_key(&manifest.task, header.pages - 1), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let tail = ParticipantPage::decode(&raw)?;
                            if tail.facts.index != header.pages - 1
                                || tail.chain()? != header.summary.sha256
                            {
                                return Err(bad_participants());
                            }
                            tr.set(&key, &encode_value(&header)?);
                            Ok(header)
                        }
                        .await,
                    )
                }
            })
            .await??;
        Ok(NodeRenameGlobalPlan {
            manifest: Arc::clone(manifest),
            header,
        })
    }
    pub async fn node_rename_global_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
    ) -> Result<Option<NodeRenameGlobalPlan>> {
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        let Some(raw) = tr.get(&global_key(&manifest.task), false).await? else {
            preparation::require_unsealed_at(&tr, manifest).await?;
            return Ok(None);
        };
        let header = GlobalHeader::decode(&raw, manifest)?;
        let baseline = revalidate_at(&tr, route, owner, &manifest.request).await?;
        if header.creator > owner || target_volume(&baseline) != header.target {
            return Err(bad_participants());
        }
        drop(tr);
        self.verify_participant_pages(route, owner, manifest, &header)
            .await?;
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        let raw = tr
            .get(&global_key(&manifest.task), false)
            .await?
            .ok_or_else(bad_participants)?;
        if GlobalHeader::decode(&raw, manifest)? != header {
            return Err(bad_participants());
        }
        Ok(Some(NodeRenameGlobalPlan {
            manifest: Arc::new(manifest.clone()),
            header,
        }))
    }
    async fn verify_participant_pages(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        manifest: &NodeRenameManifest,
        header: &GlobalHeader,
    ) -> Result<()> {
        let mut summary = NodeRenameParticipantsSummary {
            volumes: 0,
            source_files: 0,
            target_volumes: 0,
            anchors: 0,
            sha256: initial(manifest, header.anchor, header.target),
        };
        let mut last = None;
        for index in 0..header.pages {
            let tr = self.db.create_trx()?;
            require_original(&tr, route, owner, manifest).await?;
            let raw = tr
                .get(&participant_key(&manifest.task, index), false)
                .await?
                .ok_or_else(bad_participants)?;
            let page = ParticipantPage::decode(&raw)?;
            if page.creator > owner
                || page.facts.index != index
                || page.facts.logical != header.logical
                || page.facts.manifest != header.manifest
                || page.facts.previous != summary.sha256
                || index + 1 < header.pages && page.facts.members.len() != MEMBERS_PER_PAGE
                || index == 0 && page.creator != header.creator
            {
                return Err(bad_participants());
            }
            for member in &page.facts.members {
                let raw = tr
                    .get(&volume_plan_key(&manifest.task, member.volume), false)
                    .await?
                    .ok_or_else(bad_participants)?;
                let recipe = volume_plan::Record::decode(&raw)?;
                require_volume_plan_scope(&recipe, manifest, owner)?;
                if recipe.facts.binding()?.source.volume_uuid != member.volume
                    || recipe.facts.source_files() != member.source_files
                    || recipe.facts.fingerprint() != member.recipe
                {
                    return Err(bad_participants());
                }
            }
            add(&mut summary, &page, &mut last, header.anchor, header.target)?;
        }
        if summary != header.summary {
            return Err(bad_participants());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "participants_tests.rs"]
mod tests;

mod uuid_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        id: &Uuid,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        node_mutation::id(&text).map_err(serde::de::Error::custom)
    }
}
mod optional_uuid_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        id: &Option<Uuid>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        id.map(|id| id.to_string()).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Uuid>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| node_mutation::id(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[path = "attempts.rs"]
mod attempts;
pub use attempts::{NodeRenameAttempt, NodeRenameAttemptPhase, NodeRenameVolumeIntent};
pub(in crate::managed_catalog) use attempts::{
    require_no_prepared_attempt_at, require_prepared_volume_claim_at,
};

pub(in crate::managed_catalog) use attempts::require_no_missing_prepared_claim_at;

pub(in crate::managed_catalog) use attempts::require_writing_task_at;

pub use attempts::NodeRenameObservation;
pub(in crate::managed_catalog) use attempts::require_rename_observation_at;

pub use attempts::NodeRenameIntentClosure;
pub(in crate::managed_catalog) use attempts::require_rename_discard_at;

pub use attempts::NodeRenamePublication;
pub(in crate::managed_catalog) use attempts::require_rename_task_outcome_at;
