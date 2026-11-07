# One-line install and setup security review (2026-11-07)

Scope: `install.sh`, `crates/hopsync/src/setup.rs`, `crates/hopsync/src/ui.rs`, the new commands in
`main.rs`, and the release workflow change.

Reviewer: the implementing agent, so this review is not independent. A second reviewer should check
it before a public release.

## Method

1. Run setup for real under systemd in containers, as root and without root, on Debian 12, Ubuntu
   22.04 and Rocky 9 (`contrib/release/setup-e2e.sh`, 209 checks).
2. Run the installer in clean containers with curl only, wget only, and neither, including a tampered
   archive and a cut-off script (`contrib/release/install-test.sh`).
3. Drive setup on a real pseudo-terminal, record every byte, and search the recording for the
   password. Replay it in xterm.js to see what a person sees.
4. Read each new command, path and file write for what a local user or hostile value could reach.

## Claims checked

| Claim | Result | Evidence |
|---|---|---|
| Setup adds no new way to set a password | Holds | It calls the existing `/api/setup` with the existing one-time token, so hashing, password rules and the audit record are unchanged. |
| The password goes only to Hopsync's own daemon | Holds, see the accepted risk below | Before sending, `listener_uid` reads `/proc/net/tcp` and requires the port to belong to the service's uid. Unit tested; ports other customers hold are skipped in the shared-box run. |
| The password is not shown or logged | Holds | The terminal recording, plain output, journal and background log are searched for it. Typed input has no echo. |
| The password's length is not left on screen | Holds | The dots are replaced with a mark when Enter is pressed. |
| The terminal is restored on every exit | Holds | `RawMode` restores on drop, Ctrl-C at the prompt is read as a byte, and the Ctrl-C handler restores the cursor. Checked in the cancelled recording. |
| No value can add a line to a unit file or crontab | Holds | `check_path_text` allows only characters with no meaning to either, on every path and account name. `a_bad_value_cannot_inject_a_unit_line` tries it. |
| Nothing reaches a shell | Holds | Every command is fixed arguments through `std::process::Command`. The only shell is cron's, and it receives a checked path. |
| The service never runs as root | Holds | A root-owned folder is refused with the fix, and root without systemd is refused. Checked by process owner in the end-to-end test. |
| Hardening survives setup | Holds | The unit is the checked-in file with values swapped, and a unit test asserts the directives remain. A home folder gets `ProtectHome=tmpfs` with only that folder bound. |
| The web UI cannot reach the box's keys or Hopsync's secrets through the folder | Holds | Setup refuses a folder that contains `.ssh`, is inside one, contains the home or the data folder, or is inside the data folder. Writing to `authorized_keys` or `.bashrc` is running commands, so a whole home is refused like `/root`, and the default for a user without root is `~/hopsync`. Found on a real shared box, where v1.0.0 defaulted to the whole home. An install that already has a folder keeps it until setup is given another. Unit tested and checked end to end. |
| A unit setup did not write is left alone | Holds | Setup and uninstall both refuse it. Checked end to end. |
| A stale pid file cannot kill another process | Holds | A signal goes only to a process of this uid whose argv is this data directory's daemon. Checked end to end with a live unrelated process. |
| Other cron jobs are untouched | Holds | Only lines ending in the tag change, and an unreadable crontab is never rewritten. Checked end to end. |
| Uninstall leaves the user's files | Holds | Nothing ever deletes the files folder. `--purge` deletes only Hopsync's data folder and asks first. |
| A tampered or cut-off download installs nothing | Holds | Checked end to end. The script's body runs only from its last line. |
| The installer stays on HTTPS | Holds for curl and wget | `--proto =https --tlsv1.2` and `--https-only` unless a mirror was set by the user. |

## Public HTTPS (2026-11-07)

Scope: `crates/hopsync/src/tls.rs`, the listener wiring in `daemon.rs`, and `--public` in setup. Same
reviewer, so also not independent.

| Claim | Result | Evidence |
|---|---|---|
| Nothing becomes public unasked | Holds | Only `--public`, `--tls-cert` or a yes on a terminal opens it. Scripts that say nothing stay on loopback. Unit tested and checked end to end. |
| A bad certificate never falls back to plain HTTP | Holds | The daemon loads it before binding and stops on error. `tests/tls.rs` tries a missing file, a certificate given as the key, and a key from another certificate. |
| The private key stays private | Holds | Written 0600 from creation, in a 0700 data folder, owned by the service account. Not logged. Checked end to end. |
| Passwords never cross the network in plain text | Holds | Plain HTTP from another machine gets only a redirect or a 400, before any routing. Setup's own password call stays on loopback. |
| The redirect cannot be steered | Holds | `Host` must be letters, digits and `.-:[]`, so a request cannot add a header or a path. It can only name the host the browser already asked for. Unit tested. |
| One client cannot stop the others | Holds | Each connection has its own task and a 10 s limit for the first byte and for the handshake. Tested with stalled connections. |
| One address cannot use up the connections | Holds | 32 per address, 1024 in all. Tested, including that a slot comes back. |
| Session cookies stay on HTTPS | Holds | `Secure` is set whenever TLS is on. Tested against the whole daemon. |
| The login throttle still sees each client | Holds | The listener's peer address reaches handlers. Tested. |
| TLS is current | Holds | TLS 1.3 only, on the `aws-lc-rs` already in the tree. `cargo audit` and `cargo deny` pass. |

Accepted risks of public mode:

- **The first visit is only as safe as the fingerprint check.** A person who clicks through the
  warning without comparing can be intercepted. Nothing can prevent this with a self-signed
  certificate on a bare IP; `--tls-cert` with a real domain removes it.
- **The login page faces the internet.** The password has at least 12 characters and is hashed with
  argon2id, and failures are throttled per address. A botnet using many addresses is throttled less,
  and is slowed by one hash at a time. A firewall allow-list is the stronger answer where possible.
- **The setup token is exposed to the network for a moment.** Between the daemon starting and setup
  setting the password, `/api/setup` is reachable, but it needs the 0600 token, which never leaves
  the box.
- **Plain HTTP from loopback is served.** Another account on a shared box can reach the login page
  that way. It could already reach the port, and it still needs the password. Loopback traffic is
  not visible to other accounts without root.
- **A slow distributed flood can reach the 1024 total.** That belongs to a firewall.
- **IPv6 is not served** in public mode.

## Accepted risks

- **`curl | sh` trusts the release host.** The checksum list comes from the same place as the archive,
  so it catches corruption, not a compromised release. People who want more can download the archive
  and run `gh attestation verify`, which the README describes.
- **A hostile local user could win a very short race for the port.** After the check, the daemon holds
  the port, so another process cannot take it unless the daemon dies in the instant between the check
  and the request. The token is one-time and the password would be lost only in that case. On a
  shared box, loopback ports are visible to other customers in any case, and the UI requires a login.
- **The tunnel is plain HTTP on loopback.** SSH encrypts it across the network.
- **When a folder's owner runs the service, that account can read the sealed store and its key.**
  Setup does this only when the owner already holds the files, and root chooses it by naming the
  folder. Choosing a new folder keeps the dedicated account.
- **Background mode has no restart on a crash.** It restarts at reboot through cron where allowed.
  `hopsync status` shows when it has stopped.
- **A shared box's other customers can see that the port is open.** They still need the password. The
  login limits apply as they do everywhere.
- **SELinux enforcing was not tested.** The Rocky 9 run was in a container, where it is not enforced.
- **The published install URL has not run yet.** It needs a release. The tests use a local mirror, and
  the release workflow's new steps have not run on GitHub.
