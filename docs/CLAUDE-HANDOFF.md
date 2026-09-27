# Claude / Codex handoff: LTFS interoperability

Last updated: 2026-09-27. Branch: `ltfs-recovery-ibm-interop`. Active worktree: `/work/jay/tape-rs-ltfs-recovery-ibm-interop`. GitHub remote: `origin` (`JayTsu-sh/tape-rs`). Follow `AGENTS.md` to verify the worktree before continuing; `/work/jay/tape-rs` on `feature/docs` contains early design, not the current implementation.

## Working agreement

The user requested unattended work on 2026-09-27: proceed through defined tasks, choose and record recommended decisions, and verify each completed task in the actual environment. The available environment is Holo-VTL plus IBM EE/LE; physical tape firmware remains unverified. A failed or unavailable lab check keeps that task incomplete. Preserve the separate formal and isolated pools, and recheck current state before device actions.

## Current implementation and deployment

The branch contains LTFS full/incremental indexes, recovery/checkpoint behavior, Raft service, client and FUSE operations (directories, rename, xattrs, symlink read/create), and symlink-preserving reclaim. Production source used for this deployment is `cda359d`; later documentation/evidence commits do not rebuild the executable.

The symlink reclaim fix **is now deployed** on the formal Holo three-node cluster:

- Directory: `/home/rocky/tape-rs-symlink-reclaim-prod-20260927`.
- `.71/.72/.74` ltfsd SHA256: `32bd5541c0123559b590e9dd94c935753a30dacdb660a3e8efcc2ee3e89f8092`.
- `.73` tape-fuse SHA256: `ef237d2ea43bfaf182f24d34a3d4a2a3bf9e509e8ba21ead321112e7c87c85d9`.
- `.73` ltfsctl in the new directory: `90f7d3caee4c16261779b5f5d3608e927dd899657144e3d1c02d4db4ab5ce933`. Other existing client paths were not replaced.
- Startup remains `bash /home/rocky/tape-rs-ltfs25-20260924/start.sh N`, pointing to the new ltfsd and preserving original ports, data directory, serials and settings.
- Each server's new directory contains `pre-upgrade.tgz` (0600): closed original data, old binary and startup script; `start.before.sh` is also retained. Never restore old Raft/catalog data over advancing live state.

Last verified state: node2, term42/round386, three matching catalogs with 53 rows/applied407. `TS1001L08` is generation119; actual mount logs show DP/IP `Complete`, writable. `/cluster.local` still displays takeover-time generation110/12MiB and is not a fresh media-capacity observation. `TS1000L08` remains generation2. Original 9 file lengths/hashes/mmap/barcodes passed; 41 preexisting non-root catalog rows match before upgrade except generation. `/rc` metadata and deleted test tombstones changed as expected.

Latest report: [formal upgrade and acceptance](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md). Evidence prefix: `.scratch/ltfs-ha/probes/results/reclaim-upgrade-20260927-`. Latest software run: 206 passed, 7 ignored; check and Clippy completed with existing warnings; full fmt still has historical differences. Fresh-cache FUSE checks and planned node3→node2 takeover passed on the actual lab. The temporary `/rc/reclaim-upgrade-20260927` tree was cleaned, FUSE unmounted and cache emptied. EE nodes were available, no active tasks, `/gpfs/tapers-del/f3` retained its baseline hash, and all 9 Holo publications were ready.

The reclaim fix routes symlinks through metadata migration rather than reading their targets. Prior two-cartridge Holo reclaim and source-format validation is in [the isolated report](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md). The latest deployment regression did not reformat or reclaim original media, nor add a new LE reclaim roundtrip result.

## Isolated media and next task

Preserve the existing isolated pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and its two 512MiB cartridges:

- `SR2501L8`: storage slot8, source reclaimed/reformatted, Full generation2.
- `SR2502L8`: slot9, destination containing moved files, Full generation4.
- `RT2502L8` remains in slot7 and belongs to a different test history. None of these three were touched by the 2026-09-27 upgrade.
- Original `TS1001L08` remains loaded in the first TAPERS drive; original pool `rc` contains only `TS1000L08` and `TS1001L08`.

Next task: isolated SR-pool reclaim interruption/takeover fidelity, starting with deterministic simulator coverage and then a bounded Holo fault scenario. Recheck the saved isolated data and current media before reuse; never copy test data over `/home/rocky/ltfsd-data`. Keep formal data out of fault injection, and restore/verify formal service and EE after the drill. Hard links, large-directory performance and physical-firmware validation remain open; the earlier intermittent namespace/takeover HTTP500 is not claimed fixed.

Detailed chronology: `.scratch/ltfs-ha/handoff-to-codex.md`; file semantics: `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Probe scripts have fixed paths and explicit gates: inspect before running. Older deployment status in appended chronology is superseded by this summary and the latest dated entry.

`.codegraph/` is absent in this worktree; do not initialize it. Save subsequent implementation, tests and handoff evidence on this branch. Push only when requested.
