# Contributing

Thanks for helping. open-ferry is a credited Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). Most work is either porting upstream behaviour faithfully or building the parts on the [roadmap](ROADMAP.md).

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

- **Open an issue first for anything larger than a small fix,** so we can agree on the approach before you spend time on it.
- **Security problems:** see [SECURITY.md](SECURITY.md). Don't open public issues for them.
- **We won't merge client impersonation.** That covers:
  - TLS fingerprinting;
  - made-up user agents, client headers, or session or device IDs;
  - "cloaking";
  - sign-ins that borrow another company's app identity.

  [ROADMAP.md](ROADMAP.md#not-planned) lists what is out for this reason.

## Porting from CLIProxyAPI

Read [UPSTREAM.md](UPSTREAM.md) first. In short:

- **Port behaviour, not lines.** Match upstream's output, and check it against upstream with the parity tool (`tools/parity`) where it covers your change.
- **Headers.** Every ported file starts with a header naming its upstream source and the pinned version:

  ```rust
  // Ported from CLIProxyAPI internal/translator/codex/claude (v8.0.15, MIT).
  // https://github.com/router-for-me/CLIProxyAPI
  ```

- **Deviations.** Every deviation from upstream is documented in two places, with the reason: the module's `Deviations from upstream:` list, and UPSTREAM.md.
- **Tests.** Port upstream's tests along with its code, and name the upstream test in a comment. A test of our own says `Not upstream's:`.

## Rules for all code

- **No panics in non-test code:** no `unwrap`, `expect`, or unchecked indexing or slicing.
- **Never log a secret.** Tokens and keys are masked on disk and redacted in what clients see.
- **Tests don't touch the network.** They use loopback servers on ephemeral ports with dummy keys, listening on 127.0.0.1 rather than every interface (on Windows that would bring up a firewall prompt for each test binary built), and never real provider, OAuth or GitHub endpoints.

## Checks

CI runs these on Linux and Windows, and all must pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace
```

## Pull requests

- **Keep each PR focused.** Explain what changed and why, and link the issue.
- **Merging:**
  - only the maintainer merges;
  - `main` takes squash or rebase merges after review and green CI;
  - force pushes are blocked.
- **License.** By contributing, you agree that your contribution is licensed under the [MIT License](LICENSE).
