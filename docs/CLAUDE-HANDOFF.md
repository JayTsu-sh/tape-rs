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

Last verified state after the post-mkltfs drill: node1, term44/round414, three matching catalogs with 53 rows/applied417. `TS1001L08` remains generation120, DP/IP `Complete`, writable, 7MiB available. `TS1000L08` remains generation2. Original 9 file lengths/hashes/mmap/barcodes passed on a fresh FUSE cache; all 53 catalog rows and pool summaries are exactly unchanged from before this drill. Sorted catalog SHA256: `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`.

Latest report: [interruption after completed mkltfs](../.scratch/ltfs-ha/holo-reclaim-formatted-20260927.md), following [interruption before FORMAT](../.scratch/ltfs-ha/holo-reclaim-cut-20260927.md) and [formal deployment](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md). Latest software run: 208 passed, 7 ignored; check and Clippy completed with existing warnings; full fmt still has historical differences. Both interruption tasks add test-only transport barriers and no production change. Holo resumed from an empty Full1 source while replicated state was still reclaiming; repeated formatting and completion succeeded, with cold-cache FUSE and complete link XML fidelity checks. Formal service and original files were verified; EE nodes available, no active tasks, baseline f3 hash intact, 9 publications ready. FUSE unmounted/cache empty; isolated processes and strace stopped.

The reclaim fix routes symlinks through metadata migration rather than reading their targets. Prior two-cartridge Holo reclaim and source-format validation is in [the isolated report](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md). The latest deployment regression did not reformat or reclaim original media, nor add a new LE reclaim roundtrip result.

## Isolated media and next task

Preserve pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and its two 512MiB cartridges. Latest **closed isolated data** is `/home/rocky/tape-rs-reclaim-formatted-20260927/test-data` on .71/.72/.74, ports 7500/7501, node2/round95 at shutdown. Do not resume either older symlink-reclaim or reclaim-cut directory: media have moved forward.

- `SR2501L8`: slot8, source reclaimed/reformatted, Full generation2 with no files and valid pool markers.
- `SR2502L8`: slot9, current destination, Full generation4 containing the six links and other live files.
- `RT2502L8`: slot7, separate history, untouched.
- `TS1001L08`: first TAPERS drive; formal pool `rc` still contains only TS1000L08/TS1001L08.
- Holo backups: `/home/rocky/holo-reclaim-formatted-20260927`, before/after for both SR cartridges with verified hashes. The killed node1 has `at-cut.tgz` in its new test directory; this is evidence, not the latest resumable state.
- Device mapping differs: TAPERS changer is .71 `/dev/sg9`, .72 `/dev/sg8`. Always verify VPD serial `IBMtaper2287_LL3` and the current holder before device commands.

Next bounded task: raw FORMAT succeeds but the new LTFS labels/indexes have not completed; deterministic simulator coverage then a bounded Holo fault check. The latest drill covered **mkltfs fully complete**, before pool markers and `TapeReclaimed`, not this earlier construction window. Keep formal data out of fault injection, preserve the updated SR state, and restore/verify formal service and EE after the drill. Hard links, large-directory performance and physical-firmware validation remain open; the intermittent namespace/takeover HTTP500 is not claimed fixed.

Detailed chronology: `.scratch/ltfs-ha/handoff-to-codex.md`; file semantics: `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Probe scripts have fixed paths and explicit gates: inspect before running. Older deployment status in appended chronology is superseded by this summary and the latest dated entry.

`.codegraph/` is absent in this worktree; do not initialize it. Save subsequent implementation, tests and handoff evidence on this branch. Push only when requested.
