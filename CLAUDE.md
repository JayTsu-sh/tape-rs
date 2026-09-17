# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

CLI tool that controls an IBM TS4300 (labelled `IBM 3573-TL`) tape library by sending raw SCSI CDBs through the Linux `SG_IO` ioctl — **not** via LTFS mount. The target hardware is attached to a remote dev VM `10.128.54.118` (via an Emulex LPe16002B FC HBA); on that box the changer is `/dev/sg2`, the LTO-8 drives are `/dev/sg1` (ULT3580-TD8) and `/dev/sg3` (ULT3580-HH8).

**Hardware availability (2026-09-17).** The TS4300 above is currently not available. The only device is the Holo-VTL simulator in the PVE lab, where tape-rs has its own logical library (TAPERS) reached from the `tsclient` VM over iSCSI. "Hardware" results in this file and in the tests mean Holo unless stated otherwise. Holo has had several SSC/MAM deviations that were fixed in its source; treat anything that depends on drive firmware as unverified. IBM reference LTFS 2.4.8.3 (built on `tsclient` under `/opt/ltfs-ref`) and the lab's Spectrum Archive EE cluster are the oracles for checking both tape-rs and Holo.

**Linux-only by design.** `scsi/device.rs` uses `std::os::unix::io` and `nix::ioctl_readwrite_bad!`; it will not compile on Windows/macOS. The local checkout on Windows is for editing only — all builds and all hardware verification happen on the remote VM.

## Remote dev workflow

There is no git remote configured for this project; code is shipped via scp. The canonical loop is: **edit locally → scp to `root@10.128.54.118:/root/jay/tape-rs/` → `cargo build --release` there → exercise against real hardware**. Key auth is already set up; no password needed.

```bash
# push a single file
scp src/scsi/device.rs root@10.128.54.118:/root/jay/tape-rs/src/scsi/

# push whole tree (skip target/)
scp -r Cargo.toml src root@10.128.54.118:/root/jay/tape-rs/

# build + smoke test
ssh root@10.128.54.118 'cd /root/jay/tape-rs && source ~/.cargo/env && cargo build --release \
  && ./target/release/tape-rs inquiry /dev/sg2 \
  && ./target/release/tape-rs inventory --device /dev/sg2'
```

Set `RUST_LOG=debug` for the `execute()`-level CDB/sense traces.

The repeatable generate → push → remote-compile → debug loop is captured as a project-scoped skill at `.claude/skills/tape-rs-remote-dev/SKILL.md`. Invoke it by name (`tape-rs-remote-dev`) whenever iterating on SCSI / SG_IO code.

## SCSI standards this project implements

SCSI is T10 (INCITS), not IETF. Relevant specs:
- **SPC-5** — `INQUIRY (0x12)`, `TEST UNIT READY (0x00)`, `MODE SENSE(10) (0x5A)`, `REQUEST SENSE (0x03)`, sense data formats (0x70/0x71 fixed, 0x72/0x73 descriptor).
- **SSC-5** — tape stream commands: `READ(6) (0x08)`, `WRITE(6) (0x0A)`, `REWIND (0x01)`, `WRITE FILEMARKS(6) (0x10)`, `SPACE(6) (0x11)`, `LOAD/UNLOAD (0x1B)`, `READ POSITION (0x34)`.
- **SMC-3** — media changer: `MOVE MEDIUM (0xA5)`, `READ ELEMENT STATUS (0xB8)`, `INITIALIZE ELEMENT STATUS (0x07)`, `MODE SENSE` page `0x1D` (Element Address Assignment).

The TS4300 presents both an SMC device (changer) and one or two SSC devices (LTO drives) on the same Fibre Channel/SAS bus.

## Build, Run

```bash
cargo build --release                      # must run on Linux (the remote VM)
cargo run -- <subcommand> [args]
```

CLI defaults: `--device /dev/sg2` for changer ops, `/dev/sg1` for drive ops. All user-facing strings (help, command descriptions) are in Chinese — keep that consistent.

## Critical gotcha: `SG_IO` ioctl number

`SG_IO` in `<scsi/sg.h>` is defined as the literal `0x2285`. It does **not** follow the modern `_IO*` encoding. Do not use `nix::ioctl_readwrite!` (that encodes `sizeof(SgIoHdr)` and yields `0xC058_5385`, producing `EINVAL`) and do not use `nix::request_code_none!(b'S', 0x85)` either (that yields `0x5385`, which the kernel silently routes to a different handler — the observable symptom is that 84 bytes of an HBA description string get written into the `sg_io_hdr` struct itself, the dxfer buffer stays zero, and `host_status` comes back as nonsense like `0x7562`).

The correct binding is `ioctl_readwrite_bad!(sg_io, 0x2285u64, SgIoHdr);` (done in `src/scsi/sg_io.rs`). If you ever port the SG_IO call elsewhere, copy the literal number verbatim.

## Architecture

Four-layer stack, low → high:

1. **`scsi/sg_io.rs`** — `SgIoHdr` `#[repr(C)]` mirror of `sg_io_hdr_t` from `<scsi/sg.h>` (88 bytes on x86_64 with natural 4-byte padding before `usr_ptr`). Only the raw ioctl binding lives here.

2. **`scsi/cdb.rs`** — Pure functions that return fixed-size CDB byte arrays (`inquiry`, `mode_sense_10`, `read_element_status`, `move_medium`, `read_6`, `write_6`, `space`, …). Opcodes live in `cdb::opcode`. Keep these allocation-free and side-effect-free.

3. **`scsi/device.rs` + `scsi/sense.rs`** — `ScsiDevice` owns the fd to a `/dev/sg*` node and exposes `execute{,_no_data,_read,_write}`. It builds the `SgIoHdr`, invokes the ioctl, inspects `host_status`/`driver_status`, and parses sense data. `CHECK CONDITION` with a non-OK sense key becomes `TapeError::ScsiCommand { status, sense_key, asc, ascq }`. `SenseInfo::from_bytes` handles both fixed (0x70/0x71) and descriptor (0x72/0x73) formats.

4. **Domain wrappers** — `changer::commands::MediumChanger` and `tape::commands::TapeDrive`. They borrow a `&ScsiDevice` (lifetime `'a`), compose CDB builders, and translate SCSI responses into domain structs.

### Changer flow (non-obvious)

`MediumChanger` is **stateful**: `load_address_map()` must be called before `read_all_status()` or `move_medium()`, otherwise those methods return `TapeError::NotReady`. The address map comes from `MODE SENSE(10)` page `0x1D` (Element Address Assignment) and gives the base addresses + counts for Transport / Storage / I-E / Data Transfer elements. All CLI slot/drive numbers in `main.rs` are 1-based (slots) or 0-based (drives) offsets layered on top of those bases — see `cmd_load` / `cmd_unload` / `cmd_move` for the arithmetic.

On the real TS4300 we see: Transport `0x0000×1`, Drives `0x0001×2`, I/E `0x0065×5`, Storage `0x03E9×35`. Drives and I/E slots live in different SCSI address spaces — when moving, always recompute the absolute address from the map.

`read_all_status()` calls `READ ELEMENT STATUS` with `voltag=true` across all element types in one shot, then `parse_element_status_data` walks the response: 8-byte header → one or more Element Status Pages, each with its own 8-byte header, fixed `desc_len`, and a run of descriptors. When `desc_len >= 48` and the PVolTag flag is set, bytes `offset+12..offset+44` hold the barcode. Empty slots return 32 NUL bytes there — `String::from_utf8_lossy(...).trim()` does not strip NULs, so empty barcodes currently render as 32 `^@` chars; if that matters, filter with `tag_bytes.iter().any(|&b| b != 0)` before constructing the string.

### Transport seam and simulator

`scsi/transport.rs` defines `TapeTransport` (execute_no_data / execute_read / execute_write / identity). `ScsiDevice` implements it over `SG_IO`; `scsi/sim.rs` implements it as an in-memory library (`SimLibrary::changer()` / `SimLibrary::drive(i)`) that parses the same CDB bytes. `TapeDrive`, `MediumChanger`, `Mam`, `LtfsVolume`, `mkltfs`, `standard_inquiry` and `sync_from_device` all take `&dyn TapeTransport`, so `&ScsiDevice` call sites coerce without change. Status classification is shared via `device::finish_command`, so filemark detection is `Ok(transferred = 0)` and BLANK CHECK is `ScsiCommand { sense_key: 0x08 }` on both. The simulator models records/filemarks per partition, buffered-vs-flushed objects (`SimLibrary::power_cut()` drops unflushed), and per-opcode fault injection (`Fault { opcode, action, after, remaining }`). Integration tests in `tests/` run the full LTFS path on it; a simulator pass never substitutes for hardware verification.

### Sense flags and UNIT ATTENTION (learned on Holo-VTL)

`SenseInfo` parses the fixed-format SSC flags (`filemark`, `eom`, `ili`) and the INFORMATION field (only when VALID). `TapeDrive::read_block` decides by those, not by the sg residual: FILEMARK → 0 bytes; ILI with positive INFORMATION → `requested − information`. An unpatched Holo-VTL reports a filemark with only the FM bit and no residual, so relying on `transferred == 0` reads a filemark as a full buffer. `transport::retry_unit_attention` retries a bounded number of UNIT ATTENTIONs and is applied **only to position-independent commands** (TUR, REWIND, LOCATE, READ POSITION, LOAD/UNLOAD, MODE SELECT, FORMAT, MAM, all changer commands). READ/WRITE/WRITE FILEMARKS/SPACE/ERASE must never auto-retry: a UA 0x29 means position may be lost, so they surface the error and commit freezing plus recovery take over. `SimQuirks::holo_vtl()` reproduces the unpatched Holo behaviour (filemark without residual, ILLEGAL REQUEST for SPACE at BOP/EOD) and the simulator queues UAs after FORMAT (2A/01) and after a medium is moved into a drive (28/00).

### Volume recovery on mount (D02)

`ltfs/recovery.rs` runs on every `LtfsVolume::mount`: SPACE to EOD on each partition, step back over filemarks region by region (regions alternate index / data), validate the self pointer of each candidate (`volumeuuid`, partition, `startblock`), classify the tail after the last valid index (T0 Complete / T1 UnindexedData / T2 TruncatedIndex / T3 FakeIndex / T4 Unknown, plus EmptyData for a data partition that has only the label), verify the DP `previousgenerationlocation` chain, and check the append position (`EOD == closing FM + 1`, then LOCATE + READ POSITION). Before stepping back it tries the LTFS 2.5.1 §10.3 fast path: read the medium VCR (MAM 0x0009, drive-maintained) and each partition's VCI (MAM 0x080C, layout per Table 17/18: VCR length, VCR, generation, block, ACSI); a VCI whose VCR equals the current VCR gives that partition's last-index block as a hint, which is still read and validated (`hint_used` in the report). Any write changes the VCR, so a stale VCI simply falls back to the standard search. A fresh VCI is still not enough: the hinted index is accepted only if its closing filemark is immediately followed by EOD, otherwise the standard search runs — this keeps a newer committed index from being missed when the VCI was not updated (barrier/VCI-stage failure) or on a device whose VCR does not change on writes (seen on an unpatched Holo-VTL). The view is the DP last index (may be newer than IP; IP lag is recorded as `ip_debt`). Non-complete tails mount read-only: `append_file`/`commit` return `TapeError::RecoveryRestricted`. No automatic repair, no test writes. `mkltfs` writes a generation-1 index on the data partition too (LTFS 2.5.1 §5.3), and `commit` back-points to the previous DP Full (§5.4.3). Both end with the §10.3 sequence: write index → `WRITE FILEMARKS(0)` flush → read VCR → write a VCI to every partition (each pointing at its own last index).

### Index fidelity: xattrs, symlinks, hashes

`ltfs/index.rs` round-trips `<extendedattributes>` (files and directories; `Xattr { key, value, base64 }` keeps the text verbatim), `<symlink>` and `<volumelockstate>`. `commit` rewrites the whole index, so anything the parser does not know would be lost: unknown element names are collected in `LtfsIndex::unknown_elements`, and a non-empty list mounts the volume read-only with the names in `restricted_reason`. When adding support for a new element, add it to `KNOWN_ELEMENTS` **and** to the writer in the same change. `read_file_to_writer` follows one symlink level inside the volume (IBM EE layout: data in `.LTFSEE_DATA/<id>`, user paths are symlinks). `append_file` records content hashes per LTFS 2.5.1 Annex F.3: `HashPolicy` defaults to `ltfs.hash.sha256sum` only; `md5sum` is opt-in because computing both on one thread (about 290 MiB/s measured) is slower than LTO-8. `verify_file` / `ltfs-verify` prefer sha256, then md5. `ltfs-write --xattr KEY=VALUE` (repeatable) and `--md5` expose this on the CLI; keys are stored without the `user.` prefix, which IBM LTFS adds on Linux (index key `origin` shows up as `user.origin`). VCRs are compared numerically (`vcr_matches`) and written 8 bytes wide in the VCI, matching IBM LTFS byte for byte (`decodes_vci_written_by_ibm_ltfs`).

### On-tape format rules verified against IBM LTFS 2.4.8.3

The reference implementation (same build number as the EE cluster's LE) rejected tapes written by earlier tape-rs. These rules are now pinned by `on_tape_layout_matches_what_ibm_ltfs_requires`; do not regress them:

- Timestamps come from `label::ltfs_time_now()` only: `YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ`, nine fractional digits are mandatory.
- Both partition labels must be identical except `<location>`; mkltfs takes `formattime` once.
- An Index Construct is filemark + index + filemark (§5.2.3). After the label construct (blocks 0–3) block 4 is the opening filemark and the first index is at block 5 (`FIRST_INDEX_BLOCK`). `commit` rewrites the IP from block 4 (`P1_DATA_START` is the content-area start on both partitions): filemark, index, filemark.
- `fileuid` 0 is reserved; `ensure_dir` allocates from `highestfileuid`.
- The IP index back pointer points at the DP index of the **same** generation (definition of a consistent volume). Pointing at the previous generation makes IBM LTFS rewrite the IP on mount.
- Recovery accepts IBM's "IP ahead" state (`RecoveryReport::ip_ahead()`: IP generation newer and its back pointer equals the DP last index) and uses the IP index as the view; only a mismatching back pointer is a conflict. Because commit rewrites the whole IP, a view with any extent on the index partition mounts read-only, and with no IP data a complete DP tail is enough to allow appends regardless of the IP tail.

### Device-level fencing (persistent reservations)

Raft decides who may execute; the drive does not know about terms. `scsi/reservation.rs` makes the device reject a stale executor. `fence(dev, ReservationKey::new(node, round))` reads the registrations, registers this round's key, re-reads (the holder may be this same initiator with an older key, in which case the reservation already follows the new key), preempts-and-aborts or reserves with Exclusive Access, removes stale registrations, and only reports `Fenced` after a read-back shows the registration table holds exactly this key, the holder is this key and the PR generation moved. `Unsupported` means the device rejects PR commands; any transport error is an `Err` and must be treated as "not confirmed, do not take over". **Never re-register or preempt outside `fence`**: IBM LTFS preempts back on conflict because it uses PR for multipath, which defeats fencing. `ownership_lost(&err)` is true for RESERVATION CONFLICT, UNIT ATTENTION 2A/03–05, 29/xx and `OwnershipLost`; on any of them the caller stops. `retry_unit_attention` deliberately does not retry 2A/03–05. `LtfsVolume::set_reservation_guard(Some(key))` checks the holder before the first byte of an append, before a commit touches the tape (`guard_pre`) and before the barrier (`guard`); the device cannot reject a stale instance on the same initiator, so these checks are the only defence there. The simulator models initiators (`SimLibrary::drive_as(idx, initiator)`), Exclusive Access, per-initiator 2A/03 and `reset_drive` (reservations lost, 29/00). CLI: `pr-status`, `pr-fence`, `pr-release`, `ltfs-write --guard NODE:ROUND`. Verified on the lab's Linux LIO target with two initiators; real LTO-8 / TS4300 firmware behaviour is not yet verified.

### Closing the tail after a takeover

A takeover in the middle of a write leaves the data partition with a T1 (unindexed data) or T2 (truncated index) tail, which mounts read-only. `LtfsVolume::close_tail(TailPolicy)` makes the volume consistent and writable again by **appending** an index construct at EOD that points back at the last valid DP index; it is a commit with no new files, so the IP and both VCIs are rewritten as usual. Nothing is overwritten or truncated. It refuses unless exclusivity is proven (a reservation guard key is set and `verify_holder` passes) and refuses every other restriction: T3/T4 tails, broken chain, partition conflict, view taken from the IP, unknown index elements, locked volume, file data on the IP, or a LOCATE/READ POSITION mismatch at EOD. `TailPolicy::Discard` (default) leaves the blocks unreferenced and records the range in the root xattr `tapers.abandonedBlocks`; `TailPolicy::Salvage` reads up to the first filemark and registers the bytes as a read-only file under `_ltfs_lostandfound/`. `ltfs-takeover --node N --round R [--salvage] [--report-only]` runs fence → mount → close when needed. IBM LTFS 2.4.8.3 `ltfsck` accepts the result. `tests/sim_close_tail.rs` covers it.

### ltfsd skeleton (`src/daemon/`, `src/bin/ltfsd.rs`)

Three-node Raft (raft-rs 0.7, `protobuf-codec`, no protoc needed) on top of the fencing and recovery pieces. Threads: one Raft loop (`node.rs`), one device executor (`executor.rs`, the only code that touches devices; device work can take minutes and never runs in the Raft loop), TCP send/receive threads (`net.rs`, best effort, Raft retries). `store.rs` writes term, vote, commit and log entries through to SQLite and uses `MemStorage` as a read cache; there are no snapshots yet. `state.rs` is the replicated control state with three commands. **The execution round is the log index at which a leader's `Takeover` entry commits**, and that round goes into the reservation key. Sequence: elected → `Takeover` commits → executor fences every device of the library and reads back → `Fenced{round}` commits (if a newer `Takeover` was applied first the round is void) → mount, close the tail if needed, serve. On `ownership_lost` or a failed fence the executor stops, the node commits `Abandoned` and transfers leadership, then refuses to take over for a cooldown; it never re-reserves on its own. Devices are located by VPD 0x80 serial, never by `/dev/sgN`. `status.json` is a node's last self-report, not a cluster fact (a paused node's file is stale). `files.rs` is the file service shared by HTTP threads and the executor: after recovery the executor opens it with a `core::volume_state::VolumeState` built from the index (instance id = round); uploads go admit → ingest → complete → freeze → tape commit → publish, and "complete + enqueue" and "drain queue + freeze" happen under one lock so the frozen batch covers exactly the files that get written. Concurrent uploads are batched into one mount and one commit. When the executor changes, staged-only tasks become `failed` (re-upload to the new leader) and tasks already in a batch become `indeterminate` (ask the new leader for the path); any commit failure ends the round. `http.rs` is a minimal hand-written HTTP/1.1 interface (`PUT/GET /files/<path>`, `/stat`, `/tasks`, `/list`, `/cluster`); 202 means staged only, archive success is a tape commit (`?wait=1` → 201); non-serving nodes answer 503 with the leader URL. `tests/ltfsd_cluster.rs` runs three nodes in one process over channels against the simulator. The failover test keeps uploading to the partitioned leader until it stops, and asserts that every upload reported as committed is visible and intact on the new leader. `.cargo/config.toml` pins C code to baseline x86-64 because a host gcc defaulting to `-march=x86-64-v3` made bundled SQLite crash with an illegal instruction on the lab VMs.

### Commit semantics (S3–S5)

`LtfsVolume` keeps two indexes: `working` (includes uncommitted appends; `pending_files()`) and `index` (the published committed view served by `list`/`read_file_to_writer`/`index()`). `commit` runs the device steps in stages (`locate`, `opening_fm`, `index_records`, `closing_fm`, `index_partition` (filemark + index + filemark from block 4), `barrier`, `vci`) and on any failure returns `TapeError::CommitFailed { stage, .. }`, freezes the volume (`writable() == false`, further `append_file`/`commit` return `RecoveryRestricted`) and leaves the published view untouched. A failed commit is "result indeterminate": the caller must remount and let the recovery protocol decide — the DP index may be complete (stages `index_partition` and later) even though the call failed. `unmount` no longer auto-commits a frozen volume. `tests/sim_commit_faults.rs` covers each stage plus the 04 "threading scenario" (files 甲/乙 under three fault branches recovered by a fresh instance).

### Core volume state (D03)

`core/volume_state.rs` is the first ltfsd-core module and does no device I/O. `VolumeState` is a per-volume sequencer over an immutable `VolumeRoot` (epoch, instance, committed directory tree, generation, pending set, ledger, sessions, commit slot, capability). Readers `load()` an `Arc` of the whole root; every update builds a new root and swaps one pointer; replaced roots go to `retired` and `reclaim()` frees those with no readers. The committed view is a directory tree updated by path copying, so a publish allocates in proportion to changed paths × depth, not tree size (`PublishStats::last_publish_nodes_copied`). Flow: `admit` (reservation + in_progress entry) → `ingest` → `complete` → `freeze` (S2, one batch per volume) → caller writes the index and proves S4 → `publish(batch, S4Evidence)` (CP2 evidence check, CP3 read current root, CP4 diff by identity: only covered ranges leave pending, prefix covers keep the entry and its earliest-registered time, reservations release only their unconsumed remainder, calibration after freeze is not double-counted, CP5 swap, CP6 notify exact waiters). `fail_next_publish()` exercises the pre-built poisoned root (O(1), no allocation). Session deadlines use a caller-supplied monotonic clock; `tick(now, observed_epoch)` ignores sessions renewed after `observed_epoch`. `tests/volume_state.rs` covers PC01–PC06 and PR01/04/06.

### Tape drive flow

`TapeDrive::read_file` terminates on either `BLANK CHECK` (sense key `0x08`) or the filemark-detected sense signature `sense_key=0x00, asc=0x00, ascq=0x01` — both are treated as normal EOF, not errors. `write_file` writes a single filemark after the last block. Both use **variable-length** `READ(6)`/`WRITE(6)` (the `fixed` bool in the CDB is `false`); if you add fixed-block support, note that `transfer_len` then counts **blocks**, not bytes.

### Timeouts

Timeouts are baked into each call site (ms): INQUIRY 10s, MODE SENSE 30s, READ ELEMENT STATUS 60s, MOVE MEDIUM 300s, INITIALIZE ELEMENT STATUS 600s, REWIND/LOAD/UNLOAD 300s, READ/WRITE block 120s. Mechanical operations need the long ones — don't shrink them without testing on real hardware.

## Conventions

- Errors: every fallible function returns `crate::error::Result<T>` (alias for `Result<T, TapeError>`). `nix::errno::Errno` and `std::io::Error` have `From` impls; don't wrap them manually.
- Logging: `info!` for user-visible milestones, `debug!` for per-CDB detail. User-facing output in `main.rs` uses `println!` directly.
- Hardware scenarios: `examples/hw_tapers.rs` runs the D02 recovery scenarios on a real drive. It is destructive (mkltfs per scenario), requires `TAPE_RS_HW_DRIVE` and `TAPE_RS_HW_SERIAL`, and refuses to run unless the drive's VPD 0x80 serial matches. It is an example rather than a `#[test]` because libtest needs GLIBC_2.39 (`pidfd_spawnp`) and the lab hosts run glibc 2.34.
- Tests: unit tests live next to the code; `tests/sim_ltfs.rs` and `tests/sim_recovery.rs` exercise the full stack on `SimTransport` and run anywhere (`cargo test`). Anything touching `/dev/sg*` must stay behind an env var or `#[ignore]` — CI and non-hardware hosts can't run it.
