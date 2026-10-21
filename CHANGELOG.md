# Changelog

All notable changes to open-ferry are recorded here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and open-ferry follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0.0, a minor version may change behaviour.

Where open-ferry differs from CLIProxyAPI, which it ports, is in [UPSTREAM.md](UPSTREAM.md). How to move over is in the [migration guide](docs/migrating-from-cliproxyapi.md).

## [Unreleased]

The first release, 0.1.0, isn't out yet. The releases below are candidates for it, published to test updating from one signed release to the next. This is what is in place today, ported from CLIProxyAPI v8.0.38.

### Added

- **Offline differential parity in CI**: relevant pull requests and merge-queue entries run every hand-written case and a fixed-seed random set against the pinned CLIProxyAPI source, as part of `check (ubuntu-latest)`. A separate daily and manually dispatched run uses a larger set with a rotating seed. Unexpected differences fail the run, with synthetic failing inputs kept for 14 days ([parity in CI](tools/parity/README.md#in-ci)).
