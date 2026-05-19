# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

CLI tool that controls an IBM TS4300 (labelled `IBM 3573-TL`) tape library by sending raw SCSI CDBs through the Linux `SG_IO` ioctl — **not** via LTFS mount. The target hardware is attached to a remote dev VM `10.128.54.118` (via an Emulex LPe16002B FC HBA); on that box the changer is `/dev/sg2`, the LTO-8 drives are `/dev/sg1` (ULT3580-TD8) and `/dev/sg3` (ULT3580-HH8).

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

### Tape drive flow

`TapeDrive::read_file` terminates on either `BLANK CHECK` (sense key `0x08`) or the filemark-detected sense signature `sense_key=0x00, asc=0x00, ascq=0x01` — both are treated as normal EOF, not errors. `write_file` writes a single filemark after the last block. Both use **variable-length** `READ(6)`/`WRITE(6)` (the `fixed` bool in the CDB is `false`); if you add fixed-block support, note that `transfer_len` then counts **blocks**, not bytes.

### Timeouts

Timeouts are baked into each call site (ms): INQUIRY 10s, MODE SENSE 30s, READ ELEMENT STATUS 60s, MOVE MEDIUM 300s, INITIALIZE ELEMENT STATUS 600s, REWIND/LOAD/UNLOAD 300s, READ/WRITE block 120s. Mechanical operations need the long ones — don't shrink them without testing on real hardware.

## Conventions

- Errors: every fallible function returns `crate::error::Result<T>` (alias for `Result<T, TapeError>`). `nix::errno::Errno` and `std::io::Error` have `From` impls; don't wrap them manually.
- Logging: `info!` for user-visible milestones, `debug!` for per-CDB detail. User-facing output in `main.rs` uses `println!` directly.
- No tests exist. If you add them, gate anything touching `/dev/sg*` behind an env var or `#[ignore]` — CI and non-hardware hosts can't run them.
