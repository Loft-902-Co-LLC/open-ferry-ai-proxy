# Releasing

How the maintainer makes a release. Pushing a tag runs [the release workflow](.github/workflows/release.yml), which does the following:
- builds the web dashboard once, and each binary with it built in;
- writes `SHA256SUMS`;
- attests each archive's build provenance;
- creates a **draft** release.

Nothing is public until the draft is reviewed and published by hand.

## Versions

open-ferry follows [Semantic Versioning](https://semver.org/). Until 1.0.0, a minor version may change behaviour; say so in the changelog when it does.

- The version is `version` under `[workspace.package]` in `Cargo.toml`. Every crate takes it from there, and the binary reports it: in its startup log line, in the management API's `X-CPA-VERSION` header, and in its `open-ferry/<version>` user agent.
- The tag is `v` and the version, such as `v0.1.0`. The workflow refuses a tag that doesn't match `Cargo.toml`.
- A version with a pre-release part, such as `0.2.0-rc.1`, is published as a pre-release.

## Steps

1. **Bump the version.** On a branch off `main`, set the new version in `Cargo.toml`. Then run any cargo build, such as `cargo check --workspace`, so that `Cargo.lock` takes it too. Commit both files: the release builds with `--locked` and fails on a stale lock file.

2. **Update [CHANGELOG.md](CHANGELOG.md).**
   - Move what is under `## [Unreleased]` to a new `## [X.Y.Z] - YYYY-MM-DD` section below it, and leave `Unreleased` empty.
   - Write it for users: what they'll notice, not how it was done. Changes to the behaviour of CLIProxyAPI's config, flags or management API get a line each, and so does a change of the oldest glibc or macOS the binaries run on.
   - Update the links at the bottom. `[Unreleased]` compares the new tag with `HEAD`, and the new version links to its release:

     ```
     [Unreleased]: https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/compare/vX.Y.Z...HEAD
     [X.Y.Z]: https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/tag/vX.Y.Z
     ```

   The workflow fails if the section is missing, and uses its text as the release notes.

3. **Check the docs.** The [migration guide](docs/migrating-from-cliproxyapi.md) and the README's install section should match what ships.

4. **Merge.** Open a pull request with these changes and merge it once CI passes.

5. **Dry run (recommended).** Run the workflow on `main` by hand (see [Dry run](#dry-run)), and check that every target builds before you tag.

6. **Tag the merged commit on `main`, and push the tag:**

   ```sh
   git switch main
   git pull --ff-only
   git tag -a vX.Y.Z -m "open-ferry X.Y.Z"
   git push origin vX.Y.Z
   ```

7. **Review the draft.** When the workflow finishes, the draft is on the [Releases](https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases) page. Check that:
   - it has five archives and `SHA256SUMS`;
   - the notes are the changelog section;
   - an archive downloads and checks out as the README's [install section](README.md#install) says: its hash against `SHA256SUMS`, and its attestation with `gh attestation verify`;
   - the binary runs (`open-ferry -h`).

8. **Publish** the draft. It then becomes the latest release, which the management API's `latest-version` route reports to the running proxies. A pre-release doesn't.

## Dry run

Run the **Release** workflow from the Actions tab (**Run workflow**), on `main` or any branch. It does the following:
- builds every target;
- packages the archives;
- writes `SHA256SUMS`.

It keeps them all as workflow artifacts, so they can be downloaded and tried. It doesn't check the tag or the changelog, attests nothing and publishes nothing. The archives are named after the version in `Cargo.toml`, whatever it is.

## When something fails

- **Before the draft is created:** if the failure was a passing fault, such as a runner or network problem, re-run the failed jobs. Otherwise, fix it on `main`, then move the tag to the fixed commit. Delete the tag on GitHub and locally (`git push origin :refs/tags/vX.Y.Z`, `git tag -d vX.Y.Z`), and tag again.
- **After the draft is created:** delete the draft before re-running, since a tag can have only one release. Until the release is published, the tag can still be moved as above.
- **After publishing:** never move or reuse a published tag. Fix it in a new patch release.

A run that failed may have made attestations already. They stay, but they only vouch for the exact files they name, so they do no harm.

## Notes

- **Runners.** The workflow pins its runner labels:

  | Target | Runner |
  |---|---|
  | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
  | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
  | `x86_64-apple-darwin` | `macos-15-intel` |
  | `aarch64-apple-darwin` | `macos-15` |
  | `x86_64-pc-windows-msvc` | `windows-2025` |

  Each target builds natively on a runner of its own architecture. Move the labels on when GitHub deprecates an image (see the [runner images](https://github.com/actions/runner-images) announcements).
- **glibc.** The Linux binaries need the glibc of the Ubuntu they were built on: 2.39 or newer on Ubuntu 24.04. Each build's summary shows the newest glibc symbol the binary uses. Building on a newer Ubuntu raises the requirement, so mention it in the changelog when you change the runner.
- **macOS.** The binaries run on macOS 11 or newer, set by `MACOSX_DEPLOYMENT_TARGET`.
- **Windows.** The binary links the C runtime statically, so it doesn't need the Visual C++ Redistributable.
- **Build information.** `OPEN_FERRY_COMMIT` and `OPEN_FERRY_BUILD_DATE` are set at build time. They are the management API's `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` headers. A build without them says `none` and `unknown`.
- **No cache.** Release builds start from scratch, so nothing a cache holds can get into a release.
- **The dashboard.** The `dashboard` job builds the web app in `dashboard/` once, with the Node version in `dashboard/.nvmrc`, `npm ci --ignore-scripts` and `npm run build`, without an npm cache. Every build job downloads the result into `dashboard/dist` and builds with `OPEN_FERRY_REQUIRE_DASHBOARD=1`, so a binary can't ship without it. Each archive's `licenses/dashboard-third-party-licenses.txt` lists the npm packages built into the dashboard, with their licenses. The dashboard build fails if one of them isn't MIT, ISC, Apache-2.0 or BSD; replace the package, or settle its license, before releasing. To move to a newer Node, change `.nvmrc`; `engines.node` in `dashboard/package.json` is the oldest Node that builds it.
- **Actions** are pinned to full commit SHAs, and Dependabot proposes updates.
- **Attestations** need the repository to be public, or on GitHub Enterprise Cloud if it is private.
- **Tag protection.** Consider a ruleset that lets only the maintainer create `v*` tags, since pushing one starts a release.
