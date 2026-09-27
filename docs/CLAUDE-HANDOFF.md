# Claude / Codex handoff: LTFS interoperability

Last updated: 2026-09-27. Branch: `ltfs-recovery-ibm-interop`. Active worktree: `/work/jay/tape-rs-ltfs-recovery-ibm-interop`. GitHub remote: `origin` (`JayTsu-sh/tape-rs`). Follow `AGENTS.md` to verify the worktree before continuing; `/work/jay/tape-rs` on `feature/docs` contains early design, not the current implementation.

Next authorized queue: [remaining implementation](../.scratch/ltfs-ha/remaining-implementation.md), in order: documentation, intermittent directory outcome, LE metadata semantics, performance, formal rollout. The prior 0–5 queue is complete.

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

Last verified state after completing tasks 0–5: node2, term51/round449, three matching catalogs with 53 rows. `TS1001L08` remains generation120, DP/IP `Complete`, writable, 7MiB available. `TS1000L08` remains generation2. Original 9 file lengths/hashes/mmap/barcodes passed on a fresh FUSE cache; all 53 catalog rows and pool summaries are exactly unchanged from before this drill. Sorted catalog SHA256: `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`.

Earlier report: [interruption after completed mkltfs](../.scratch/ltfs-ha/holo-reclaim-formatted-20260927.md), following [interruption before FORMAT](../.scratch/ltfs-ha/holo-reclaim-cut-20260927.md) and [formal deployment](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md). Latest software run: 218 passed, 7 ignored; full fmt, check and strict Clippy (`-D warnings`) pass after actual warning fixes with no new suppression. The compatible Zig build succeeds with an external deprecated linker optimization warning retained. See [cleanup validation](../.scratch/ltfs-ha/holo-cleanup-20260927.md). Those earlier interruption tasks added test-only barriers; the raw-FORMAT recovery below now includes a production fix. Holo resumed from an empty Full1 source while replicated state was still reclaiming; repeated formatting and completion succeeded, with cold-cache FUSE and complete link XML fidelity checks. Formal service and original files were verified; EE nodes available, no active tasks, baseline f3 hash intact, 9 publications ready. FUSE unmounted/cache empty; isolated processes and strace stopped.

The reclaim fix routes symlinks through metadata migration rather than reading their targets. Prior two-cartridge Holo reclaim and source-format validation is in [the isolated report](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md). Original media were not reformatted or reclaimed by this queue. The separate reclaimed-media LE roundtrip is now verified below.

## Isolated media and next task

Preserve pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and its two 512MiB cartridges. Latest **closed isolated data** is `/home/rocky/tape-rs-directory-outcome-20260927/test-data` on .71/.72/.74, ports 7500/7501, node2/round246 at shutdown, 1139 rows. Do not resume either older symlink-reclaim or reclaim-cut directory: media have moved forward.

- `SR2501L8`: slot8, destination, Full generation41 after directory outcome verification and normal shutdown, six original links, LE file, 1100 benchmark files and three sync files.
- `SR2502L8`: slot9, Full generation3 with the deleted checkpoint test file’s superseded copy; the live catalog tombstone is on SR1.
- `RT2502L8`: slot7, separate history, untouched.
- `TS1001L08`: first TAPERS drive; formal pool `rc` still contains only TS1000L08/TS1001L08.
- Latest Holo backups: `/home/rocky/holo-sync-boundary-20260927`, before/after for both SR cartridges with verified hashes. The earlier reclaim-formatted node1 directory has `at-cut.tgz`; this is evidence, not the latest resumable state.
- Device mapping differs: TAPERS changer is .71 `/dev/sg9`, .72 `/dev/sg8`. Always verify VPD serial `IBMtaper2287_LL3` and the current holder before device commands.

Ordered unattended [tasks 0–5](../.scratch/ltfs-ha/autonomous-queue-20260927.md) are complete. Latest isolated candidate is `/home/rocky/tape-rs-sync-boundary-20260927`: ltfsd SHA256 `5d2ff8eb0b39debadb95739213cfdf315cb444786f9540420053a78554ecaaa0`, FUSE `62aa241430eb677fcbca9de807a631334816875e09694f0711ea66ea318996a2`. Both TAPERS drives configured. **Formal binaries above remain unchanged; these new changes are not formally rolled out or pushed to GitHub.** Physical firmware validation remains deferred.

- [Raw FORMAT recovery](../.scratch/ltfs-ha/holo-reclaim-raw-20260927.md): durable reformatting state before destructive FORMAT. Never mix old/new nodes with that state pending.
- [Namespace/checkpoint read fix](../.scratch/ltfs-ha/holo-namespace-20260927.md): typed error causes, retryable503 only for lost ownership before read output.
- [Reclaimed-media LE roundtrip](../.scratch/ltfs-ha/holo-reclaim-le-20260927.md): file/link/xattr/mtime fidelity passed. LE SR2501L08 now Full10 in LISA slot1033, unassigned; its history diverged before the benchmark. **Do not copy it over current SR1.** TS1000L8 remains restored Full99.
- [Directory benchmark](../.scratch/ltfs-ha/holo-directory-perf-20260927.md): optional link types with old-server fallback; 1000-file median843→18ms, HTTP1013→10; cold1100-file verification passed. One initial full-suite directory test returned Indeterminate during concurrent build; isolated and full retests passed, initial log retained.
- [Hard links and explicit sync](../.scratch/ltfs-ha/holo-sync-boundary-20260927.md): hard links explicitly unsupported (LE also rejects). Write-only `user.ltfs.sync` on the FUSE mount root flushes existing writers/staging and checkpoints the active write volume; failures/ambiguous responses never report success. Holo Full30 persisted across SIGKILL and node3 takeover, with cold-cache verification. Existing stronger fsync guarantee retained.

Detailed chronology: `.scratch/ltfs-ha/handoff-to-codex.md`; file semantics: `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Probe scripts have fixed paths and explicit gates: inspect before running. Older deployment status in appended chronology is superseded by this summary and the latest dated entry.

`.codegraph/` is absent in this worktree; do not initialize it. Save subsequent implementation, tests and handoff evidence on this branch. Push only when requested.

## Remaining queue progress (2026-09-27)

Items 1–2 complete: research status consolidated; [directory outcome investigation](../.scratch/ltfs-ha/directory-outcome-20260927.md) identified cached old-leader requests after fencing, corrected the persistence test, and preserved server diagnostics without automatic replay. 219 tests passed, 7 ignored; strict Clippy/fmt/check passed; Holo fault and cold FUSE verification passed. Next: LE permissions/time semantics, then performance and formal rollout.

Latest formal state: node1 term52/round454, original 53 catalog rows unchanged and 9 files verified. Formal binaries remain cda359d deployment. Latest closed isolated data is directory-outcome/test-data (SR1 Full41, SR2 Full3, 1139 rows); use it instead of sync-boundary data. Media backups: /home/rocky/holo-directory-outcome-20260927. All isolated processes/tracers stopped and FUSE unmounted. Physical acceptance remains deferred.
