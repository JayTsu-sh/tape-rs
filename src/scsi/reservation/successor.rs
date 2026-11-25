//! Strict single-device successor PR engine. No production authority exists.
//! An issued command is never retried or cleaned up by this module.
use super::*;
use sha2::{Digest, Sha256};

mod sealed {
    pub trait Sealed {}
}

#[derive(Clone, Debug)]
pub(crate) struct SuccessorFenceBinding {
    transition: [u8; 32],
    attempt: [u8; 32],
    device_intent: [u8; 32],
    scope: [u8; 32],
    lu: [u8; 32],
    target: [u8; 32],
    nexus: [u8; 32],
    key: ReservationKey,
    round: u64,
    original_keys: Vec<u64>,
    original_nexus_key: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletePrObservation {
    generation: u32,
    keys: Vec<u64>,
    holder: Option<(u64, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Register,
    Reserve,
    PreemptAbort(u64),
    Preempt(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlannedPrOut {
    step: u32,
    action: Action,
    before: CompletePrObservation,
    after_keys: Vec<u64>,
    after_holder: Option<(u64, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssuedPrOut {
    binding: [u8; 32],
    before_hash: [u8; 32],
    plan: PlannedPrOut,
}

#[derive(Clone, Debug)]
struct ConfirmedStep {
    intent: IssuedPrOut,
    after: CompletePrObservation,
}

#[derive(Clone, Debug)]
pub(crate) struct DeviceFenceCheckpoint {
    binding: [u8; 32],
    original_before: CompletePrObservation,
    confirmed: Vec<ConfirmedStep>,
    pending: Option<IssuedPrOut>,
}

#[derive(Debug, PartialEq, Eq)]
enum Submission {
    NotSubmitted,
    Acknowledged,
    Unknown,
    ObservedOriginal,
}

#[derive(Debug)]
pub(crate) struct PrStepObservation {
    submission: Submission,
    observed: Option<CompletePrObservation>,
    allowed_after: bool,
    device_fenced: bool,
    error: Option<TapeError>,
}

#[derive(Debug)]
pub(crate) struct DeviceFenced {
    binding: [u8; 32],
    before: CompletePrObservation,
    after: CompletePrObservation,
}

#[derive(Debug)]
pub(crate) enum DeviceFenceProgress {
    StepObserved {
        step: u32,
        after: CompletePrObservation,
    },
    DeviceFenced(DeviceFenced),
    RecoveryRequired {
        intent: Option<IssuedPrOut>,
        observation: PrStepObservation,
        record_error: Option<TapeError>,
    },
    Rejected {
        reason: String,
    },
    Unsupported,
    Superseded {
        key: ReservationKey,
    },
}

pub(crate) trait SuccessorFenceAuthority<'d>: sealed::Sealed {
    fn transport(&self) -> &'d dyn TapeTransport;
    fn binding(&self) -> &SuccessorFenceBinding;
    fn check_live(&mut self) -> Result<()>;
    fn checkpoint(&mut self) -> Result<DeviceFenceCheckpoint>;
    fn issue(&mut self, step: &PlannedPrOut, before: &CompletePrObservation)
    -> Result<IssuedPrOut>;
    fn record(&mut self, intent: &IssuedPrOut, observed: &PrStepObservation) -> Result<()>;
}

fn reject(reason: &str) -> TapeError {
    TapeError::Refused {
        reason: reason.into(),
    }
}

impl CompletePrObservation {
    fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new().chain_update(b"tape-rs/successor-pr-observation/1");
        h.update(self.generation.to_be_bytes());
        h.update((self.keys.len() as u64).to_be_bytes());
        for k in &self.keys {
            h.update(k.to_be_bytes());
        }
        match self.holder {
            None => h.update([0]),
            Some((key, typ)) => {
                h.update([1]);
                h.update(key.to_be_bytes());
                h.update([typ]);
            }
        }
        h.finalize().into()
    }
}

impl SuccessorFenceBinding {
    fn validate(&self) -> Result<()> {
        if [
            self.transition,
            self.attempt,
            self.device_intent,
            self.scope,
            self.lu,
            self.target,
            self.nexus,
        ]
        .iter()
        .any(|d| *d == [0; 32])
            || self.round == 0
            || self.round > 0xffff_ffff_ffff
            || !self.key.is_ours()
            || self.key.node() == 0
            || self.key.round() != self.round
            || self.original_keys.windows(2).any(|k| k[0] >= k[1])
            || self
                .original_keys
                .iter()
                .any(|k| !ReservationKey(*k).is_ours() || *k == self.key.0)
            || self.original_keys.len() + usize::from(self.original_nexus_key.is_none()) > 127
            || self
                .original_nexus_key
                .is_some_and(|k| !self.original_keys.contains(&k))
        {
            return Err(reject("successor原设备/nexus/未截断round或注册证明无效"));
        }
        Ok(())
    }
    fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new().chain_update(b"tape-rs/successor-pr-binding/1");
        for d in [
            self.transition,
            self.attempt,
            self.device_intent,
            self.scope,
            self.lu,
            self.target,
            self.nexus,
        ] {
            h.update(d);
        }
        h.update(self.key.0.to_be_bytes());
        h.update(self.round.to_be_bytes());
        h.update((self.original_keys.len() as u64).to_be_bytes());
        for k in &self.original_keys {
            h.update(k.to_be_bytes());
        }
        match self.original_nexus_key {
            None => h.update([0]),
            Some(k) => {
                h.update([1]);
                h.update(k.to_be_bytes());
            }
        }
        h.finalize().into()
    }
}

struct BoundIo<'d> {
    device: &'d dyn TapeTransport,
    binding: SuccessorFenceBinding,
}
impl<'d> BoundIo<'d> {
    fn live<A: SuccessorFenceAuthority<'d> + ?Sized>(&self, a: &mut A) -> Result<()> {
        a.check_live()?;
        if a.binding().hash() != self.binding.hash() || !std::ptr::eq(self.device, a.transport()) {
            return Err(reject("successor执行权改变了原设备或scope"));
        }
        Ok(())
    }
    fn read<A: SuccessorFenceAuthority<'d> + ?Sized>(&self, a: &mut A, sa: u8) -> Result<Vec<u8>> {
        self.live(a)?;
        let mut buf = [0u8; 1024];
        let result = self.device.execute_read(
            &cdb::persistent_reserve_in(sa, 1024),
            &mut buf,
            PR_TIMEOUT_MS,
        )?;
        if result.status != 0 {
            return Err(TapeError::ScsiStatus {
                device: self.device.identity().into(),
                status: result.status,
            });
        }
        if result.transferred > buf.len() {
            return Err(reject("PR IN实际传输长度超出allocation"));
        }
        Ok(buf[..result.transferred].to_vec())
    }
    fn pair<A: SuccessorFenceAuthority<'d> + ?Sized>(
        &self,
        a: &mut A,
    ) -> Result<CompletePrObservation> {
        decode_pair(
            &self.read(a, SA_IN_READ_KEYS)?,
            &self.read(a, SA_IN_READ_RESERVATION)?,
        )
    }
    fn observe<A: SuccessorFenceAuthority<'d> + ?Sized>(
        &self,
        a: &mut A,
    ) -> Result<CompletePrObservation> {
        // Two complete pairs, within the contract's maximum three rounds.
        // Any change rejects this advance; this is not a device atomic snapshot.
        let first = self.pair(a)?;
        let second = self.pair(a)?;
        if first != second {
            return Err(reject("完整PR观察期间注册、持有者或generation改变"));
        }
        Ok(second)
    }
}

fn header(data: &[u8]) -> Result<(u32, usize)> {
    if data.len() < 8 {
        return Err(TapeError::InvalidResponse {
            expected: 8,
            actual: data.len(),
        });
    }
    let generation = u32::from_be_bytes(data[..4].try_into().unwrap());
    let extra = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    if extra.checked_add(8) != Some(data.len()) {
        return Err(reject("PR IN声明长度与完整响应不符"));
    }
    Ok((generation, extra))
}
fn decode_pair(keys: &[u8], reservation: &[u8]) -> Result<CompletePrObservation> {
    let (generation, extra) = header(keys)?;
    if extra % 8 != 0 {
        return Err(reject("READ KEYS含不完整key"));
    }
    let mut keys: Vec<u64> = keys[8..]
        .chunks_exact(8)
        .map(|k| u64::from_be_bytes(k.try_into().unwrap()))
        .collect();
    keys.sort_unstable();
    if keys.contains(&0) || keys.windows(2).any(|k| k[0] == k[1]) {
        return Err(reject("PR重复或零key无法确定nexus"));
    }
    let (rg, len) = header(reservation)?;
    if rg != generation {
        return Err(reject("READ KEYS与READ RESERVATION代数不一致"));
    }
    let holder = match len {
        0 => None,
        16 => {
            if reservation[16..21] != [0; 5]
                || reservation[21] != PR_TYPE_EXCLUSIVE_ACCESS
                || reservation[22..24] != [0; 2]
            {
                return Err(reject("只允许LU作用域Exclusive Access原预留"));
            }
            let key = u64::from_be_bytes(reservation[8..16].try_into().unwrap());
            if !keys.contains(&key) {
                return Err(reject("PR持有者不在完整注册集合"));
            }
            Some((key, PR_TYPE_EXCLUSIVE_ACCESS))
        }
        _ => return Err(reject("READ RESERVATION描述符截断或无法解释")),
    };
    Ok(CompletePrObservation {
        generation,
        keys,
        holder,
    })
}

fn plan_next(
    binding: &SuccessorFenceBinding,
    before: &CompletePrObservation,
    step: u32,
) -> Result<Option<PlannedPrOut>> {
    let mut keys = before.keys.clone();
    let mut holder = before.holder;
    let action = if !keys.contains(&binding.key.0) {
        if step != 0 {
            return Err(reject("已确认的本attempt注册消失"));
        }
        if let Some(old) = binding.original_nexus_key {
            let at = keys
                .iter()
                .position(|k| *k == old)
                .ok_or_else(|| reject("原nexus注册缺失"))?;
            keys.remove(at);
            if holder.map(|h| h.0) == Some(old) {
                holder = Some((binding.key.0, PR_TYPE_EXCLUSIVE_ACCESS));
            }
        }
        keys.push(binding.key.0);
        keys.sort_unstable();
        Action::Register
    } else if step == 0 {
        return Err(reject("没有原ISSUED的本attempt key不能认领"));
    } else {
        match holder {
            None => {
                holder = Some((binding.key.0, PR_TYPE_EXCLUSIVE_ACCESS));
                Action::Reserve
            }
            Some((h, PR_TYPE_EXCLUSIVE_ACCESS)) if h != binding.key.0 => {
                if !binding.original_keys.contains(&h) {
                    return Err(reject("不允许抢占未知holder"));
                }
                keys.retain(|k| *k != h);
                holder = Some((binding.key.0, PR_TYPE_EXCLUSIVE_ACCESS));
                Action::PreemptAbort(h)
            }
            Some((h, PR_TYPE_EXCLUSIVE_ACCESS)) if h == binding.key.0 => {
                let Some(stale) = keys.iter().copied().find(|k| *k != binding.key.0) else {
                    return Ok(None);
                };
                if !binding.original_keys.contains(&stale) {
                    return Err(reject("不允许清理未知注册"));
                }
                keys.retain(|k| *k != stale);
                Action::Preempt(stale)
            }
            _ => return Err(reject("不允许更改未知预留类型")),
        }
    };
    Ok(Some(PlannedPrOut {
        step,
        action,
        before: before.clone(),
        after_keys: keys,
        after_holder: holder,
    }))
}
fn allowed_after(plan: &PlannedPrOut, after: &CompletePrObservation) -> bool {
    plan.after_keys == after.keys
        && plan.after_holder == after.holder
        && (plan.action == Action::Reserve || plan.before.generation != after.generation)
}
fn fenced(
    binding: &SuccessorFenceBinding,
    original: &CompletePrObservation,
    after: &CompletePrObservation,
) -> bool {
    after.keys == [binding.key.0]
        && after.holder == Some((binding.key.0, PR_TYPE_EXCLUSIVE_ACCESS))
        && after.generation != original.generation
}
fn verify_checkpoint(
    binding: &SuccessorFenceBinding,
    cp: &DeviceFenceCheckpoint,
) -> Result<CompletePrObservation> {
    if cp.binding != binding.hash()
        || cp.original_before.keys != binding.original_keys
        || cp.confirmed.len() > binding.original_keys.len() + 2
    {
        return Err(reject("原device checkpoint身份或步数不符"));
    }
    let mut current = cp.original_before.clone();
    for (step, confirmed) in cp.confirmed.iter().enumerate() {
        let planned = plan_next(binding, &current, step as u32)?
            .ok_or_else(|| reject("完成后仍存在多余PR step"))?;
        verify_issued(binding, &planned, &confirmed.intent)?;
        if !allowed_after(&planned, &confirmed.after) {
            return Err(reject("原确认步骤不符合exact允许迁移"));
        }
        current = confirmed.after.clone();
    }
    if let Some(pending) = &cp.pending {
        let plan = plan_next(binding, &current, cp.confirmed.len() as u32)?
            .ok_or_else(|| reject("已完成设备不能有新pending"))?;
        verify_issued(binding, &plan, pending)?;
    }
    Ok(current)
}
fn verify_issued(
    binding: &SuccessorFenceBinding,
    plan: &PlannedPrOut,
    intent: &IssuedPrOut,
) -> Result<()> {
    if intent.binding != binding.hash()
        || intent.before_hash != plan.before.hash()
        || intent.plan != *plan
    {
        return Err(reject("ISSUED不是原device/scope/step/before"));
    }
    Ok(())
}
fn recovery(
    intent: Option<IssuedPrOut>,
    observation: PrStepObservation,
    record_error: Option<TapeError>,
) -> DeviceFenceProgress {
    DeviceFenceProgress::RecoveryRequired {
        intent,
        observation,
        record_error,
    }
}
fn not_submitted<'d, A: SuccessorFenceAuthority<'d> + ?Sized>(
    a: &mut A,
    intent: IssuedPrOut,
    error: TapeError,
) -> DeviceFenceProgress {
    let observation = PrStepObservation {
        submission: Submission::NotSubmitted,
        observed: None,
        allowed_after: false,
        device_fenced: false,
        error: Some(error),
    };
    let record_error = a.record(&intent, &observation).err();
    recovery(Some(intent), observation, record_error)
}
fn final_progress(
    binding: &SuccessorFenceBinding,
    cp: &DeviceFenceCheckpoint,
    intent: &IssuedPrOut,
    after: CompletePrObservation,
) -> DeviceFenceProgress {
    if fenced(binding, &cp.original_before, &after) {
        DeviceFenceProgress::DeviceFenced(DeviceFenced {
            binding: binding.hash(),
            before: cp.original_before.clone(),
            after,
        })
    } else {
        DeviceFenceProgress::StepObserved {
            step: intent.plan.step,
            after,
        }
    }
}

pub(crate) fn advance_device<'d, A: SuccessorFenceAuthority<'d> + ?Sized>(
    a: &mut A,
) -> Result<DeviceFenceProgress> {
    run(a, false)
}
pub(crate) fn observe_original<'d, A: SuccessorFenceAuthority<'d> + ?Sized>(
    a: &mut A,
) -> Result<DeviceFenceProgress> {
    run(a, true)
}
fn run<'d, A: SuccessorFenceAuthority<'d> + ?Sized>(
    a: &mut A,
    observe_only: bool,
) -> Result<DeviceFenceProgress> {
    a.binding().validate()?;
    let io = BoundIo {
        device: a.transport(),
        binding: a.binding().clone(),
    };
    io.live(a)?;
    let observed = match io.observe(a) {
        Ok(s) => s,
        Err(e) if is_unsupported(&e) => return Ok(DeviceFenceProgress::Unsupported),
        Err(e) => return Err(e),
    };
    let cp = a.checkpoint()?;
    let expected = verify_checkpoint(&io.binding, &cp)?;
    for old in &io.binding.original_keys {
        let old = ReservationKey(*old);
        if old.is_ours() && old.round() >= io.binding.round {
            return Ok(DeviceFenceProgress::Superseded { key: old });
        }
    }
    if let Some(intent) = cp.pending.clone() {
        if !observe_only {
            return Ok(recovery(
                Some(intent),
                PrStepObservation {
                    submission: Submission::Unknown,
                    observed: Some(observed),
                    allowed_after: false,
                    device_fenced: false,
                    error: None,
                },
                None,
            ));
        }
        if !allowed_after(&intent.plan, &observed) {
            return Ok(recovery(
                Some(intent),
                PrStepObservation {
                    submission: Submission::Unknown,
                    observed: Some(observed),
                    allowed_after: false,
                    device_fenced: false,
                    error: None,
                },
                None,
            ));
        }
        let observation = PrStepObservation {
            submission: Submission::ObservedOriginal,
            device_fenced: fenced(&io.binding, &cp.original_before, &observed),
            observed: Some(observed.clone()),
            allowed_after: true,
            error: None,
        };
        if let Err(e) = a.record(&intent, &observation) {
            return Ok(recovery(Some(intent), observation, Some(e)));
        }
        if let Err(e) = io.live(a) {
            return Ok(recovery(Some(intent), observation, Some(e)));
        }
        return Ok(final_progress(&io.binding, &cp, &intent, observed));
    }
    if observed != expected {
        return Ok(DeviceFenceProgress::Rejected {
            reason: "介质状态偏离原已确认checkpoint，禁止新PR OUT".into(),
        });
    }
    let Some(plan) = plan_next(&io.binding, &observed, cp.confirmed.len() as u32)? else {
        io.live(a)?;
        if fenced(&io.binding, &cp.original_before, &observed) {
            return Ok(DeviceFenceProgress::DeviceFenced(DeviceFenced {
                binding: io.binding.hash(),
                before: cp.original_before,
                after: observed,
            }));
        }
        return Ok(DeviceFenceProgress::Rejected {
            reason: "generation相同，不能证明单设备fence完成".into(),
        });
    };
    if observe_only {
        return Ok(recovery(
            None,
            PrStepObservation {
                submission: Submission::NotSubmitted,
                observed: Some(observed),
                allowed_after: false,
                device_fenced: false,
                error: None,
            },
            None,
        ));
    }
    let intent = a.issue(&plan, &observed)?;
    if let Err(e) = verify_issued(&io.binding, &plan, &intent) {
        return Ok(not_submitted(a, intent, e));
    }
    if let Err(e) = io.live(a) {
        return Ok(not_submitted(a, intent, e));
    }
    let before = match io.observe(a) {
        Ok(s) => s,
        Err(e) => return Ok(not_submitted(a, intent, e)),
    };
    if before != plan.before {
        return Ok(not_submitted(a, intent, reject("ISSUED后完整before改变")));
    }
    if let Err(e) = io.live(a) {
        return Ok(not_submitted(a, intent, e));
    }
    // The only PR OUT call in this engine. Do not use pr_out/UA retry wrappers.
    let (sa, typ, key, sa_key) = match plan.action {
        Action::Register => (SA_OUT_REGISTER_IGNORE, 0, 0, io.binding.key.0),
        Action::Reserve => (
            SA_OUT_RESERVE,
            PR_TYPE_EXCLUSIVE_ACCESS,
            io.binding.key.0,
            0,
        ),
        Action::PreemptAbort(k) => (
            SA_OUT_PREEMPT_ABORT,
            PR_TYPE_EXCLUSIVE_ACCESS,
            io.binding.key.0,
            k,
        ),
        Action::Preempt(k) => (
            SA_OUT_PREEMPT,
            PR_TYPE_EXCLUSIVE_ACCESS,
            io.binding.key.0,
            k,
        ),
    };
    let mut parameter = [0u8; 24];
    parameter[..8].copy_from_slice(&key.to_be_bytes());
    parameter[8..16].copy_from_slice(&sa_key.to_be_bytes());
    let command = cdb::persistent_reserve_out(sa, typ, 24);
    let result = io.device.execute_write(&command, &parameter, PR_TIMEOUT_MS);
    let (ack, error) = match result {
        Ok(r) if r.status == 0 && r.transferred == parameter.len() => (true, None),
        Ok(r) => (
            false,
            Some(if r.status == 0 {
                TapeError::InvalidResponse {
                    expected: parameter.len(),
                    actual: r.transferred,
                }
            } else {
                TapeError::ScsiStatus {
                    device: io.device.identity().into(),
                    status: r.status,
                }
            }),
        ),
        Err(e) => (false, Some(e)),
    };
    if let Some(error) = error {
        let observation = PrStepObservation {
            submission: Submission::Unknown,
            observed: None,
            allowed_after: false,
            device_fenced: false,
            error: Some(error),
        };
        let record_error = a.record(&intent, &observation).err();
        return Ok(recovery(Some(intent), observation, record_error));
    }
    debug_assert!(ack);
    let after = match io.observe(a) {
        Ok(after) => after,
        Err(e) => {
            let observation = PrStepObservation {
                submission: Submission::Acknowledged,
                observed: None,
                allowed_after: false,
                device_fenced: false,
                error: Some(e),
            };
            let record_error = a.record(&intent, &observation).err();
            return Ok(recovery(Some(intent), observation, record_error));
        }
    };
    let allowed = allowed_after(&plan, &after);
    let observation = PrStepObservation {
        submission: Submission::Acknowledged,
        device_fenced: allowed && fenced(&io.binding, &cp.original_before, &after),
        observed: Some(after.clone()),
        allowed_after: allowed,
        error: (!allowed).then(|| reject("PR OUT回读不是原exact允许after")),
    };
    if let Err(e) = a.record(&intent, &observation) {
        return Ok(recovery(Some(intent), observation, Some(e)));
    }
    if !allowed {
        return Ok(recovery(Some(intent), observation, None));
    }
    if let Err(e) = io.live(a) {
        return Ok(recovery(Some(intent), observation, Some(e)));
    }
    Ok(final_progress(&io.binding, &cp, &intent, after))
}

#[cfg(test)]
#[path = "successor_tests.rs"]
mod tests;
