# Releasing

How the maintainer makes a release. Pushing a tag runs [the release workflow](.github/workflows/release.yml), which does the following:
- builds the web dashboard once, and each binary with it built in, the static (musl) Linux ones in Alpine;
- lists the licenses of the Rust crates built into each binary;
- writes `SHA256SUMS`, of the archives and the install scripts, `install.sh` and `install.ps1`;
- once you approve it, signs `SHA256SUMS` with the release key, as `SHA256SUMS.minisig`, and checks the signature against [`release-keys.pub`](release-keys.pub) (see [The release key](#the-release-key));
- attests the build provenance of each archive and script;
- creates a **draft** release;
- then builds the container image from the static binaries, tests it, pushes it to GHCR and attests it.

Nothing but the container image is public until the draft is reviewed and published by hand. The image is pushed before the draft is published, and anyone can pull it from then on, once its package is public (see step 7 and the container image under [Notes](#notes)).

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

   When the archives are built, the **Sign SHA256SUMS** job waits for you. Open the run, choose **Review deployments**, tick `release-signing` and approve. Approve only a run of a tag you pushed.

7. **Review the draft.** When the workflow finishes, the draft is on the [Releases](https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases) page. Check that:
   - it has seven archives, one for each target in the [runners](#notes) table, `install.sh`, `install.ps1`, `SHA256SUMS`, which lists those nine, and `SHA256SUMS.minisig`;
   - the signature checks out with the key in `release-keys.pub`, and its trusted comment is `open-ferry X.Y.Z SHA256SUMS`: `minisign -Vm SHA256SUMS -P <key>`;
   - an archive holds `config.example.yaml`, and its `licenses/` holds `rust-third-party-licenses.txt` for its own target and `dashboard-third-party-licenses.txt`;
   - the notes are the changelog section;
   - an archive downloads and checks out as the README's [install section](README.md#download-a-release) says: its hash against `SHA256SUMS`, and its attestation with `gh attestation verify`. So do the two scripts;
   - the binary runs (`open-ferry -h`);
   - the image is on GHCR, tagged `X.Y.Z` and, unless it's a pre-release, `latest`; it runs (`docker run --rm ghcr.io/loft-902-co-llc/open-ferry:X.Y.Z open-ferry -h`), and its attestation checks out: `gh attestation verify oci://ghcr.io/loft-902-co-llc/open-ferry:X.Y.Z --repo Loft-902-Co-LLC/open-ferry-ai-proxy`;
   - after the first release only: the package `open-ferry` is public. A new GHCR package may start private; make it public in its settings (**Package settings**, **Change visibility**), or pulls fail for everyone else.

8. **Publish** the draft. It then becomes the latest release, which the management API's `latest-version` route reports to the running proxies. A pre-release doesn't.

9. **Try the install scripts.** They download from published releases only, so try them now, on Linux or macOS and on Windows, with the one-liners in the README's [install section](README.md#install-script).

## Dry run

Run the **Release** workflow from the Actions tab (**Run workflow**), on `main` or any branch. It does the following:
- builds every target;
- packages the archives;
- writes `SHA256SUMS`;
- builds the container image for linux/amd64 and linux/arm64 and tests the amd64 one, as on a tag.

It keeps them all as workflow artifacts, so they can be downloaded and tried; the image is `container-image`, an OCI archive of both platforms, for `docker load` or `skopeo`. It doesn't check the tag or the changelog, signs nothing, attests nothing, pushes no image and publishes nothing. The archives and the image are named after the version in `Cargo.toml`, whatever it is.

## The release key

open-ferry checks a release's signature before it installs it as an update (see [docs/updates.md](docs/updates.md)). The private half of a [minisign](https://jedisct1.github.io/minisign/) Ed25519 key signs each release's `SHA256SUMS`; its public half is in [`release-keys.pub`](release-keys.pub), which the binary builds in and the release workflow checks the signature against. That file holds at most two keys, so that a key can be rotated. While it holds none, a build trusts no release key and never updates itself, and the sign job fails.

### Making the key (once)

On a machine you trust, with [minisign](https://jedisct1.github.io/minisign/) installed:

```sh
minisign -G -W -p open-ferry-release.pub -s open-ferry-release.key
```

`-W` leaves the private key without a password. GitHub keeps the secret encrypted and gives it only to the approved job, so a password would be a second secret kept beside it, and the job would have to type it in. Then:

1. Put the private key straight into the secret, and into your password manager:

   ```sh
   gh secret set MINISIGN_SECRET_KEY --env release-signing --repo Loft-902-Co-LLC/open-ferry-ai-proxy < open-ferry-release.key
   ```

   Save the file's two lines as a secure note in your password manager. Then delete the file (`shred -u open-ferry-release.key`, or delete it and empty the bin). Don't commit it, mail it or paste it anywhere else.
2. Add the public key to `release-keys.pub`: the second line of `open-ferry-release.pub`, the one starting `RW`. Keep the file's comments. Merge it like any change; builds from then on trust it.

### The environment

The secret lives in the GitHub environment `release-signing` (**Settings**, **Environments**), not in the repository's secrets, so only the sign job can read it, and only once it's approved:
- **Required reviewers:** you. Leave **Prevent self-review** off if you are the only maintainer, or you can't approve your own release.
- **Deployment branches and tags:** selected tags only, the pattern `v*`, so no branch, and no pull request, can use the key.
- **Secret:** `MINISIGN_SECRET_KEY`, the whole private key file. No other secret.

Each release's sign job waits until you approve it (step 6). A run you didn't start, or of a tag you didn't push, should be rejected.

### Rotating the key

A binary updates only to a release signed with a key it trusts, so trust the new key before you sign with it:

1. Make a new key as above, but don't touch the secret yet. Keep the new private key in your password manager.
2. Add the new public key to `release-keys.pub`, below the current one, and release. That release is still signed with the current key, so every install can update to it, and from then on trusts both.
3. Once most installs have that release, replace the secret with the new private key (`gh secret set` again) and release. That release is signed with the new key.
4. In a later release, take the old key out of `release-keys.pub`, and delete the old private key from your password manager.

An install too old to trust the new key says that `SHA256SUMS` is signed with a key it doesn't trust, and installs nothing. It needs updating by hand, with the install script or a download.

### If the private key leaks

The updater downloads only from this repository's GitHub releases, over HTTPS, so a leaked key alone doesn't push an update to anyone: an attacker would also need to publish a release here. Still, act at once:

1. Delete the secret from the `release-signing` environment, so no job can sign with it, and check the repository's releases, tags and recent workflow runs for any you didn't make. If the leak came through GitHub, rotate your GitHub credentials too.
2. Make a new key, and put only its public half in `release-keys.pub`. Put the new private key in the secret.
3. Release a new version, signed with the new key. Never sign anything with the leaked key again, not even to move installs on: anyone could sign the same way.
4. Publish a security advisory (see [SECURITY.md](SECURITY.md)). Installs that trust only the leaked key can't update to the new release on their own; they say that `SHA256SUMS` is signed with a key they don't trust. Tell users to install the new release by hand, and to check its signature with the new key.

## When something fails

- **If the sign job fails,** its error says why. The secret `MINISIGN_SECRET_KEY` may be missing from the `release-signing` environment, or not match a key in `release-keys.pub`, or be a key with a password (see [The release key](#the-release-key)). Fix that, then re-run the failed jobs; nothing was published.
- **Before the draft is created:** if the failure was a passing fault, such as a runner or network problem, re-run the failed jobs. Otherwise, fix it on `main`, then move the tag to the fixed commit. Delete the tag on GitHub and locally (`git push origin :refs/tags/vX.Y.Z`, `git tag -d vX.Y.Z`), and tag again.
- **After the draft is created:** delete the draft before re-running, since a tag can have only one release. Until the release is published, the tag can still be moved as above.
- **If only the image job failed,** re-run that job: the draft stays. A re-run after the tag moved pushes the new image under the same tags; delete the old one's version on the package's page on GitHub. If you abandon a release whose image was pushed, delete that version there too, and if it took `latest`, push the previous release's image as `latest` again (with `docker buildx imagetools create`).
- **After publishing:** never move or reuse a published tag. Fix it in a new patch release.

A run that failed may have made attestations already. They stay, but they only vouch for the exact files they name, so they do no harm.

## Notes

- **Runners.** The workflow pins its runner labels:

  | Target | Runner |
  |---|---|
  | `x86_64-unknown-linux-gnu` | `ubuntu-24.04`, in the Debian 11 container |
  | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm`, in the Debian 11 container |
  | `x86_64-unknown-linux-musl` | `ubuntu-24.04`, in the Rust Alpine image |
  | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm`, in the Rust Alpine image |
  | `x86_64-apple-darwin` | `macos-15-intel` |
  | `aarch64-apple-darwin` | `macos-15` |
  | `x86_64-pc-windows-msvc` | `windows-2025` |

  Each target builds natively on a runner of its own architecture. Move the labels on when GitHub deprecates an image (see the [runner images](https://github.com/actions/runner-images) announcements).
- **glibc.** The Linux binaries need glibc 2.31 or newer, as on Debian 11 and Ubuntu 20.04. A binary needs the glibc of the system it was linked on, so the Linux targets don't build on the runner's Ubuntu but in a container of Debian 11 (bullseye): the official Rust image, `rust:1-bullseye`, pinned by digest in the workflow's `container`. The image brings GCC, which aws-lc-rs compiles its C code with, and rustup, which installs the toolchain `rust-toolchain.toml` names. Each build's summary shows the newest glibc symbol the binary uses, and the build fails if it is newer than `GLIBC_FLOOR` (2.31).
  - Debian 11's long-term support ended in August 2026. That matters little for an image that only builds, and the digest keeps it as it is, so keep the pin unless a build needs a newer C compiler.
  - Moving to a newer image, such as Debian 12 (glibc 2.36), raises the floor. Change `GLIBC_FLOOR`, the README's install section and this paragraph with it, and `GLIBC_FLOOR` in `install.sh`, which picks the static build below it, and mention it in the changelog.
- **The static Linux binaries.** The `-linux-musl` targets build in the official Rust image on Alpine, `rust:1-alpine3.24`, pinned by digest in the workflow (`RUST_ALPINE`). A step starts it with `docker run`, rather than making it the job's container, as GitHub runs no JavaScript action in an Alpine container on arm64. In it, `.github/scripts/build-musl.sh` installs the toolchain and builds natively: Alpine's GCC and musl-dev compile the C code of aws-lc-rs and SQLite, and `+crt-static` is set, though it's the default. Then `.github/scripts/check-static.sh` checks with `readelf` that the binary has no program interpreter and no shared libraries, and `open-ferry -h` runs in plain Alpine (`ALPINE`, pinned by digest), which has no glibc. Either failing fails the build. The binaries need no C library at all, so they run on any Linux of their architecture.
- **The container image.** [`docker/Dockerfile`](docker/Dockerfile) builds it from the two static archives, each checked against `SHA256SUMS`, on Alpine 3.24, with the time zone database from Google's distroless static image; both are pinned by digest. No stage runs code for the target platform, so the arm64 image builds on the amd64 runner without emulation. The `image` job runs BuildKit in a container of its own (`BUILDKIT`, pinned by digest), builds both platforms with `.github/scripts/build-image.sh`, and tests the amd64 image with `.github/scripts/test-image.sh`: its paths, and a server that answers with a dummy credential and nothing it can reach outside. On a tag, it then pushes the image to `ghcr.io/loft-902-co-llc/open-ferry` as the version and, unless it's a pre-release, `latest`, and attests it, with the attestation pushed to the registry too. It's the only job with `packages: write`.
  - It runs after the draft is created, so its push is the one thing public before you publish. Review the draft soon after.
  - The image's timestamps are the run's build date (`SOURCE_DATE_EPOCH`), the one the binaries report, so the job's three builds (the archive, the test and the push) make one image, and so does a re-run of the image job alone.
  - To move to a newer Alpine, change its digest in the Dockerfile (`ALPINE`), the release workflow (`ALPINE`) and the CI workflow's `install-scripts` job together. The Rust image (`RUST_ALPINE`) can move on its own.
- **Signing.** The sign job installs minisign from Ubuntu's package, rather than building a tool of our own: it is the reference implementation, by the author of `minisign-verify`, which open-ferry checks signatures with, and so there's no signing code here to get wrong. The job writes the key to a file only it can read, signs with `-t "open-ferry X.Y.Z SHA256SUMS"`, deletes the file when the step ends, and checks the signature with each key in `release-keys.pub` (with `-H`, as open-ferry accepts only prehashed signatures), and that its trusted comment names this release.
- **The install scripts.** `install.sh` and `install.ps1` are release assets, in `SHA256SUMS` and attested, downloaded by the README's one-liners from the latest release. CI tests them on Linux, macOS and Windows against fake releases served from 127.0.0.1, in `tests/install/`, and lints `install.sh` with ShellCheck. A new target needs adding to `install.sh`'s detection, or to `install.ps1` for Windows, and to the tests.
- **macOS.** The binaries run on macOS 11 or newer, set by `MACOSX_DEPLOYMENT_TARGET`.
- **Windows.** The binary links the C runtime statically, so it doesn't need the Visual C++ Redistributable.
- **Build information.** `OPEN_FERRY_COMMIT` and `OPEN_FERRY_BUILD_DATE` are set at build time. They are the management API's `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` headers. A build without them says `none` and `unknown`.
- **No cache.** Release builds start from scratch, so nothing a cache holds can get into a release.
- **The dashboard.** The `dashboard` job builds the web app in `dashboard/` once, with the Node version in `dashboard/.nvmrc`, `npm ci --ignore-scripts` and `npm run build`, without an npm cache. Every build job downloads the result into `dashboard/dist` and builds with `OPEN_FERRY_REQUIRE_DASHBOARD=1`, so a binary can't ship without it. Each archive's `licenses/dashboard-third-party-licenses.txt` lists the npm packages built into the dashboard, with their licenses. The dashboard build fails if one of them isn't MIT, ISC, Apache-2.0 or BSD; replace the package, or settle its license, before releasing. To move to a newer Node, change `.nvmrc`; `engines.node` in `dashboard/package.json` is the oldest Node that builds it.
- **The Rust crates' licenses.** The `rust-licenses` job runs [cargo-about](https://github.com/EmbarkStudios/cargo-about) 0.9.2, installed with `--locked`, through `.github/scripts/rust-licenses.py`, on the crates `cargo fetch --locked` downloads. For each target, it takes the crates in that target's build of `open-ferry`, build-time crates included, and writes their license texts with the crates each covers. It adds a crate's own license files where cargo-about gave a license's standard text instead, and every NOTICE file at the root of a crate. Each archive's `licenses/rust-third-party-licenses.txt` is the file for its target.
  - The job fails if a crate's license isn't in `about.toml`'s accepted list: MIT, Apache-2.0, ISC, BSD-3-Clause, Unicode-3.0, Zlib and CC0-1.0, which is what the release targets' crates use today. CI runs the same job on every pull request, so a crate under another license fails there first. Widen the list only for a permissive license; otherwise replace the crate.
  - To run it locally: `cargo install cargo-about --version 0.9.2 --locked --features cli`, `cargo fetch --locked`, then `python3 .github/scripts/rust-licenses.py <dir> [<target>...]`.
  - A new release target needs adding to the script's `TARGETS` too; until it is, its build fails at packaging.
- **Actions** are pinned to full commit SHAs, and Dependabot proposes updates.
- **Attestations** need the repository to be public, or on GitHub Enterprise Cloud if it is private.
- **Tag protection.** Pushing a `v*` tag starts a release, so the repository's "Protect release tags" ruleset lets only its admins create, move or delete one.
