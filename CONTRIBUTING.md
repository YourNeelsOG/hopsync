# Contributing to Hopsync

## Branches

| Branch | Purpose | Who can change it |
|---|---|---|
| `main` | Production. Holds what is released. | The maintainer only. |
| `dev` | Integration. All new work lands here first. | Pull requests only. |
| Any other | Your feature or fix. | You. |

```mermaid
flowchart LR
    work["Your branch<br>(in your fork)"] -->|pull request| dev
    dev -->|maintainer merges| main
    main --> prod["Production"]
```

Do not open a pull request against `main`. It will be closed or retargeted to `dev`.

## Send a change

1. Fork the repository. If you have write access, you can create the branch in this repository
   instead.
2. Create a branch from `dev`:

   ```bash
   git fetch origin
   git switch -c fix/short-name origin/dev
   ```

3. Make the change, and add a test with every bug fix.
4. Run the checks in the next section.
5. Push your branch and open a pull request with `dev` as the base branch.

## Run the checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo audit
```

If you changed the web UI, also run these:

```bash
cd crates/hopsync/web
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test
pnpm build
```

Use the pnpm version that `packageManager` in `package.json` names. An older pnpm stops with
`packages field missing or empty`. To run the pinned version without installing it, replace `pnpm`
with `npx pnpm@<version>`.

No CI runs on pull requests yet, so include the results of these checks in your pull request.

## Project rules

- **Research before you choose.** Compare at least two options before you add a dependency. Check
  its maintenance, RustSec advisories, license and size. Record the decision in the
  [Decision log](plan.md#10-decision-log).
- **List the edge cases first.** Consider network drops, a full disk, permission errors, changed
  source files, odd file names, symlinks, huge trees, empty files, restarts during a job and hostile
  servers.
- **Treat security as a requirement.** Do not store secrets in plaintext or write them to a log.
  Always verify and pin host keys. Keep paths inside their configured roots, and never pass a path
  to a shell. No endpoint, setting or plugin may run a command.
- **Keep to the performance budget.** Memory stays bounded, whole files are never read into memory,
  and transfers make no temporary copies. A performance regression is a bug.
- **Keep what already works.** Do not remove a test to make a change pass. If you must weaken a
  safeguard, explain why in the pull request.

Changes to authentication, secrets, SSH, path handling, the API or packaging need a security review
before they merge.

## Report a security problem

Do not open a public issue for a vulnerability. Use
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository.

## License

By contributing, you agree that your work is licensed under both [Apache-2.0](LICENSE-APACHE) and
[MIT](LICENSE-MIT), as the rest of the project is.
