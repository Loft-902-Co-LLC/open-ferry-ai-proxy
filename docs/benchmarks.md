# Benchmarks

`tools/bench` runs open-ferry and CLIProxyAPI, one after the other, in front of the same fake upstream on 127.0.0.1, with the same config, and sends both the same requests: to each provider in its own format, and translated from another. It measures how long each takes to start, how many short requests it answers a second and how fast, how much time it adds to the upstream's, the CPU time it spends on each request, its memory, and how fast it passes on a long conversation.

The results below are from one run on 2026-10-08, against CLIProxyAPI v8.0.20, on an Azure Standard_D8s_v6 VM (8 vCPUs, Intel Xeon Platinum 8573C) in East US, made for the run and deleted after. Its CPUs were idle before the run. In short:

- **A long conversation** (a coding agent's 300 KiB request): open-ferry adds 4.4 to 14 ms to the upstream's time, and CLIProxyAPI 17 to 38 ms, 2.6 to 6.6 times as much. The gap is widest for Chat Completions sent on to Claude.
- **Short requests:** open-ferry adds 0.7 to 2.8 ms at the median, and CLIProxyAPI 0.6 to 5.0 ms, and both answer as many requests a second, which the fake upstream's 20 ms wait sets. With one client at a time open-ferry adds less, for 9 of the 10 kinds of request; with 16, CLIProxyAPI adds up to 0.4 ms less, for all 10; with 64 they are even at the median, and open-ferry's 99th percentile is lower for all 10.
- **CPU:** open-ferry spends 5% to 37% less CPU time per request in 29 of the 30 rows, and 2% more in the other.
- **Upstream connections:** open-ferry keeps its connections to the upstream and reuses them, opening at most 48 for 64 clients. CLIProxyAPI opened 705 to 6,297 with 16 and 64 clients, and with 64 every kind of request reached the benchmark's limit on connections and stopped early, after 3.2 to 6.5 s of its 10.
- **Start:** 15.5 ms to open-ferry's first answer, and 23.9 ms to CLIProxyAPI's (medians).
- **Memory:** open-ferry uses less when idle (29.6 MiB, against 53.0 MiB) and more under load (98.7 MiB against 93.9 MiB at the median, and 127 MiB against 107 MiB at the peak).

Another machine gives other numbers; [Running it](#running-it) says how to get your own.

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
bash run-ubuntu.sh --note "Azure Standard_D8s_v6, East US" --ref main --tag v8.0.20
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
| `--machine-note <text>` | | What the machine is, for the report, such as "Azure Standard_D8s_v6, East US" |
| `--prepare` | | Build CLIProxyAPI, check both binaries, and exit without measuring anything |
| `--out <file>` | | Also write the results to `<file>`, between its `bench-results` markers |

## Results

From the run described at the top of this page.

<!-- bench-results:start -->
<!-- Written by tools/bench's --out option. Don't edit it by hand: rerun the tool. -->
Run on 2026-10-08 on Ubuntu 24.04 (Linux 6.17.0-1022-azure), INTEL(R) XEON(R) PLATINUM 8573C with 8 logical CPUs and 31 GiB of memory. Before the run its CPUs were 0% busy.

The machine: Azure Standard_D8s_v6 (Intel Xeon Platinum 8573C, 8 vCPUs), East US

- open-ferry: release build of commit `f2fdb1e`
- CLIProxyAPI: v8.0.20 (commit `0f96f568e4db`), built with go1.26.4

The Rust toolchain was `rustc 1.99.0 (b940084d7 2026-09-28)`, and Go go1.26.4.

The fake upstream waited 20.0 ms before each answer. Each load level ran for 10.00 s. For the start time each proxy was started 5 times, after a start that wasn't measured. Each kind of long conversation request was sent 30 times, one after another.

#### What each proxy adds

The time a proxy adds to a request, on top of the fake upstream's: the latency at the 50th percentile, minus the fake upstream's own time for the same requests at the 50th percentile, which it measures from reading the request to handing over the last of its answer. Short requests are with one client at once, the long conversation one request after another. The tables below give it for every level.

| Request | open-ferry, short | CLIProxyAPI, short | open-ferry, long | CLIProxyAPI, long |
|---|---:|---:|---:|---:|
| Chat Completions | 668 µs | 787 µs | 4.35 ms | 18.7 ms |
| Chat Completions, streamed | 1.10 ms | 1.17 ms | 4.97 ms | 19.0 ms |
| Claude Messages | 711 µs | 840 µs | 5.77 ms | 22.5 ms |
| Claude Messages, streamed | 1.07 ms | 1.15 ms | 5.92 ms | 23.0 ms |
| Chat Completions → Claude Messages | 851 µs | 968 µs | 5.75 ms | 37.7 ms |
| Chat Completions → Claude Messages, streamed | 1.23 ms | 1.36 ms | 6.45 ms | 38.0 ms |
| Claude Messages → Chat Completions | 719 µs | 829 µs | 5.59 ms | 16.9 ms |
| Claude Messages → Chat Completions, streamed | 1.45 ms | 1.36 ms | 14.0 ms | 36.8 ms |
| Responses → Chat Completions | 758 µs | 832 µs | 5.85 ms | 23.6 ms |
| Responses → Chat Completions, streamed | 1.64 ms | 1.80 ms | 7.07 ms | 25.6 ms |

#### Start time

From starting the process to its first answer on `GET /v1/models`.

| Proxy | Median | Fastest | Slowest |
|---|---:|---:|---:|
| open-ferry | 15.5 ms | 15.3 ms | 15.9 ms |
| CLIProxyAPI | 23.9 ms | 22.8 ms | 24.0 ms |

#### Short requests

A system prompt and a one-line question, sent again and again by each number of clients at once. Latency is to the end of the answer; the proxy adds the p50 minus the fake upstream's own p50 for the same requests. CPU is the proxy's own user and system time, divided by the requests it answered. Upstream connections are the connections the proxy opened to the fake upstream. Each closed one holds a port of the machine for up to a minute, and the machine has 28,232 ports for outgoing connections (Linux's ip_local_port_range: ports 32768 to 60999, and TIME_WAIT for a minute). So a level stops early once the proxy has opened 7,000 within that time, a quarter of them, and the next waits until fewer than 3,500 were.

Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 668 µs | 542 µs | 0 | 0 |
| 1 | CLIProxyAPI | 48 | 21.0 ms | 21.2 ms | 21.4 ms | 787 µs | 796 µs | 0 | 0 |
| 16 | open-ferry | 756 | 21.1 ms | 21.5 ms | 22.1 ms | 950 µs | 532 µs | 0 | 15 |
| 16 | CLIProxyAPI | 764 | 20.8 ms | 21.2 ms | 21.9 ms | 698 µs | 647 µs | 0 | 2,262 |
| 64 | open-ferry | 3,047 | 20.9 ms | 21.4 ms | 22.0 ms | 782 µs | 489 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,054* | 20.8 ms | 21.2 ms | 22.6 ms | 645 µs | 605 µs | 0 | 4,737 |

Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.5 ms | 21.7 ms | 21.9 ms | 1.10 ms | 1.40 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.5 ms | 21.7 ms | 22.2 ms | 1.17 ms | 1.55 ms | 0 | 0 |
| 16 | open-ferry | 735 | 21.6 ms | 22.2 ms | 23.1 ms | 1.33 ms | 1.07 ms | 0 | 0 |
| 16 | CLIProxyAPI | 748 | 21.3 ms | 21.5 ms | 22.6 ms | 1.04 ms | 1.27 ms | 0 | 917 |
| 64 | open-ferry | 2,928 | 21.6 ms | 22.6 ms | 24.1 ms | 1.32 ms | 1.02 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,848* | 22.0 ms | 23.9 ms | 26.7 ms | 1.68 ms | 1.11 ms | 0 | 6,087 |

Claude Messages:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 711 µs | 542 µs | 0 | 0 |
| 1 | CLIProxyAPI | 48 | 21.0 ms | 21.2 ms | 21.4 ms | 840 µs | 861 µs | 0 | 0 |
| 16 | open-ferry | 756 | 21.1 ms | 21.6 ms | 22.1 ms | 973 µs | 554 µs | 0 | 15 |
| 16 | CLIProxyAPI | 764 | 20.9 ms | 21.2 ms | 22.2 ms | 732 µs | 711 µs | 0 | 1,969 |
| 64 | open-ferry | 3,037 | 21.0 ms | 21.5 ms | 22.2 ms | 857 µs | 516 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,048* | 20.8 ms | 21.2 ms | 22.6 ms | 711 µs | 680 µs | 0 | 5,031 |

Claude Messages, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.4 ms | 21.6 ms | 21.9 ms | 1.07 ms | 1.31 ms | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.4 ms | 21.6 ms | 21.8 ms | 1.15 ms | 1.39 ms | 0 | 0 |
| 16 | open-ferry | 739 | 21.5 ms | 22.1 ms | 22.9 ms | 1.26 ms | 992 µs | 0 | 0 |
| 16 | CLIProxyAPI | 752 | 21.2 ms | 21.3 ms | 22.7 ms | 973 µs | 1.19 ms | 0 | 705 |
| 64 | open-ferry | 2,965 | 21.4 ms | 22.1 ms | 23.0 ms | 1.16 ms | 958 µs | 0 | 1 |
| 64 | CLIProxyAPI | 2,921* | 21.5 ms | 22.8 ms | 25.8 ms | 1.29 ms | 1.06 ms | 0 | 6,297 |

Chat Completions → Claude Messages:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.2 ms | 21.4 ms | 21.6 ms | 851 µs | 805 µs | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.2 ms | 21.5 ms | 21.9 ms | 968 µs | 1.11 ms | 0 | 0 |
| 16 | open-ferry | 744 | 21.4 ms | 22.1 ms | 22.8 ms | 1.21 ms | 761 µs | 0 | 0 |
| 16 | CLIProxyAPI | 758 | 21.0 ms | 21.3 ms | 22.1 ms | 835 µs | 944 µs | 0 | 1,245 |
| 64 | open-ferry | 2,946 | 21.6 ms | 22.4 ms | 23.3 ms | 1.40 ms | 749 µs | 0 | 0 |
| 64 | CLIProxyAPI | 2,955* | 21.3 ms | 22.5 ms | 24.9 ms | 1.09 ms | 953 µs | 0 | 5,756 |

Chat Completions → Claude Messages, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.6 ms | 21.8 ms | 21.9 ms | 1.23 ms | 1.60 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.6 ms | 21.9 ms | 22.5 ms | 1.36 ms | 1.84 ms | 0 | 0 |
| 16 | open-ferry | 732 | 21.7 ms | 22.2 ms | 22.9 ms | 1.45 ms | 1.19 ms | 0 | 0 |
| 16 | CLIProxyAPI | 742 | 21.4 ms | 21.8 ms | 22.7 ms | 1.21 ms | 1.54 ms | 0 | 1,467 |
| 64 | open-ferry | 2,916 | 21.7 ms | 22.6 ms | 24.1 ms | 1.44 ms | 1.15 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,751* | 22.6 ms | 25.3 ms | 30.7 ms | 2.23 ms | 1.38 ms | 0 | 5,533 |

Claude Messages → Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 719 µs | 564 µs | 0 | 1 |
| 1 | CLIProxyAPI | 48 | 21.0 ms | 21.2 ms | 21.5 ms | 829 µs | 840 µs | 0 | 0 |
| 16 | open-ferry | 754 | 21.1 ms | 21.7 ms | 22.4 ms | 1.00 ms | 552 µs | 0 | 15 |
| 16 | CLIProxyAPI | 764 | 20.8 ms | 21.2 ms | 22.2 ms | 708 µs | 679 µs | 0 | 2,166 |
| 64 | open-ferry | 3,036 | 21.0 ms | 21.5 ms | 22.2 ms | 867 µs | 509 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,055* | 20.8 ms | 21.2 ms | 22.5 ms | 667 µs | 645 µs | 0 | 4,838 |

Claude Messages → Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.8 ms | 22.1 ms | 22.3 ms | 1.45 ms | 1.77 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.7 ms | 21.9 ms | 22.5 ms | 1.36 ms | 1.74 ms | 0 | 0 |
| 16 | open-ferry | 727 | 21.9 ms | 22.5 ms | 23.3 ms | 1.61 ms | 1.30 ms | 0 | 0 |
| 16 | CLIProxyAPI | 742 | 21.4 ms | 21.7 ms | 23.2 ms | 1.20 ms | 1.47 ms | 0 | 1,162 |
| 64 | open-ferry | 2,872 | 22.0 ms | 23.2 ms | 24.9 ms | 1.72 ms | 1.24 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,747* | 22.6 ms | 25.7 ms | 29.7 ms | 2.21 ms | 1.30 ms | 0 | 5,869 |

Responses → Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 758 µs | 607 µs | 0 | 0 |
| 1 | CLIProxyAPI | 48 | 21.0 ms | 21.2 ms | 21.8 ms | 832 µs | 903 µs | 0 | 0 |
| 16 | open-ferry | 751 | 21.2 ms | 21.8 ms | 22.4 ms | 1.08 ms | 599 µs | 0 | 0 |
| 16 | CLIProxyAPI | 763 | 20.9 ms | 21.2 ms | 22.0 ms | 735 µs | 740 µs | 0 | 2,036 |
| 64 | open-ferry | 3,007 | 21.2 ms | 21.8 ms | 22.6 ms | 1.05 ms | 558 µs | 0 | 0 |
| 64 | CLIProxyAPI | 3,034* | 20.9 ms | 21.5 ms | 23.0 ms | 750 µs | 708 µs | 0 | 4,964 |

Responses → Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 45 | 22.0 ms | 22.2 ms | 22.4 ms | 1.64 ms | 2.25 ms | 0 | 0 |
| 1 | CLIProxyAPI | 45 | 22.1 ms | 22.4 ms | 23.1 ms | 1.80 ms | 2.51 ms | 0 | 0 |
| 16 | open-ferry | 716 | 22.1 ms | 22.9 ms | 24.2 ms | 1.86 ms | 1.90 ms | 0 | 0 |
| 16 | CLIProxyAPI | 711 | 22.1 ms | 23.4 ms | 25.5 ms | 1.83 ms | 2.19 ms | 0 | 2,606 |
| 64 | open-ferry | 2,733 | 23.1 ms | 24.7 ms | 26.9 ms | 2.76 ms | 1.70 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,375* | 25.5 ms | 32.8 ms | 39.7 ms | 4.98 ms | 1.97 ms | 0 | 4,394 |

\* Stopped early, at the limit of upstream connections:

- CLIProxyAPI, Chat Completions, 64 clients: after 6.20 s
- CLIProxyAPI, Chat Completions, streamed, 64 clients: after 4.48 s
- CLIProxyAPI, Claude Messages, 64 clients: after 6.41 s
- CLIProxyAPI, Claude Messages, streamed, 64 clients: after 5.18 s
- CLIProxyAPI, Chat Completions → Claude Messages, 64 clients: after 5.19 s
- CLIProxyAPI, Chat Completions → Claude Messages, streamed, 64 clients: after 4.11 s
- CLIProxyAPI, Claude Messages → Chat Completions, 64 clients: after 6.45 s
- CLIProxyAPI, Claude Messages → Chat Completions, streamed, 64 clients: after 4.23 s
- CLIProxyAPI, Responses → Chat Completions, 64 clients: after 5.59 s
- CLIProxyAPI, Responses → Chat Completions, streamed, 64 clients: after 3.21 s

#### Memory

Resident memory (the working set on Windows), sampled every 50 ms.

| Proxy | Idle after start | Under load (median) | Peak |
|---|---:|---:|---:|
| open-ferry | 29.6 MiB | 98.7 MiB | 127.2 MiB |
| CLIProxyAPI | 53.0 MiB | 93.9 MiB | 106.7 MiB |

#### Long conversation

A coding agent's conversation: a system prompt, two tools, and 241 messages with a tool call every fourth turn; 300 KiB as a Chat Completions request (302 messages there), 303 KiB as a Claude Messages one, and 310 KiB as a Responses one (301 input items).

| Request | Proxy | p50 | p90 | Slowest | First bytes p50 | Proxy adds | Errors |
|---|---|---:|---:|---:|---:|---:|---:|
| Chat Completions | open-ferry | 24.8 ms | 25.2 ms | 26.4 ms | 24.8 ms | 4.35 ms | 0 |
| Chat Completions | CLIProxyAPI | 39.3 ms | 40.5 ms | 41.4 ms | 39.3 ms | 18.7 ms | 0 |
| Chat Completions, streamed | open-ferry | 25.7 ms | 26.2 ms | 27.0 ms | 25.0 ms | 4.97 ms | 0 |
| Chat Completions, streamed | CLIProxyAPI | 39.8 ms | 40.6 ms | 41.2 ms | 39.1 ms | 19.0 ms | 0 |
| Claude Messages | open-ferry | 26.4 ms | 27.0 ms | 27.3 ms | 26.4 ms | 5.77 ms | 0 |
| Claude Messages | CLIProxyAPI | 43.1 ms | 44.5 ms | 44.7 ms | 43.1 ms | 22.5 ms | 0 |
| Claude Messages, streamed | open-ferry | 26.8 ms | 27.1 ms | 27.4 ms | 26.0 ms | 5.92 ms | 0 |
| Claude Messages, streamed | CLIProxyAPI | 43.8 ms | 45.3 ms | 46.1 ms | 43.0 ms | 23.0 ms | 0 |
| Chat Completions → Claude Messages | open-ferry | 26.6 ms | 26.9 ms | 27.2 ms | 26.6 ms | 5.75 ms | 0 |
| Chat Completions → Claude Messages | CLIProxyAPI | 58.6 ms | 59.5 ms | 59.8 ms | 58.6 ms | 37.7 ms | 0 |
| Chat Completions → Claude Messages, streamed | open-ferry | 27.2 ms | 27.6 ms | 27.8 ms | 26.5 ms | 6.45 ms | 0 |
| Chat Completions → Claude Messages, streamed | CLIProxyAPI | 58.9 ms | 59.7 ms | 60.5 ms | 58.3 ms | 38.0 ms | 0 |
| Claude Messages → Chat Completions | open-ferry | 26.2 ms | 26.5 ms | 27.1 ms | 26.2 ms | 5.59 ms | 0 |
| Claude Messages → Chat Completions | CLIProxyAPI | 37.6 ms | 38.5 ms | 38.9 ms | 37.6 ms | 16.9 ms | 0 |
| Claude Messages → Chat Completions, streamed | open-ferry | 34.9 ms | 35.4 ms | 35.5 ms | 34.1 ms | 14.0 ms | 0 |
| Claude Messages → Chat Completions, streamed | CLIProxyAPI | 57.7 ms | 58.4 ms | 58.9 ms | 57.0 ms | 36.8 ms | 0 |
| Responses → Chat Completions | open-ferry | 26.5 ms | 26.8 ms | 27.0 ms | 26.5 ms | 5.85 ms | 0 |
| Responses → Chat Completions | CLIProxyAPI | 44.2 ms | 45.2 ms | 45.9 ms | 44.2 ms | 23.6 ms | 0 |
| Responses → Chat Completions, streamed | open-ferry | 27.9 ms | 28.3 ms | 28.5 ms | 26.8 ms | 7.07 ms | 0 |
| Responses → Chat Completions, streamed | CLIProxyAPI | 46.4 ms | 47.3 ms | 47.6 ms | 43.6 ms | 25.6 ms | 0 |

The fake upstream answered 604,245 requests, after 20.2 ms on average.
<!-- bench-results:end -->
