# Benchmarks

`tools/bench` runs open-ferry and CLIProxyAPI, one after the other, in front of the same fake upstream on 127.0.0.1, with the same config, and sends both the same requests. It measures how long each takes to start, how many short requests it answers a second and how fast, the CPU time it spends on each, its memory, and how fast it passes on a long conversation.

> [!WARNING]
> **The results below are preliminary.** They come from a run on a machine that was busy with other builds at the time, so they show the tool working and the rough shape of the numbers, not a result to quote. They go in the README only after a run on a quiet machine.

## What it measures

**The fake upstream** runs inside the benchmark, on an ephemeral port of 127.0.0.1. It answers `POST /v1/chat/completions` as an OpenAI-compatible provider and `POST /v1/messages` as Claude, each streamed or not as the request asks, after a fixed delay (20 ms by default). Every answer is the same sixteen words, as one message or as sixteen events, with usage. It counts any request to another path, which neither proxy should make, and the report says so if there was one.

**The proxies** run with the same config, written fresh for each run:

- the server on `127.0.0.1` and a port of its own (18317 by default), with one client key;
- an `openai-compatibility` provider with the model `bench-gpt` and a Claude API key with the model `bench-claude`, both pointing at the fake upstream;
- no management key (so no management API), no control panel, no mDNS announcements, no request log, no log file, no usage statistics, and no retries;
- CLIProxyAPI's Claude request cloaking off, as open-ferry has none, so both pass the client's request on as it came.

Each is started with `-config <file> -local-model`, in a directory of its own holding the config, the auth directory and a home and temporary directory, and with an environment holding little more than those. The environment's `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` point at a closed port on 127.0.0.1, so any request beyond loopback fails at once rather than leaving the machine: CLIProxyAPI fetches a version list at start, whatever its config says.

**The measures**, for each proxy:

- **Start time:** from starting the process to its first answer on `GET /v1/models`, polled every millisecond. The proxy is started once without measuring, which puts its binary in the OS's file cache, then five times measured; the report gives the median, fastest and slowest.
- **Short requests:** a system prompt and a one-line question, as a Chat Completions request to the OpenAI-compatible provider and as a Claude Messages request to the Claude one, each streamed and not. Each number of clients (1, 16 and 64) sends them for 10 seconds, each client sending its next request as soon as it has read the whole answer to the last. The report gives requests answered per second and the latency to the end of the answer at the 50th, 90th and 99th percentiles. Every answer is checked for the fake upstream's text, and anything else is counted as an error.
- **CPU time per request:** the proxy process's user and system CPU time, as the OS reports it, over each load level, divided by the requests answered. It includes whatever else the proxy did meanwhile.
- **Memory:** the proxy's resident memory (the working set on Windows), sampled every 50 ms: the median over the two seconds after it starts, the median under the short-request load, and the peak over the whole run.
- **Long conversation:** a coding agent's conversation of 120 turns: a 2 KB system prompt, two tools, and a tool call with a 2.4 KB result every fourth turn, about 300 KiB as either kind of request. Each kind, streamed and not, is sent 30 times, one after another, after two that aren't measured. The report gives the latency at the 50th and 90th percentiles, the slowest, and the median time to the answer's first bytes.

Each request goes to a provider of its own format, so this measures the proxies' handling of a request, not translation between formats. It doesn't measure real providers, TLS, OAuth credentials or WebSockets.

**Fairness.** The load generator and the fake upstream run on the same machine as the proxy and share its CPUs; the two proxies never run at the same time. open-ferry is the workspace's release build (`cargo build --release -p open-ferry`). CLIProxyAPI is built from a checkout with the flags of its release builds, but without cgo, which only its plugin loader needs. Go's and Tokio's worker threads both default to the number of logical CPUs. With the default 20 ms delay, 64 clients can make at most 3,200 requests a second.

## Running it

You need Git, Go 1.26 ([UPSTREAM.md](../UPSTREAM.md#checking-parity) says how to get Go 1.26.4) and a CLIProxyAPI checkout at the pinned tag beside this one. On a quiet machine, plugged in, with nothing else running, from the root of this repository:

```sh
git clone --branch v8.0.15 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo build --release --locked -p open-ferry
cargo run --release --locked -p open-ferry-bench -- --upstream ../CLIProxyAPI --go go1.26.4 --out docs/benchmarks.md
```

It builds CLIProxyAPI into `target/bench` (changing nothing in the checkout), runs both proxies, prints the results, and with `--out` writes them in place of the results below. A run takes about six minutes. The options:

| Option | Default | |
|---|---|---|
| `--upstream <dir>` | | A CLIProxyAPI checkout to build CLIProxyAPI from |
| `--cliproxyapi <file>` | | A CLIProxyAPI binary to run instead of building one |
| `--go <file>` | `go` | The Go command to build with |
| `--open-ferry <file>` | `target/release/open-ferry` | open-ferry's binary |
| `--work <dir>` | `target/bench` | Where the runs and the CLIProxyAPI build go |
| `--port <n>` | 18317 | The port each proxy listens on, on 127.0.0.1. Not 8317 or 8318, which a proxy in use may hold |
| `--delay-ms <n>` | 20 | The fake upstream's wait before each answer |
| `--duration <secs>` | 10 | How long each load level runs |
| `--concurrency <list>` | `1,16,64` | The numbers of clients at once |
| `--starts <n>` | 5 | Starts measured for the start time |
| `--long-requests <n>` | 30 | How many times each kind of long conversation request is sent |
| `--only <name>` | | Run only `open-ferry` or only `cliproxyapi` |
| `--out <file>` | | Also write the results to `<file>`, between its `bench-results` markers |

## Results

Preliminary: see the warning above.

<!-- bench-results:start -->
<!-- bench-results:end -->
