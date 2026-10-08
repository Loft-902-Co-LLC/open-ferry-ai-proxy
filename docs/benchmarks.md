# Benchmarks

`tools/bench` runs open-ferry and CLIProxyAPI, one after the other, in front of the same fake upstream on 127.0.0.1, with the same config, and sends both the same requests: to each provider in its own format, and translated from another. It measures how long each takes to start, how many short requests it answers a second and how fast, how much time it adds to the upstream's, the CPU time it spends on each request, its memory, and how fast it passes on a long conversation.

> [!WARNING]
> **The results below are preliminary.** They come from a run on a machine that was busy with other builds at the time, so they show the tool working and the rough shape of the numbers, not a result to quote. They go in the README only after a run on a quiet machine.

## What it measures

**The fake upstream** runs inside the benchmark, on an ephemeral port of 127.0.0.1. It answers `POST /v1/chat/completions` as an OpenAI-compatible provider and `POST /v1/messages` as Claude, each streamed or not as the request asks, after a fixed delay (20 ms by default). Every answer is the same sixteen words, as one message or as sixteen events, with usage. It:

- checks that each request is in its own provider's format, as a proxy that translates has to send it, and refuses one that isn't with a 400 in that provider's error format, saying why: for Chat Completions, no `system`, `input` or `instructions` field, messages with the roles and parts Chat Completions has, and tools and tool calls as functions; for Claude Messages, a `max_tokens`, only user and assistant messages, Claude's content blocks, and tools with an `input_schema`;
- measures its own time for each request, from having read it whole to handing over the last of its answer, so that the report can say what each proxy adds;
- sends each write at once, with Nagle's algorithm off, as Go does on every connection. With it on, each streamed event after the first would wait for the proxy to acknowledge the one before, which Linux delays for 40 ms, and every streamed answer would take 40 ms more there;
- counts the connections each proxy opens to it, any request it refuses, and any request to another path, which neither proxy should make. The report says so if there was one.

**The proxies** run with the same config, written fresh for each run:

- the server on `127.0.0.1` and a port of its own (18317 by default), with one client key;
- an `openai-compatibility` provider with the model `bench-gpt` and a Claude API key with the model `bench-claude`, both pointing at the fake upstream;
- no management key (so no management API), no control panel, no mDNS announcements, no request log, no log file, no usage statistics, and no retries;
- CLIProxyAPI's Claude request cloaking off, as open-ferry has none, so both pass the client's request on as it came.

Each is started with `-config <file> -local-model`, in a directory of its own holding the config, the auth directory and a home and temporary directory, and with an environment holding little more than those. The environment's `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` (and their lowercase forms) point at a closed port on 127.0.0.1, so any request beyond loopback fails at once rather than leaving the machine: CLIProxyAPI fetches a version list at start, whatever its config says.

**The requests**, each streamed and not. The model picks the provider; the client's path picks the format:

| Request | Client sends | Model | The fake upstream gets |
|---|---|---|---|
| Chat Completions | `POST /v1/chat/completions` | `bench-gpt` | Chat Completions, as it came |
| Claude Messages | `POST /v1/messages` | `bench-claude` | Claude Messages, as it came |
| Chat Completions → Claude Messages | `POST /v1/chat/completions` | `bench-claude` | Claude Messages |
| Claude Messages → Chat Completions | `POST /v1/messages` | `bench-gpt` | Chat Completions |
| Responses → Chat Completions | `POST /v1/responses` | `bench-gpt` | Chat Completions |

Both proxies serve each of these: CLIProxyAPI's Claude executor translates a Chat Completions request to Claude's format, and its OpenAI-compatible executor translates Claude Messages and Responses requests to Chat Completions and sends them to the provider's `/chat/completions`; open-ferry, a port of it, does the same. Before measuring, each kind is sent 20 times, one after another, and the run stops with an error unless all 20 answers came through and the fake upstream answered all 20 at its endpoint for the provider's format, refused none, and got nothing at its other endpoint or any other path.

Every answer the load generator gets is checked: it has to be in the client's format, whole (when streamed, with the format's last event: `finish_reason`, `message_stop` or `response.completed`), and carry the fake upstream's text, translated back if it was translated. Anything else is counted as an error.

**The measures**, for each proxy:

- **Start time:** from starting the process to its first answer on `GET /v1/models`, polled every millisecond. The proxy is started once without measuring, which puts its binary in the OS's file cache, then five times measured; the report gives the median, fastest and slowest.
- **Short requests:** a system prompt and a one-line question, of each kind above. Each number of clients (1, 16 and 64) sends them for 10 seconds, each client sending its next request as soon as it has read the whole answer to the last. The report gives requests answered per second and the latency to the end of the answer at the 50th, 90th and 99th percentiles.
- **What the proxy adds:** the latency at the 50th percentile minus the fake upstream's own time at the 50th percentile for the same requests, as "Proxy adds" in each table, and in a table of its own for one client at a time and the long conversation. It's the proxy's own time for a request plus the load generator's and the loopback hops, which are the same for both proxies. It's measured rather than worked out from the configured delay, which a busy machine stretches.
- **Upstream connections:** how many connections the proxy opened to the fake upstream over each load level. A proxy that keeps its connections open needs about one per client; one that closes them opens about one per request. Each closed connection holds a port of the machine for a while (TIME_WAIT), and the ports for outgoing connections are shared by every program on the machine. So a level stops early once the proxy has opened a quarter of those ports, rounded down to hundreds, within the TIME_WAIT time, and is marked so; its requests per second are over the time it ran. The next level waits until fewer than half that many were opened within that time. The rule is the same for both proxies, and only a proxy that opens many connections meets it:
  - Windows: 16,384 ports (49152 to 65535) and two minutes, its defaults, so 4,000 connections within two minutes.
  - Linux: the range in `/proc/sys/net/ipv4/ip_local_port_range` (32768 to 60999, 28,232 ports, by default) and a minute, which Linux doesn't let one change: 7,000 connections within a minute by default. Linux can reuse a port in TIME_WAIT for a new connection on loopback after a second (`net.ipv4.tcp_tw_reuse`, on for loopback by default), so the rule is cautious there.
  - macOS: 16,384 ports and 30 seconds, so 4,000 connections within 30 seconds.
- **CPU time per request:** the proxy process's user and system CPU time, as the OS reports it, over each load level, divided by the requests answered. It includes whatever else the proxy did meanwhile.
- **Memory:** the proxy's resident memory (the working set on Windows), sampled every 50 ms: the median over the two seconds after it starts, the median under the short-request load, and the peak over the whole run.
- **Long conversation:** a coding agent's conversation of 120 turns: a 2 KB system prompt, two tools, and a tool call with a 2.4 KB result every fourth turn, about 300 KiB as a request of any of the three formats. Each kind above is sent 30 times, one after another, after two that aren't measured. The report gives the latency at the 50th and 90th percentiles, the slowest, the median time to the answer's first bytes, and what the proxy adds.

It doesn't measure real providers, TLS, OAuth credentials or WebSockets.

**The machine.** The report gives the date, the OS and its kernel, the CPU, the number of logical CPUs, the memory and how busy the CPUs were in the second before the run, the `rustc -V` of the workspace's toolchain and the Go version CLIProxyAPI was built with, and what `--machine-note` says, such as a cloud VM's size and region. It says so if the benchmark itself is a debug build, whose load generator and fake upstream are slower.

**Fairness.** The load generator and the fake upstream run on the same machine as the proxy and share its CPUs; the two proxies never run at the same time. open-ferry is the workspace's release build (`cargo build --release -p open-ferry`). CLIProxyAPI is built from a checkout with the flags of its release builds, but without cgo, which only its plugin loader needs. Go's and Tokio's worker threads both default to the number of logical CPUs. With the default 20 ms delay, 64 clients can make at most 3,200 requests a second.

**Connections to the upstream.** CLIProxyAPI sends these requests through Go's default HTTP transport, which keeps at most two idle connections per host. With more requests at once than that, it closes many connections after their answer and opens new ones (in the run below, about one for every two to four requests), which shows in its upstream connections and CPU time, and can stop its levels early. Against a real provider, each new connection costs a TLS handshake too, which the fake upstream, on plain HTTP, doesn't measure.

## Running it

### On a fresh Ubuntu 24.04 machine

`tools/bench/cloud/run-ubuntu.sh` does the whole run on a new Ubuntu 24.04 machine, such as a cloud VM with 8 dedicated vCPUs made for it and deleted after. Run it by hand, as root or as a user with sudo; CI never runs it. It:

1. installs `build-essential`, `pkg-config`, `cmake`, `git`, `curl` and `ca-certificates` with apt, if they're missing;
2. installs rustup, if it's missing, and the toolchain `rust-toolchain.toml` names;
3. downloads Go 1.26.4 for Linux from go.dev, checks its SHA-256, and unpacks it into `<dir>/sdk`;
4. fetches open-ferry at `--ref` (default `main`) into `<dir>/open-ferry` and CLIProxyAPI at `--tag` (default `v8.0.20`) into `<dir>/CLIProxyAPI`;
5. builds open-ferry and the benchmark in release mode, and CLIProxyAPI with the benchmark's `--prepare`;
6. waits until the 1-minute load average is at most `--max-load` (default 0.5), for up to `--wait` minutes (default 30);
7. runs the benchmark with `--out docs/benchmarks.md` and `--machine-note`, and copies the updated `docs/benchmarks.md`, the results and a log of everything into `<dir>/results/<time>/`.

`<dir>` is `~/ofp-bench` unless `--dir` says otherwise. Running it again is safe: it installs only what's missing, fetches the refs again, and builds only what changed. On the machine:

```sh
curl -fsSLO https://raw.githubusercontent.com/Loft-902-Co-LLC/open-ferry-ai-proxy/main/tools/bench/cloud/run-ubuntu.sh
bash run-ubuntu.sh --note "Azure Standard_D8as_v5, eastus" --ref main --tag v8.0.20
```

Then copy `~/ofp-bench/results/<time>/` off the machine, such as with `scp -r`. Options after `--` go to the benchmark, such as `-- --duration 2 --starts 1 --long-requests 2` for a quick check that everything works. `bash run-ubuntu.sh --help` lists the script's options.

### By hand

You need Git, Go 1.26 ([UPSTREAM.md](../UPSTREAM.md#checking-parity) says how to get Go 1.26.4) and a CLIProxyAPI checkout at the pinned tag beside this one. On a quiet machine, plugged in, with nothing else running, from the root of this repository:

```sh
git clone --branch v8.0.20 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo build --release --locked -p open-ferry
cargo run --release --locked -p open-ferry-bench -- --upstream ../CLIProxyAPI --go go1.26.4 --machine-note "<what the machine is>" --out docs/benchmarks.md
```

It builds CLIProxyAPI into `target/bench` (changing nothing in the checkout, and using the build from before when it was of the same clean commit, with the same Go and flags), runs both proxies, prints the results, and with `--out` writes them in place of the results below. A run takes about 12 minutes, and longer when a level has to wait for ports to be freed. The options:

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
| `--machine-note <text>` | | What the machine is, for the report, such as "Azure Standard_D8as_v5, eastus" |
| `--prepare` | | Build CLIProxyAPI, check both binaries, and exit without measuring anything |
| `--out <file>` | | Also write the results to `<file>`, between its `bench-results` markers |

## Results

Preliminary: see the warning above. These are from before the translated requests, the "Proxy adds" columns, the machine note and the Linux port rule were added, against CLIProxyAPI v8.0.15; the next run replaces them in the new layout.

<!-- bench-results:start -->
<!-- Written by tools/bench's --out option. Don't edit it by hand: rerun the tool. -->
Run on 2026-10-07 on Windows 11 Pro, AMD Ryzen 7 5700X 8-Core Processor with 16 logical CPUs and 96 GiB of memory. Before the run its CPUs were 30% busy.

- open-ferry: release build of commit `9c9b3a5`
- CLIProxyAPI: v8.0.15 (commit `a4acc9f752bd`), built with go1.26.4

The fake upstream waited 20.0 ms before each answer. Each load level ran for 10.00 s. For the start time each proxy was started 5 times, after a start that wasn't measured. Each kind of long conversation request was sent 30 times, one after another.

#### Start time

From starting the process to its first answer on `GET /v1/models`.

| Proxy | Median | Fastest | Slowest |
|---|---:|---:|---:|
| open-ferry | 62.7 ms | 61.5 ms | 72.9 ms |
| CLIProxyAPI | 34.4 ms | 32.7 ms | 59.3 ms |

#### Short requests

A system prompt and a one-line question, sent again and again by each number of clients at once. Latency is to the end of the answer; CPU is the proxy's own user and system time, divided by the requests it answered. Upstream connections are the connections the proxy opened to the fake upstream. Each closed one holds a port of the machine for up to 2 minutes, so a level stops early once the proxy has opened 4,000 within that time, and the next waits until enough are freed.

Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.1 ms | 21.4 ms | 21.6 ms | 792 µs | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.4 ms | 21.7 ms | 24.6 ms | 1.11 ms | 0 | 0 |
| 16 | open-ferry | 743 | 21.5 ms | 22.0 ms | 22.5 ms | 675 µs | 0 | 15 |
| 16 | CLIProxyAPI | 743 | 21.5 ms | 22.1 ms | 23.5 ms | 1.02 ms | 0 | 3,779 |
| 64 | open-ferry | 2,982 | 21.4 ms | 22.0 ms | 23.0 ms | 525 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,012* | 21.0 ms | 21.6 ms | 22.8 ms | 793 µs | 0 | 4,000 |

Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 45 | 22.1 ms | 22.4 ms | 22.8 ms | 2.04 ms | 0 | 0 |
| 1 | CLIProxyAPI | 45 | 22.0 ms | 22.3 ms | 24.9 ms | 2.18 ms | 0 | 1 |
| 16 | open-ferry | 713 | 22.4 ms | 23.0 ms | 24.9 ms | 1.33 ms | 0 | 0 |
| 16 | CLIProxyAPI | 537 | 22.1 ms | 34.2 ms | 265.9 ms | 1.93 ms | 0 | 1,494 |
| 64 | open-ferry | 2,870 | 22.1 ms | 23.1 ms | 25.8 ms | 1.29 ms | 0 | 1 |
| 64 | CLIProxyAPI | 564* | 45.9 ms | 286.0 ms | 910.0 ms | 1.96 ms | 0 | 2,509 |

Claude Messages:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.4 ms | 21.6 ms | 22.8 ms | 1.10 ms | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.3 ms | 21.6 ms | 22.7 ms | 1.13 ms | 0 | 1 |
| 16 | open-ferry | 740 | 21.6 ms | 22.1 ms | 22.6 ms | 626 µs | 0 | 15 |
| 16 | CLIProxyAPI | 745 | 21.4 ms | 22.0 ms | 22.9 ms | 1.06 ms | 0 | 3,506 |
| 64 | open-ferry | 2,953 | 21.5 ms | 22.2 ms | 25.1 ms | 517 µs | 0 | 48 |
| 64 | CLIProxyAPI | 2,959* | 21.2 ms | 22.2 ms | 28.1 ms | 969 µs | 0 | 3,424 |

Claude Messages, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 45 | 22.3 ms | 22.7 ms | 23.0 ms | 2.15 ms | 0 | 0 |
| 1 | CLIProxyAPI | 45 | 22.0 ms | 22.2 ms | 22.6 ms | 1.99 ms | 0 | 1 |
| 16 | open-ferry | 714 | 22.3 ms | 22.9 ms | 24.3 ms | 1.43 ms | 0 | 0 |
| 16 | CLIProxyAPI | 726 | 21.9 ms | 22.7 ms | 24.1 ms | 2.08 ms | 0 | 2,706 |
| 64 | open-ferry | 2,873 | 22.1 ms | 23.0 ms | 24.8 ms | 1.30 ms | 0 | 1 |
| 64 | CLIProxyAPI | 1,427* | 33.2 ms | 82.7 ms | 161.3 ms | 1.85 ms | 0 | 3,389 |

\* Stopped early, at the limit of upstream connections:

- CLIProxyAPI, Chat Completions, 64 clients: after 4.90 s
- CLIProxyAPI, Chat Completions, streamed, 64 clients: after 7.71 s
- CLIProxyAPI, Claude Messages, 64 clients: after 3.65 s
- CLIProxyAPI, Claude Messages, streamed, 64 clients: after 4.24 s

#### Memory

Resident memory (the working set on Windows), sampled every 50 ms.

| Proxy | Idle after start | Under load (median) | Peak |
|---|---:|---:|---:|
| open-ferry | 21.8 MiB | 41.7 MiB | 47.5 MiB |
| CLIProxyAPI | 35.7 MiB | 51.9 MiB | 89.7 MiB |

#### Long conversation

A coding agent's conversation: a system prompt, two tools, and 241 messages with a tool call every fourth turn; 300 KiB as a Chat Completions request (302 messages there), 303 KiB as a Claude Messages one.

| Request | Proxy | p50 | p90 | Slowest | First bytes p50 | Errors |
|---|---|---:|---:|---:|---:|---:|
| Chat Completions | open-ferry | 27.8 ms | 28.7 ms | 29.0 ms | 27.8 ms | 0 |
| Chat Completions | CLIProxyAPI | 52.7 ms | 66.9 ms | 82.7 ms | 52.7 ms | 0 |
| Chat Completions, streamed | open-ferry | 27.9 ms | 29.7 ms | 53.8 ms | 27.2 ms | 0 |
| Chat Completions, streamed | CLIProxyAPI | 51.2 ms | 67.4 ms | 82.3 ms | 50.1 ms | 0 |
| Claude Messages | open-ferry | 27.5 ms | 28.8 ms | 29.9 ms | 27.5 ms | 0 |
| Claude Messages | CLIProxyAPI | 43.9 ms | 47.6 ms | 49.6 ms | 43.9 ms | 0 |
| Claude Messages, streamed | open-ferry | 27.5 ms | 28.8 ms | 42.8 ms | 26.5 ms | 0 |
| Claude Messages, streamed | CLIProxyAPI | 44.0 ms | 48.1 ms | 69.2 ms | 43.3 ms | 0 |

The fake upstream answered 213,772 requests, after 20.5 ms on average.
<!-- bench-results:end -->
