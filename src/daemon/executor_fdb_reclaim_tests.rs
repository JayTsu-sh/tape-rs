//! Exact original-child commit boundary, isolated FDB + Sim only.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct AfterChildBarrier {
    inner: Box<dyn TapeTransport + Send>,
    catalog: Arc<crate::managed_catalog::ManagedCatalog>,
    parent: crate::managed_catalog::ReclaimReservation,
    owner: u64,
    route: crate::managed_catalog::ActiveLibraryRoute,
    fired: Arc<AtomicBool>,
    terminal: bool,
    before_body: bool,
    original: Option<(crate::managed_catalog::ActiveFileRequest, String)>,
}
impl AfterChildBarrier {
    pub(super) fn after_terminal(
        inner: Box<dyn TapeTransport + Send>,
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        parent: crate::managed_catalog::ReclaimReservation,
        owner: u64,
        route: crate::managed_catalog::ActiveLibraryRoute,
        fired: Arc<AtomicBool>,
        original: (crate::managed_catalog::ActiveFileRequest, String),
    ) -> Self {
        let mut value = Self::new(inner, catalog, parent, owner, route, fired);
        value.terminal = true;
        value.original = Some(original);
        value
    }
    pub(super) fn before_body(
        inner: Box<dyn TapeTransport + Send>,
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        parent: crate::managed_catalog::ReclaimReservation,
        owner: u64,
        route: crate::managed_catalog::ActiveLibraryRoute,
        fired: Arc<AtomicBool>,
    ) -> Self {
        let mut value = Self::new(inner, catalog, parent, owner, route, fired);
        value.before_body = true;
        value
    }
    fn child_pending(&self) -> Result<bool> {
        futures_executor::block_on(async {
            let page = self
                .catalog
                .reclaim_uploads_page(&self.parent, self.owner, None, 16)
                .await?;
            for request in page.items {
                if self
                    .catalog
                    .active_file_attempt(&request, self.route)
                    .await?
                    .is_some()
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .map_err(|e: crate::managed_catalog::CatalogError| TapeError::Catalog(e.to_string()))
    }
    fn terminal_window(&self) -> Result<bool> {
        let (request, barcode) = self.original.as_ref().expect("terminal boundary request");
        futures_executor::block_on(async {
            Ok(matches!(
                self.catalog
                    .active_upload_result(request, self.route)
                    .await?,
                Some(crate::managed_catalog::PublishOutcome::AlreadyApplied(_))
            ) && self
                .catalog
                .active_tape_monitor(&request.library_serial, self.route, barcode)
                .await?
                .is_some_and(|m| m.requires_validation))
        })
        .map_err(|e: crate::managed_catalog::CatalogError| TapeError::Catalog(e.to_string()))
    }

    pub(super) fn new(
        inner: Box<dyn TapeTransport + Send>,
        catalog: Arc<crate::managed_catalog::ManagedCatalog>,
        parent: crate::managed_catalog::ReclaimReservation,
        owner: u64,
        route: crate::managed_catalog::ActiveLibraryRoute,
        fired: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner,
            catalog,
            parent,
            owner,
            route,
            fired,
            terminal: false,
            before_body: false,
            original: None,
        }
    }
}
impl TapeTransport for AfterChildBarrier {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.inner.execute_no_data(cdb, timeout)?;
        if !self.terminal
            && !self.before_body
            && cdb.first() == Some(&0x10)
            && cdb.get(2..5) == Some(&[0, 0, 0])
            && !self.fired.load(Ordering::SeqCst)
        {
            let pending = self.child_pending()?;
            if pending && !self.fired.swap(true, Ordering::SeqCst) {
                return Err(TapeError::ScsiHost(3));
            }
        }
        Ok(result)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        out: &mut [u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        let result = self.inner.execute_read(cdb, out, timeout)?;
        if self.terminal
            && cdb.first() == Some(&0x5e)
            && !self.fired.load(Ordering::SeqCst)
            && self.terminal_window()?
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        Ok(result)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        if self.before_body
            && cdb.first() == Some(&0x0a)
            && !self.fired.load(Ordering::SeqCst)
            && self.child_pending()?
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        self.inner.execute_write(cdb, data, timeout)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}

/// Fails the original managed handle once at a precise append stage. A new
/// SimTransport handle has a separate fault queue and cannot exercise it.
pub(super) struct FailOneAppend {
    inner: Box<dyn TapeTransport + Send>,
    opcode: u8,
    after: usize,
    matching: std::sync::atomic::AtomicUsize,
    fired: Arc<AtomicBool>,
}
impl FailOneAppend {
    pub(super) fn new(
        inner: Box<dyn TapeTransport + Send>,
        opcode: u8,
        after: usize,
        fired: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner,
            opcode,
            after,
            matching: std::sync::atomic::AtomicUsize::new(0),
            fired,
        }
    }
    fn before(&self, cdb: &[u8]) -> Result<()> {
        if cdb.first() == Some(&self.opcode)
            && !self.fired.load(Ordering::SeqCst)
            && self.matching.fetch_add(1, Ordering::SeqCst) == self.after
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            return Err(TapeError::ScsiHost(3));
        }
        Ok(())
    }
}
impl TapeTransport for FailOneAppend {
    fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> Result<crate::scsi::device::ScsiResult> {
        self.before(cdb)?;
        self.inner.execute_no_data(cdb, timeout)
    }
    fn execute_read(
        &self,
        cdb: &[u8],
        out: &mut [u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        self.before(cdb)?;
        self.inner.execute_read(cdb, out, timeout)
    }
    fn execute_write(
        &self,
        cdb: &[u8],
        data: &[u8],
        timeout: u32,
    ) -> Result<crate::scsi::device::ScsiResult> {
        self.before(cdb)?;
        self.inner.execute_write(cdb, data, timeout)
    }
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn session_epoch(&self) -> Option<u64> {
        self.inner.session_epoch()
    }
    fn execution_guard(&self) -> Option<(u64, usize)> {
        self.inner.execution_guard()
    }
}
