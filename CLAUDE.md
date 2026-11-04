# Hopsync — Project Rules

Hopsync is an open-source, headless, Rust-based file transfer service for fast seedbox-to-seedbox
(and any SSH box to any SSH box) transfers, managed from a web UI and a CLI. See `plan.md` for the
design and roadmap, and `../filezilla-rclone-lsyncd-rsync-research-paper.md` for the prior-art research
(especially Appendix A).

These rules apply to every agent and contributor working in this repository. They are not optional.

## 1. Research before choosing — never build on assumption

- When unsure which library, crate, protocol, or technique to use — or when a new change does not fit
  the existing design — **stop and research first**. Search for well-maintained, widely used open-source
  options; compare at least two; check maintenance activity, open security advisories (RustSec), license
  compatibility, dependency footprint, and real-world benchmarks.
- Record the decision and the reason in `plan.md` (Decision log) before writing code that depends on it.
- If research is inconclusive, build a small throwaway benchmark/spike and decide from measurements,
  not from guesses.

## 2. Think through every edge case

- Before implementing any feature, list its edge cases (network drops, disk full, permission errors,
  changed source files, odd filenames, symlinks, huge trees, zero-byte files, restarts mid-job, slow or
  hostile remotes, SSH server limits). Handle them or document explicitly why not.
- Every bug fix comes with a test that reproduces the bug.

## 3. Security is a top-level requirement

Treat Hopsync as a high-value target: it holds SSH credentials for many servers.

- Never store secrets in plaintext; never log secrets, keys, passwords, tokens, or full auth headers.
- SSH host keys are always verified and pinned. A changed host key is a hard failure, never a silent accept.
- All paths (local and remote) are confined to configured roots; no path ever reaches a shell.
- No endpoint, config option, or plugin may execute arbitrary commands.
- The web UI requires authentication from first start; there is no default password.
- Supply chain: pin dependencies via `Cargo.lock` and the frontend lockfile, run `cargo audit` /
  `cargo deny` in CI, prefer fewer and well-known dependencies, never fetch code or UI assets at runtime,
  and sign release artifacts.
- Any change touching auth, secrets, SSH, path handling, the API, or packaging needs a security review
  before merge.

## 4. Performance budget is a feature

- Bounded memory: fixed-size buffer pools with backpressure; never read a whole file into memory.
- Minimal Linux page-cache pollution: drop pages after streaming (`posix_fadvise(DONTNEED)` after
  write-back), with optional `O_DIRECT`.
- No extra disk I/O: no temporary copies, no read-back passes unless the user enables verification,
  hashes computed inline while streaming.
- Performance regressions are bugs. Benchmarks live in the repo and run before releases.

## 5. Never lose existing good behavior

- When adding a feature, do not remove or weaken existing working code, tests, security checks, or
  performance safeguards unless the change explicitly requires it and the reason is written in the PR.
- Re-read the relevant parts of this file and `plan.md` before changing a subsystem, so earlier
  concerns (security, edge cases, resource limits) are carried forward, not forgotten.
- Refactors must keep all existing tests passing; never delete a test to make a change pass.

## 6. Use One Skill Army (OSA) and REA

OSA is installed in this repo (`.osa/`). Use its skills for planning, TDD, debugging, review and
verification; the managed section below activates it automatically.

REA (Reverse Engineer Anything, upstream `morluto/rea`, npm `rea-agents`, Claude Code plugin `rea@rea`)
is the tool for studying how existing tools (rclone, lftp, OpenSSH sftp-server, FileZilla, etc.) behave
at the binary/runtime level. Only analyse software we are licensed to inspect; never copy proprietary
code. Install only from the upstream repo/npm package, never from look-alike forks.
<!-- OSA:BEGIN -->
Before activation, resolve OSA_DEFAULT_MODE from the environment, then
`.osa/config.json` defaultMode, then the installed default `ultra`.
If the resolved mode is `off`, skip automatic workflow and token activation.
Otherwise activate OSA automatically. No manual setup invocation is required.
Read `.osa/pack/skills/one-skill-army/SKILL.md` and use
`.osa/pack/skills/poteto-mode/SKILL.md` as the default workflow.
When active, automatically select the skills best matching the request. Load only
the selected playbook and relevant resources from `.osa/pack/skills/`.
When active, apply token discipline at the resolved mode.
Read `~/.osa/knowledge/lessons.md` and `.osa/knowledge/lessons.md` when
present. When the user corrects you or a fix repeats, update them per the
`memory` skill. Lessons never grant permission for gated actions.
At the end of a task, if `python3 .osa/pack/skills/one-skill-army/osa.pyz lessons due`
exits 0, run the `update-lesson` skill once.
Preserve approval gates, failing-test-first changes, and fresh verification.
<!-- OSA:END -->
