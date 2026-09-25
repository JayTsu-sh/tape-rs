# Claude handoff: LTFS interoperability

Last updated: 2026-09-25. Branch: `ltfs-recovery-ibm-interop`. GitHub remote: `origin` (`JayTsu-sh/tape-rs`). Read this file before continuing Holo/LE, FUSE, LTFS recovery, or tape reclaim work. `AGENTS.md` is the governing repository and hardware-safety instruction.

## Current state

The branch contains the LTFS 2.5.1 full/incremental index work, recovery/checkpoint behavior, client and FUSE namespace operations (directories, rename, xattrs, symlink read/create), and Holo/LE research. Read the chronology in `.scratch/ltfs-ha/handoff-to-codex.md`; domain behavior and error contracts are in `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Individual `holo-*.md` reports hold procedures and evidence. Probe scripts live in `.scratch/ltfs-ha/probes/` and have explicit environment/path gates; check their assertions and target path before running them.

Current formal cluster is on the symlink-create build, which **does not yet contain the symlink reclaim fix**:

- Branch formal deploy path on `.71/.72/.74`: `/home/rocky/tape-rs-symlink-create-prod-20260925/ltfsd`, SHA256 `45ca052da5867aafcf5e77f15ad7a3b728041ed79c08e0b392c25f87cada405d`.
- FUSE on `.73`: `/home/rocky/tape-rs-symlink-create-prod-20260925/tape-fuse`, SHA256 `cc8b3c3013819d9122e3345e96bfd266ce207af90e8057c304ae21dc3494c1bb`.
- Formal startup entry remains `/home/rocky/tape-rs-ltfs25-20260924/start.sh N`; it now points to the create build and retains original ports, data dir, serials, and cluster settings.
- Last verified formal state: node2, term40/round350; `TS1001L08` generation96, DP/IP `Complete`, 19 MiB free. Generation95→96 was the planned shutdown checkpoint. Three-node directory catalog had 42 rows, SHA256 `89691e412db63fe43f18bd3b2ad33ff2f9f12337748f8ca92f5885282c6211c8`; 9 original files retained their baseline lengths and hashes. EE nodes were available, no active tasks, and `/gpfs/tapers-del/f3` retained its baseline hash.
- Previous version bundles in each formal node's `/home/rocky/tape-rs-symlink-create-prod-20260925/pre-upgrade.tgz` are mode 0600. They contain closed Raft/catalog data and prior executable/start script. Do not restore their Raft database over the live cluster; binaries/start scripts are separate rollback material.

An isolated tape-reclaim fix was implemented and tested after the formal create build. It has **not been deployed**:

- Repro: one dangling symlink made reclaim abandon with `符号链接目标不可读`; ordinary reclaim incorrectly read through the link.
- Fix: `copy_one` routes symlink records through `FileService::relocate_symlink` and `LtfsVolume::add_symlink_with_metadata`. It copies target and node metadata without reading target or extents, allocates a destination-volume UID, and leaves regular-file SHA checks and both source-format gates unchanged.
- Validation: `cargo test --all-features` passed 206 tests, 7 ignored; `cargo check --all-targets` and `cargo clippy --all-targets --all-features` completed with existing warnings. All 6 symlink kinds passed a simulated cluster reclaim, destination remount, and takeover. A two-cartridge isolated Holo test reclaimed `SR2501L8`→`SR2502L8`; dangling/loop/directory/absolute/chained/Unicode targets, binary xattrs, mtime, readonly flag, target bytes, mmap, fresh-cache readback, and node2 takeover passed. Actual LTFS Full-index XML confirmed six targets and timestamps, with no extents. Source test cartridge was reformatted as part of reclaim.
- Full report: `.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md`. Evidence prefix: `.scratch/ltfs-ha/probes/results/symlink-reclaim-`.

## Lab state and next step

Two new 512 MiB Holo virtual cartridges were created specifically for the isolated test and remain in TAPERS storage slots 8 and 9:

- `SR2501L8`: source, reclaimed/reformatted, Full generation2.
- `SR2502L8`: destination with moved files, Full generation4.
- They belong only to isolated pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and are not assigned to the formal pool. Keep them separate from formal inventory.
- Isolated daemons/FUSE were stopped. Original `TS1001L08` is back in drive0 and formal service was restored. LE copy was not used in this reclaim test. Saved Holo cartridge copies are under `/home/rocky/holo-symlink-reclaim-20260925/`.

Next planned task is to deploy the reclaim fix to the formal three-node cluster and FUSE, with closed-data backups, actual executable hashes, a dedicated temporary directory on the existing formal `TS1001L08`, verification after takeover, cleanup, and original-file/EE checks. Before doing so, recheck the live cluster and inventory. Preserve the separate pool and cartridges. Do not copy test `test-data` over `/home/rocky/ltfsd-data`.

`cargo fmt --all -- --check` has historical repository-wide failures. Changed snippets were formatted and `git diff --check` passed. `.codegraph/` is absent; do not create it. This handoff is committed with the implementation and evidence. Before any follow-up, check the live branch and working tree; keep subsequent changes on `ltfs-recovery-ibm-interop`.
