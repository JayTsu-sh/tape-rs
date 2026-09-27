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

Last verified state after the formatting/Clippy regression: node1, term49/round439, three matching catalogs with 53 rows/applied432. `TS1001L08` remains generation120, DP/IP `Complete`, writable, 7MiB available. `TS1000L08` remains generation2. Original 9 file lengths/hashes/mmap/barcodes passed on a fresh FUSE cache; all 53 catalog rows and pool summaries are exactly unchanged from before this drill. Sorted catalog SHA256: `3339567f1c5d8e5f2255ef7fae5557dacf4d60bad2af9b5ea3d03c33f480e6b3`.

Earlier report: [interruption after completed mkltfs](../.scratch/ltfs-ha/holo-reclaim-formatted-20260927.md), following [interruption before FORMAT](../.scratch/ltfs-ha/holo-reclaim-cut-20260927.md) and [formal deployment](../.scratch/ltfs-ha/holo-reclaim-upgrade-20260927.md). Latest software run: 213 passed, 7 ignored; full fmt, check and strict Clippy (`-D warnings`) pass after actual warning fixes with no new suppression. The compatible Zig build succeeds with an external deprecated linker optimization warning retained. See [cleanup validation](../.scratch/ltfs-ha/holo-cleanup-20260927.md). Those earlier interruption tasks added test-only barriers; the raw-FORMAT recovery below now includes a production fix. Holo resumed from an empty Full1 source while replicated state was still reclaiming; repeated formatting and completion succeeded, with cold-cache FUSE and complete link XML fidelity checks. Formal service and original files were verified; EE nodes available, no active tasks, baseline f3 hash intact, 9 publications ready. FUSE unmounted/cache empty; isolated processes and strace stopped.

The reclaim fix routes symlinks through metadata migration rather than reading their targets. Prior two-cartridge Holo reclaim and source-format validation is in [the isolated report](../.scratch/ltfs-ha/holo-symlink-reclaim-20260925.md). The latest deployment regression did not reformat or reclaim original media, nor add a new LE reclaim roundtrip result.

## Isolated media and next task

Preserve pool UUID `51cfd06c-bf79-4bbf-ac85-886608a0ab4b` (`sr`) and its two 512MiB cartridges. Latest **closed isolated data** is `/home/rocky/tape-rs-reclaim-le-20260927/test-data` on .71/.72/.74, ports 7500/7501, node1/round163 at shutdown. Do not resume either older symlink-reclaim or reclaim-cut directory: media have moved forward.

- `SR2501L8`: slot8, destination, Full generation8 after IBM LE roundtrip, six links and the new LE file.
- `SR2502L8`: slot9, Full generation3 with the deleted checkpoint test file’s superseded copy; the live catalog tombstone is on SR1.
- `RT2502L8`: slot7, separate history, untouched.
- `TS1001L08`: first TAPERS drive; formal pool `rc` still contains only TS1000L08/TS1001L08.
- Latest Holo backups: `/home/rocky/holo-namespace-20260927`, before/after for both SR cartridges with verified hashes. The earlier reclaim-formatted node1 directory has `at-cut.tgz`; this is evidence, not the latest resumable state.
- Device mapping differs: TAPERS changer is .71 `/dev/sg9`, .72 `/dev/sg8`. Always verify VPD serial `IBMtaper2287_LL3` and the current holder before device commands.

Next bounded task: large-directory measurement and measured optimization. Reclaimed-media IBM LE roundtrip passed; see [report](../.scratch/ltfs-ha/holo-reclaim-le-20260927.md). New LE copy SR2501L08 is unassigned in LISA slot1033, Full8; TS1000L8 restored Full99. Latest isolated directory uses both drives. [Namespace/checkpoint fix](../.scratch/ltfs-ha/holo-namespace-20260927.md) is verified: preserve commit error causes and answer retryable 503 for ownership loss before read output. Candidate ltfsd 10409954… is in `/home/rocky/tape-rs-namespace-20260927`; formal binaries remain unchanged. The older namespace regression startup has one drive; latest reclaim-le startup has both. Raw FORMAT recovery is also verified; never mix old/new nodes with reformatting pending. Large-directory performance and hard links/user.ltfs.sync remain queued; physical firmware validation is deferred.

Ordered unattended queue: [tasks 0–5](../.scratch/ltfs-ha/autonomous-queue-20260927.md). Cleanup candidate `/home/rocky/tape-rs-cleanup-20260927` is validated in isolation; formal binaries remain unchanged.

Detailed chronology: `.scratch/ltfs-ha/handoff-to-codex.md`; file semantics: `.scratch/ltfs-ha/file-contract.md` and `docs/file-client.md`. Probe scripts have fixed paths and explicit gates: inspect before running. Older deployment status in appended chronology is superseded by this summary and the latest dated entry.

`.codegraph/` is absent in this worktree; do not initialize it. Save subsequent implementation, tests and handoff evidence on this branch. Push only when requested.
