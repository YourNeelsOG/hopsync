# Logins and releases security review (2026-11-06)

Scope: the `keyboard-interactive` login path (`crates/hopsync/src/ssh.rs`), the RSA errors
(`ssh.rs`, `hostkeys.rs`), the pnpm guard (`crates/hopsync/web/`), and the CI and release pipeline
(`.github/workflows/`, `contrib/release/`).

Reviewer: the implementing agent, so this review is not independent. A second reviewer should check
it before a public release.

## Method

1. Read russh's client and server login code, then test each login case against an in-process server
   (`tests/ssh_password.rs`).
2. Run the whole path through the daemon against real OpenSSH in a container, for each login method
   (`web/e2e/password_install.py`). This run failed on the build from before the fix.
3. Run the release archives on old and current distros, and the ARM ones under QEMU
   (`contrib/release/smoke-all.sh`).
4. Lint the workflows with `actionlint`, and read each for what a build step could reach.

## Claims checked

| Claim | Result | Evidence |
|---|---|---|
| The password goes only to a single hidden prompt | Holds | `a_second_factor_fails_without_revealing_the_password` and `a_visible_prompt_never_receives_the_password` record what the server received: nothing. |
| A hostile server cannot keep a login going | Holds | Rounds are capped at 4, inside the existing connect timeout. |
| The host key is checked before any password is sent | Holds | Key exchange finishes before authentication, and `PinnedHostKeys` rejects an unpinned key. The new path changes neither. |
| The password is not kept | Holds | `password_install.py` searches every file in the data directory and the log after each install. |
| Other keys on the server are untouched | Holds | `password_install.py` compares `authorized_keys` before and after, on real OpenSSH. |
| The RSA messages reveal nothing | Holds | They name the key type and the path. Key material is not read into any message. |
| The build job cannot publish | Holds | `build` has `contents: read`, and checkout keeps no credentials. Only `publish` can write, and it does not check out or run repository code. |
| Workflow values cannot inject shell | Holds | The tag reaches `run` steps only through `env`. |
| Forks get no secrets | Holds | CI uses `pull_request`, not `pull_request_target`, with read-only access. |
| A release is built from reviewed code | Holds | The tag must be on `main`, match the crate version, and pass audits and tests in the same job. |

## Findings

### L1. Password login failed on servers with only keyboard-interactive (medium, availability)

**What:** only the `password` method was tried, so the correct password was reported as "rejected the
key". A wrong password now says "rejected the password". **Fix:** fall back to `keyboard-interactive`. **Test:**
`a_server_that_only_asks_by_keyboard_interactive_logs_in`, and the second setup in
`password_install.py`.

### L2. RSA keys failed with misleading errors (low, availability)

**What:** an RSA user key was reported as rejected, and an RSA-only host as "Wrong server signature".
**Fix:** both now name RSA and the way out. **Test:** `tests/ssh_key_types.rs`.

## Accepted risks

- **A second factor and a wrong password get the same message.** It names both possibilities.
- **"Wrong server signature" could have another cause.** The trust message says RSA is the usual
  cause, not the only one, and nothing is pinned in any case.
- **The workflows have not run yet.** Only `actionlint` and local runs of the scripts they call have
  checked them. The first tag is their first real test.
- **Build tools come from crates.io and PyPI at pinned versions, without hashes.** The tag on `main`
  and the build record show what ran, not that the tools were clean.
- **The ARM binaries ran under QEMU, not on ARM hardware.** A transfer was run only on x86_64.
- **Glibc 2.17 ran in containers, on one recent kernel.** Older kernels take the fallback path in
  `paths/local.rs`, and no release has been tested on one.
- **Password login is tested on Debian's OpenSSH, not on a provider's setup.** Custom PAM stacks may
  ask differently, and they get a plain rejection.
- **Tags are not protected yet.** Anyone with write access could push one, but only the owner has it,
  and the tag must be on `main`, which only the owner can change.
