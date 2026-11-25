use super::*;
use crate::scsi::device::ScsiResult;
use crate::scsi::sim::{SimLibrary, SimTransport};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Copy)]
enum ReadFault {
    None,
    ShortKeys,
    ExtraKeys,
    OddKeys,
    ShortReservation,
    DifferentGeneration,
    ChangingPairs,
    FrozenGeneration(u32),
}
#[derive(Clone, Copy)]
enum OutFault {
    None,
    LostAck,
    UnitAttention,
    ShortTransfer,
}
struct Wire<'d> {
    inner: &'d dyn TapeTransport,
    reads: Cell<usize>,
    outs: Cell<usize>,
    read_fault: Cell<ReadFault>,
    out_fault: Cell<OutFault>,
    after_out: RefCell<Option<Box<dyn FnMut() + 'd>>>,
    log: Rc<RefCell<Vec<&'static str>>>,
}
impl<'d> Wire<'d> {
    fn new(inner: &'d dyn TapeTransport) -> Self {
        Self {
            inner,
            reads: Cell::new(0),
            outs: Cell::new(0),
            read_fault: Cell::new(ReadFault::None),
            out_fault: Cell::new(OutFault::None),
            after_out: RefCell::new(None),
            log: Rc::new(RefCell::new(Vec::new())),
        }
    }
}
impl TapeTransport for Wire<'_> {
    fn identity(&self) -> &str {
        self.inner.identity()
    }
    fn execute_no_data(&self, c: &[u8], t: u32) -> Result<ScsiResult> {
        self.inner.execute_no_data(c, t)
    }
    fn execute_read(&self, c: &[u8], b: &mut [u8], t: u32) -> Result<ScsiResult> {
        assert_eq!(c[0], 0x5e);
        assert_eq!(t, 30_000);
        assert_eq!(b.len(), 1024);
        self.log.borrow_mut().push("in");
        let count = self.reads.get();
        self.reads.set(count + 1);
        let mut r = self.inner.execute_read(c, b, t)?;
        let keys = c[1] & 31 == SA_IN_READ_KEYS;
        match self.read_fault.get() {
            ReadFault::ShortKeys if keys => r.transferred = 7,
            ReadFault::ExtraKeys if keys => b[4..8].copy_from_slice(&1024u32.to_be_bytes()),
            ReadFault::OddKeys if keys => {
                b[4..8].copy_from_slice(&1u32.to_be_bytes());
                r.transferred = 9;
            }
            ReadFault::ShortReservation if !keys => r.transferred = 12,
            ReadFault::DifferentGeneration if !keys => {
                let g = u32::from_be_bytes(b[..4].try_into().unwrap());
                b[..4].copy_from_slice(&g.wrapping_add(1).to_be_bytes());
            }
            ReadFault::ChangingPairs => {
                let g = u32::from_be_bytes(b[..4].try_into().unwrap());
                b[..4].copy_from_slice(&g.wrapping_add((count / 2) as u32).to_be_bytes());
            }
            ReadFault::FrozenGeneration(g) => b[..4].copy_from_slice(&g.to_be_bytes()),
            _ => {}
        }
        Ok(r)
    }
    fn execute_write(&self, c: &[u8], b: &[u8], t: u32) -> Result<ScsiResult> {
        assert_eq!(c[0], 0x5f);
        assert_eq!(t, 30_000);
        assert_eq!(b.len(), 24);
        assert_eq!(
            &c[..],
            &cdb::persistent_reserve_out(c[1] & 31, c[2] & 15, 24)[..]
        );
        self.log.borrow_mut().push("out");
        self.outs.set(self.outs.get() + 1);
        if matches!(self.out_fault.get(), OutFault::UnitAttention) {
            return Err(TapeError::ScsiCommand {
                status: 2,
                sense_key: 6,
                asc: 0x2a,
                ascq: 3,
                sense: None,
            });
        }
        let result = self.inner.execute_write(c, b, t)?;
        if let Some(hook) = self.after_out.borrow_mut().as_mut() {
            hook();
        }
        if matches!(self.out_fault.get(), OutFault::LostAck) {
            return Err(TapeError::ScsiHost(7));
        }
        let mut result = result;
        if matches!(self.out_fault.get(), OutFault::ShortTransfer) {
            result.transferred = 23;
        }
        Ok(result)
    }
}

struct Fake<'d> {
    device: &'d dyn TapeTransport,
    outs: &'d Cell<usize>,
    reads: &'d Cell<usize>,
    log: Rc<RefCell<Vec<&'static str>>>,
    binding: SuccessorFenceBinding,
    checkpoint: DeviceFenceCheckpoint,
    live: bool,
    issue_fail: bool,
    record_fail: bool,
    lose_before_out: bool,
    lose_after_issue: bool,
    replace_after_issue: bool,
    lose_after_record: bool,
    issue_hook: Option<Box<dyn FnMut() + 'd>>,
    records: Vec<(Submission, bool, bool, String)>,
}
impl sealed::Sealed for Fake<'_> {}
impl<'d> SuccessorFenceAuthority<'d> for Fake<'d> {
    fn transport(&self) -> &'d dyn TapeTransport {
        self.device
    }
    fn binding(&self) -> &SuccessorFenceBinding {
        &self.binding
    }
    fn check_live(&mut self) -> Result<()> {
        self.log.borrow_mut().push("live");
        if self.live && !(self.lose_before_out && self.reads.get() >= 8) {
            Ok(())
        } else {
            Err(reject("test live permission lost"))
        }
    }
    fn checkpoint(&mut self) -> Result<DeviceFenceCheckpoint> {
        self.log.borrow_mut().push("checkpoint");
        Ok(self.checkpoint.clone())
    }
    fn issue(
        &mut self,
        plan: &PlannedPrOut,
        before: &CompletePrObservation,
    ) -> Result<IssuedPrOut> {
        self.log.borrow_mut().push("issue");
        if self.issue_fail {
            return Err(TapeError::Catalog("test durable issue unavailable".into()));
        }
        assert_eq!(before, &plan.before);
        assert!(self.checkpoint.pending.is_none());
        let issued = IssuedPrOut {
            binding: self.binding.hash(),
            before_hash: before.hash(),
            plan: plan.clone(),
        };
        self.checkpoint.pending = Some(issued.clone());
        if let Some(h) = self.issue_hook.as_mut() {
            h();
        }
        if self.lose_after_issue {
            self.live = false;
        }
        if self.replace_after_issue {
            self.binding.scope[0] ^= 1;
        }
        Ok(issued)
    }
    fn record(&mut self, intent: &IssuedPrOut, o: &PrStepObservation) -> Result<()> {
        self.log.borrow_mut().push("record");
        self.records.push((
            match o.submission {
                Submission::NotSubmitted => Submission::NotSubmitted,
                Submission::Acknowledged => Submission::Acknowledged,
                Submission::Unknown => Submission::Unknown,
                Submission::ObservedOriginal => Submission::ObservedOriginal,
            },
            o.allowed_after,
            o.device_fenced,
            o.error
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        ));
        if self.record_fail {
            return Err(TapeError::Catalog("test record unavailable".into()));
        }
        assert_eq!(self.checkpoint.pending.as_ref(), Some(intent));
        if o.allowed_after {
            self.checkpoint.confirmed.push(ConfirmedStep {
                intent: intent.clone(),
                after: o.observed.clone().unwrap(),
            });
            self.checkpoint.pending = None;
        }
        if self.lose_after_record {
            self.live = false;
        }
        Ok(())
    }
}
fn setup() -> (SimLibrary, SimTransport, SimTransport) {
    let lib = SimLibrary::new(1, 2, 2);
    let old = lib.drive_as(0, 1);
    let new = lib.drive_as(0, 2);
    fence(&old, ReservationKey::new(1, 7)).unwrap();
    (lib, old, new)
}
fn fake<'d>(wire: &'d Wire<'_>, same: bool) -> Fake<'d> {
    let st = read_status(wire.inner).unwrap();
    let mut keys = st.keys;
    keys.sort_unstable();
    let binding = SuccessorFenceBinding {
        transition: [1; 32],
        attempt: [2; 32],
        device_intent: [3; 32],
        scope: [4; 32],
        lu: [5; 32],
        target: [6; 32],
        nexus: [7; 32],
        key: ReservationKey::new(if same { 1 } else { 2 }, 8),
        round: 8,
        original_keys: keys.clone(),
        original_nexus_key: same.then_some(ReservationKey::new(1, 7).0),
    };
    let checkpoint = DeviceFenceCheckpoint {
        binding: binding.hash(),
        original_before: CompletePrObservation {
            generation: st.generation,
            keys,
            holder: st.holder,
        },
        confirmed: Vec::new(),
        pending: None,
    };
    Fake {
        device: wire,
        outs: &wire.outs,
        reads: &wire.reads,
        log: wire.log.clone(),
        binding,
        checkpoint,
        live: true,
        issue_fail: false,
        record_fail: false,
        lose_before_out: false,
        lose_after_issue: false,
        replace_after_issue: false,
        lose_after_record: false,
        issue_hook: None,
        records: Vec::new(),
    }
}
fn success<'d>(a: &mut Fake<'d>) -> DeviceFenced {
    for _ in 0..a.binding.original_keys.len() + 3 {
        let before = a.outs.get();
        let result = advance_device(a).unwrap();
        assert!(a.outs.get() - before <= 1);
        match result {
            DeviceFenceProgress::DeviceFenced(proof) => return proof,
            DeviceFenceProgress::StepObserved { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    panic!("bounded original steps did not finish")
}

#[test]
fn same_and_different_nexus_execute_real_sim_steps_one_out_per_advance() {
    for same in [false, true] {
        let (_lib, old, new) = setup();
        let wire = Wire::new(if same { &old } else { &new });
        let mut a = fake(&wire, same);
        let proof = success(&mut a);
        assert_eq!(proof.binding, a.binding.hash());
        assert_ne!(proof.before.generation, proof.after.generation);
        assert_eq!(proof.after.keys, [a.binding.key.0]);
        assert_eq!(proof.after.holder, Some((a.binding.key.0, 3)));
        assert_eq!(wire.outs.get(), if same { 1 } else { 2 });
        assert!(a.checkpoint.pending.is_none());
        let log = wire.log.borrow();
        for (i, event) in log.iter().enumerate() {
            if *event == "out" {
                assert_eq!(log[i - 1], "live");
                assert!(log[..i].contains(&"issue"));
                assert!(log[i + 1..].contains(&"record"));
            }
        }
    }
}

#[test]
fn exact_nonholder_cleanup_is_one_original_step_at_a_time() {
    let (lib, _old, new) = setup();
    let extra = lib.drive_as(0, 3);
    let k = ReservationKey::new(3, 6).0;
    pr_out(&extra, SA_OUT_REGISTER_IGNORE, 0, 0, k).unwrap();
    let wire = Wire::new(&new);
    let mut a = fake(&wire, false);
    let p = success(&mut a);
    assert_eq!(p.after.keys, [a.binding.key.0]);
    assert_eq!(wire.outs.get(), 3);
    assert!(matches!(a.checkpoint.confirmed[2].intent.plan.action,Action::Preempt(x) if x==k));
}

#[test]
fn lost_ack_is_unknown_and_restart_observes_original_without_out() {
    let (_lib, _old, new) = setup();
    let wire = Wire::new(&new);
    wire.out_fault.set(OutFault::LostAck);
    let mut a = fake(&wire, false);
    let result = advance_device(&mut a).unwrap();
    assert!(matches!(
        result,
        DeviceFenceProgress::RecoveryRequired {
            observation: PrStepObservation {
                submission: Submission::Unknown,
                error: Some(TapeError::ScsiHost(7)),
                ..
            },
            ..
        }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert!(a.checkpoint.pending.is_some());
    let mut resumed = fake_restart(&a);
    wire.out_fault.set(OutFault::None);
    assert!(matches!(
        observe_original(&mut resumed).unwrap(),
        DeviceFenceProgress::StepObserved { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert!(resumed.checkpoint.pending.is_none());
    let p = success(&mut resumed);
    assert_eq!(p.after.keys, [resumed.binding.key.0]);
}
fn fake_restart<'d>(old: &Fake<'d>) -> Fake<'d> {
    Fake {
        device: old.device,
        outs: old.outs,
        reads: old.reads,
        log: old.log.clone(),
        binding: old.binding.clone(),
        checkpoint: old.checkpoint.clone(),
        live: true,
        issue_fail: false,
        record_fail: false,
        lose_before_out: false,
        lose_after_issue: false,
        replace_after_issue: false,
        lose_after_record: false,
        issue_hook: None,
        records: Vec::new(),
    }
}

#[test]
fn pending_cannot_be_reissued_and_observe_never_advances_fresh_work() {
    let (_lib, _old, new) = setup();
    let wire = Wire::new(&new);
    let mut a = fake(&wire, false);
    assert!(matches!(
        observe_original(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { intent: None, .. }
    ));
    assert_eq!(wire.outs.get(), 0);
    wire.out_fault.set(OutFault::UnitAttention);
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert!(matches!(
        observe_original(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
}

#[test]
fn ua_out_is_called_once_and_preserves_original_sense() {
    let (_lib, _old, new) = setup();
    let wire = Wire::new(&new);
    wire.out_fault.set(OutFault::UnitAttention);
    let mut a = fake(&wire, false);
    let r = advance_device(&mut a).unwrap();
    assert!(matches!(
        r,
        DeviceFenceProgress::RecoveryRequired {
            observation: PrStepObservation {
                error: Some(TapeError::ScsiCommand {
                    sense_key: 6,
                    asc: 0x2a,
                    ascq: 3,
                    ..
                }),
                submission: Submission::Unknown,
                ..
            },
            ..
        }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert_eq!(
        read_status(&new).unwrap().keys,
        [ReservationKey::new(1, 7).0]
    );
}

#[test]
fn durable_issue_failure_and_issue_wait_permission_loss_produce_no_out() {
    for failure in [0, 1, 2, 3] {
        let (_lib, _old, new) = setup();
        let wire = Wire::new(&new);
        let mut a = fake(&wire, false);
        a.issue_fail = failure == 0;
        a.lose_after_issue = failure == 1;
        a.replace_after_issue = failure == 2;
        a.lose_before_out = failure == 3;
        let result = advance_device(&mut a);
        if failure == 0 {
            assert!(result.is_err());
            assert!(a.checkpoint.pending.is_none());
        } else {
            assert!(matches!(
                result.unwrap(),
                DeviceFenceProgress::RecoveryRequired {
                    observation: PrStepObservation {
                        submission: Submission::NotSubmitted,
                        ..
                    },
                    ..
                }
            ));
            assert!(a.checkpoint.pending.is_some());
        }
        assert_eq!(wire.outs.get(), 0);
    }
}

#[test]
fn owner_or_registration_change_during_issue_wait_is_not_submitted() {
    let (lib, _old, new) = setup();
    let competitor = lib.drive_as(0, 3);
    let wire = Wire::new(&new);
    let mut a = fake(&wire, false);
    a.issue_hook = Some(Box::new(move || {
        fence(&competitor, ReservationKey::new(3, 9)).unwrap();
    }));
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired {
            observation: PrStepObservation {
                submission: Submission::NotSubmitted,
                ..
            },
            ..
        }
    ));
    assert_eq!(wire.outs.get(), 0);
    assert_eq!(
        read_status(&new).unwrap().keys,
        [ReservationKey::new(3, 9).0]
    );
}

#[test]
fn foreign_after_register_is_unknown_and_not_cleaned_up() {
    let (lib, _old, new) = setup();
    let other = lib.drive_as(0, 3);
    let foreign = 0x1234;
    let wire = Wire::new(&new);
    wire.after_out.replace(Some(Box::new(move || {
        pr_out(&other, SA_OUT_REGISTER_IGNORE, 0, 0, foreign).unwrap();
    })));
    let mut a = fake(&wire, false);
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired {
            observation: PrStepObservation {
                allowed_after: false,
                ..
            },
            ..
        }
    ));
    let state = read_status(&new).unwrap();
    assert!(state.keys.contains(&foreign) && state.keys.contains(&a.binding.key.0));
    assert_eq!(wire.outs.get(), 1);
    assert!(matches!(
        observe_original(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert_eq!(read_status(&new).unwrap(), state);
}

#[test]
fn wrong_same_nexus_mid_stops_without_cleanup() {
    let (_lib, old, _new) = setup();
    let wire = Wire::new(&old);
    wire.after_out.replace(Some(Box::new(|| {
        pr_out(
            &old,
            SA_OUT_REGISTER_IGNORE,
            0,
            0,
            ReservationKey::new(1, 7).0,
        )
        .unwrap();
    })));
    let mut a = fake(&wire, true);
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert_eq!(
        read_status(&old).unwrap().keys,
        [ReservationKey::new(1, 7).0]
    );
}

#[test]
fn failed_record_and_final_permission_loss_do_not_return_fenced() {
    for lose in [false, true] {
        let (_lib, old, _new) = setup();
        let wire = Wire::new(&old);
        let mut a = fake(&wire, true);
        a.record_fail = !lose;
        a.lose_after_record = lose;
        assert!(matches!(
            advance_device(&mut a).unwrap(),
            DeviceFenceProgress::RecoveryRequired {
                record_error: Some(_),
                ..
            }
        ));
        assert_eq!(wire.outs.get(), 1);
        assert_eq!(read_status(&old).unwrap().keys, [a.binding.key.0]);
        if !lose {
            let mut resumed = fake_restart(&a);
            assert!(matches!(
                observe_original(&mut resumed).unwrap(),
                DeviceFenceProgress::DeviceFenced(_)
            ));
            assert_eq!(wire.outs.get(), 1);
        }
    }
}

#[test]
fn malformed_or_unstable_complete_reads_produce_zero_out() {
    for fault in [
        ReadFault::ShortKeys,
        ReadFault::ExtraKeys,
        ReadFault::OddKeys,
        ReadFault::ShortReservation,
        ReadFault::DifferentGeneration,
        ReadFault::ChangingPairs,
    ] {
        let (_lib, _old, new) = setup();
        let wire = Wire::new(&new);
        let mut a = fake(&wire, false);
        wire.read_fault.set(fault);
        assert!(advance_device(&mut a).is_err());
        assert_eq!(wire.outs.get(), 0);
        assert!(wire.reads.get() <= 4);
    }
}

#[test]
fn generation_not_changed_never_confirms_success() {
    let (_lib, old, _new) = setup();
    let wire = Wire::new(&old);
    let mut a = fake(&wire, true);
    wire.read_fault.set(ReadFault::FrozenGeneration(
        a.checkpoint.original_before.generation,
    ));
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert!(matches!(
        observe_original(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired { .. }
    ));
    assert_eq!(wire.outs.get(), 1);
}

#[test]
fn high_nonholder_or_holder_and_foreign_or_unissued_current_key_produce_zero_out() {
    for nonholder in [false, true] {
        let (lib, old, new) = setup();
        let high = ReservationKey::new(3, 9);
        if nonholder {
            pr_out(&lib.drive_as(0, 3), SA_OUT_REGISTER_IGNORE, 0, 0, high.0).unwrap();
        } else {
            fence(&old, high).unwrap();
        }
        let wire = Wire::new(&new);
        let mut a = fake(&wire, false);
        assert!(matches!(
            advance_device(&mut a).unwrap(),
            DeviceFenceProgress::Superseded { .. }
        ));
        assert_eq!(wire.outs.get(), 0);
    }
    for current in [false, true] {
        let (lib, _old, new) = setup();
        let wire = Wire::new(&new);
        let mut a = fake(&wire, false);
        let foreign = if current { a.binding.key.0 } else { 0x1234 };
        pr_out(&lib.drive_as(0, 3), SA_OUT_REGISTER_IGNORE, 0, 0, foreign).unwrap();
        assert!(matches!(
            advance_device(&mut a).unwrap(),
            DeviceFenceProgress::Rejected { .. }
        ));
        assert_eq!(wire.outs.get(), 0);
    }
}

#[test]
fn binding_round_nexus_and_checkpoint_cannot_be_forged_by_mask_or_progress() {
    for bad in [0, 1, 2, 3] {
        let (_lib, _old, new) = setup();
        let wire = Wire::new(&new);
        let mut a = fake(&wire, false);
        match bad {
            0 => a.binding.round = 1 << 48,
            1 => a.binding.original_nexus_key = Some(0x5678),
            2 => a.binding.lu = [0; 32],
            _ => a.checkpoint.binding[0] ^= 1,
        }
        assert!(advance_device(&mut a).is_err());
        assert_eq!(wire.outs.get(), 0);
    }
}

#[test]
fn unsupported_pr_is_not_fenced_and_has_zero_out() {
    let lib = SimLibrary::new(1, 2, 1);
    let changer = lib.changer();
    let wire = Wire::new(&changer);
    let mut a = {
        let drive = lib.drive(0);
        let st = read_status(&drive).unwrap();
        let binding = SuccessorFenceBinding {
            transition: [1; 32],
            attempt: [2; 32],
            device_intent: [3; 32],
            scope: [4; 32],
            lu: [5; 32],
            target: [6; 32],
            nexus: [7; 32],
            key: ReservationKey::new(2, 8),
            round: 8,
            original_keys: vec![],
            original_nexus_key: None,
        };
        let cp = DeviceFenceCheckpoint {
            binding: binding.hash(),
            original_before: CompletePrObservation {
                generation: st.generation,
                keys: vec![],
                holder: None,
            },
            confirmed: vec![],
            pending: None,
        };
        Fake {
            device: &wire,
            outs: &wire.outs,
            reads: &wire.reads,
            log: wire.log.clone(),
            binding,
            checkpoint: cp,
            live: true,
            issue_fail: false,
            record_fail: false,
            lose_before_out: false,
            lose_after_issue: false,
            replace_after_issue: false,
            lose_after_record: false,
            issue_hook: None,
            records: vec![],
        }
    };
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::Unsupported
    ));
    assert_eq!(wire.outs.get(), 0);
}

#[test]
fn two_devices_never_create_a_group_result_or_release_completed_device() {
    for unknown in [false, true] {
        let lib = SimLibrary::new(2, 2, 1);
        let first = lib.drive_as(0, 2);
        let second = lib.drive_as(1, 2);
        let one = Wire::new(&first);
        let two = Wire::new(&second);
        let mut a = fake(&one, false);
        let proof = success(&mut a);
        let mut b = fake(&two, false);
        if unknown {
            two.out_fault.set(OutFault::LostAck);
            assert!(matches!(
                advance_device(&mut b).unwrap(),
                DeviceFenceProgress::RecoveryRequired { .. }
            ));
            assert_eq!(two.outs.get(), 1);
            assert_eq!(read_status(&second).unwrap().keys, [b.binding.key.0]);
        } else {
            b.issue_fail = true;
            assert!(advance_device(&mut b).is_err());
            assert_eq!(two.outs.get(), 0);
        }
        assert_eq!(read_status(&first).unwrap().keys, proof.after.keys);
        assert_eq!(one.outs.get(), 2);
    }
}

#[test]
fn strict_decoder_rejects_duplicates_foreign_holder_and_unknown_type_or_scope() {
    let keys = |raw: &[u64]| {
        let mut bytes = 7u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&((raw.len() * 8) as u32).to_be_bytes());
        for k in raw {
            bytes.extend_from_slice(&k.to_be_bytes());
        }
        bytes
    };
    let mut holder = 7u32.to_be_bytes().to_vec();
    holder.extend_from_slice(&16u32.to_be_bytes());
    holder.extend_from_slice(&5u64.to_be_bytes());
    holder.extend_from_slice(&[0, 0, 0, 0, 0, 3, 0, 0]);
    assert!(decode_pair(&keys(&[5, 5]), &holder).is_err());
    assert!(decode_pair(&keys(&[6]), &holder).is_err());
    for kind in [1, 0x13, 0] {
        let mut bad = holder.clone();
        bad[21] = kind;
        assert!(decode_pair(&keys(&[5]), &bad).is_err());
    }
    assert!(decode_pair(&keys(&[5]), &holder).is_ok());
}

#[test]
fn short_out_transfer_after_real_effect_remains_unknown_without_retry() {
    let (_lib, old, _new) = setup();
    let wire = Wire::new(&old);
    wire.out_fault.set(OutFault::ShortTransfer);
    let mut a = fake(&wire, true);
    assert!(matches!(
        advance_device(&mut a).unwrap(),
        DeviceFenceProgress::RecoveryRequired {
            observation: PrStepObservation {
                submission: Submission::Unknown,
                error: Some(TapeError::InvalidResponse {
                    expected: 24,
                    actual: 23
                }),
                ..
            },
            ..
        }
    ));
    assert_eq!(wire.outs.get(), 1);
    assert_eq!(read_status(&old).unwrap().keys, [a.binding.key.0]);
    wire.out_fault.set(OutFault::None);
    assert!(matches!(
        observe_original(&mut a).unwrap(),
        DeviceFenceProgress::DeviceFenced(_)
    ));
    assert_eq!(wire.outs.get(), 1);
}

#[test]
fn even_a_foreign_key_in_fake_proof_is_rejected_not_whitelisted() {
    let lib = SimLibrary::new(1, 2, 1);
    let old = lib.drive_as(0, 1);
    let new = lib.drive_as(0, 2);
    fence(&old, ReservationKey(0x4000_0000_0a83_0948)).unwrap();
    let wire = Wire::new(&new);
    let mut a = fake(&wire, false);
    assert!(advance_device(&mut a).is_err());
    assert_eq!(wire.outs.get(), 0);
}
