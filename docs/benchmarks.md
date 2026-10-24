# Benchmarks

`tools/bench` runs open-ferry and CLIProxyAPI, one after the other, in front of the same fake upstream on 127.0.0.1, with the same config, and sends both the same requests: to each provider in its own format, and translated from another. It measures how long each takes to start, how many short requests it answers a second and how fast, how much time it adds to the upstream's, the CPU time it spends on each request, its memory, and how fast it passes on a long conversation.

The results below are from one run on 2026-10-24, against CLIProxyAPI v8.0.38, on an Azure Standard_D8s_v6 VM (8 vCPUs, Intel Xeon Platinum 8573C) in East US, made for the run and deleted after. Its CPUs were idle before the run. In short:

- **A long conversation** (a coding agent's 300 KiB request): open-ferry adds 4.0 to 13.5 ms to the upstream's time, and CLIProxyAPI 17 to 42 ms, 2.8 to 6.1 times as much. The gap is widest for Chat Completions sent on to Claude.
- **Short requests:** open-ferry adds 0.5 to 2.2 ms at the median, and CLIProxyAPI 0.6 to 6.7 ms. With 1 and 16 clients both answer as many requests a second, which the fake upstream's 20 ms wait sets. With one client at a time open-ferry adds less, for all 10 kinds of request; with 16, CLIProxyAPI adds up to 0.3 ms less, for 8 of the 10; with 64, open-ferry adds less and its 99th percentile is lower, for all 10.
- **CPU:** open-ferry spends 7% to 41% less CPU time per request, in all 30 rows.
- **Upstream connections:** open-ferry keeps its connections to the upstream and reuses them, opening at most 48 for 64 clients. CLIProxyAPI opened 725 to 6,276 with 16 and 64 clients, and with 64 every kind of request reached the benchmark's limit on connections and stopped early, after 3.4 to 6.1 s of its 10.
- **Start:** 16.9 ms to open-ferry's first answer, and 27.2 ms to CLIProxyAPI's (medians).
- **Memory:** open-ferry uses less when idle (32.9 MiB, against 50.2 MiB) and more under load (104.7 MiB against 85.2 MiB at the median, and 136 MiB against 107 MiB at the peak). Most of open-ferry's idle memory is its own code and read-only data, which Linux counts once the process has touched it.

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

The current harness keeps `config.yaml` in each run's root and captures process output in `logs/stdout.log` and `logs/stderr.log`. The harness creates this subdirectory before starting either proxy. Appending log lines therefore stays outside the nonrecursive config-directory watch; the proxy watcher is unchanged. Use the same harness version and log layout for both binaries in a comparison. Comparisons with earlier harness versions must account for their root-level log files waking the config watcher.

**Connections to the upstream.** CLIProxyAPI sends these requests through Go's default HTTP transport, which keeps at most two idle connections per host. With more requests at once than that, it closes many connections after their answer and opens new ones (in the run below, about one for every two to four requests), which shows in its upstream connections and CPU time, and can stop its levels early. Against a real provider, each new connection costs a TLS handshake too, which the fake upstream, on plain HTTP, doesn't measure.

## Running it

### On a fresh Ubuntu 24.04 machine

`tools/bench/cloud/run-ubuntu.sh` does the whole run on a new Ubuntu 24.04 machine, such as a cloud VM with 8 dedicated vCPUs made for it and deleted after. Run it by hand, as root or as a user with sudo; CI never runs it. It:

1. installs `build-essential`, `pkg-config`, `cmake`, `git`, `curl` and `ca-certificates` with apt, if they're missing;
2. installs rustup, if it's missing, and the toolchain `rust-toolchain.toml` names;
3. downloads Go 1.26.4 for Linux from go.dev, checks its SHA-256, and unpacks it into `<dir>/sdk`;
4. fetches open-ferry at `--ref` (default `main`) into `<dir>/open-ferry` and CLIProxyAPI at `--tag` (default `v8.0.38`) into `<dir>/CLIProxyAPI`;
5. builds open-ferry and the benchmark in release mode, and CLIProxyAPI with the benchmark's `--prepare`;
6. waits until the 1-minute load average is at most `--max-load` (default 0.5), for up to `--wait` minutes (default 30);
7. runs the benchmark with `--out docs/benchmarks.md` and `--machine-note`, and copies the updated `docs/benchmarks.md`, the results and a log of everything into `<dir>/results/<time>/`.

`<dir>` is `~/ofp-bench` unless `--dir` says otherwise. Running it again is safe: it installs only what's missing, fetches the refs again, and builds only what changed. On the machine:

```sh
curl -fsSLO https://raw.githubusercontent.com/Loft-902-Co-LLC/open-ferry-ai-proxy/main/tools/bench/cloud/run-ubuntu.sh
bash run-ubuntu.sh --note "Azure Standard_D8s_v6, East US" --ref main --tag v8.0.38
```

Then copy `~/ofp-bench/results/<time>/` off the machine, such as with `scp -r`. Options after `--` go to the benchmark, such as `-- --duration 2 --starts 1 --long-requests 2` for a quick check that everything works. `bash run-ubuntu.sh --help` lists the script's options.

### By hand

You need Git, Go 1.26 ([UPSTREAM.md](../UPSTREAM.md#checking-parity) says how to get Go 1.26.4) and a CLIProxyAPI checkout at the pinned tag beside this one. On a quiet machine, plugged in, with nothing else running, from the root of this repository:

```sh
git clone --branch v8.0.38 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
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
Run on 2026-10-24 on Ubuntu 24.04 (Linux 7.0.0-1017-azure), INTEL(R) XEON(R) PLATINUM 8573C with 8 logical CPUs and 31 GiB of memory. Before the run its CPUs were 0% busy.

The machine: Azure Standard_D8s_v6, East US

- open-ferry: release build of commit `4f7aa5b`
- CLIProxyAPI: v8.0.38 (commit `0db190255716`), built with go1.26.4

The Rust toolchain was `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, and Go go1.26.4.

Each run keeps `config.yaml` in its root and captures proxy output in `logs/stdout.log` and `logs/stderr.log`. The logs directory is created before the proxy starts, keeping log writes outside its nonrecursive config watch.

The fake upstream waited 20.0 ms before each answer. Each load level ran for 10.00 s. For the start time each proxy was started 5 times, after a start that wasn't measured. Each kind of long conversation request was sent 30 times, one after another.

#### What each proxy adds

The time a proxy adds to a request, on top of the fake upstream's: the latency at the 50th percentile, minus the fake upstream's own time for the same requests at the 50th percentile, which it measures from reading the request to handing over the last of its answer. Short requests are with one client at once, the long conversation one request after another. The tables below give it for every level.

| Request | open-ferry, short | CLIProxyAPI, short | open-ferry, long | CLIProxyAPI, long |
|---|---:|---:|---:|---:|
| Chat Completions | 637 µs | 764 µs | 3.98 ms | 18.4 ms |
| Chat Completions, streamed | 952 µs | 1.28 ms | 4.99 ms | 18.9 ms |
| Claude Messages | 644 µs | 794 µs | 6.97 ms | 25.8 ms |
| Claude Messages, streamed | 1.04 ms | 1.22 ms | 7.80 ms | 26.1 ms |
| Chat Completions → Claude Messages | 786 µs | 962 µs | 6.77 ms | 41.5 ms |
| Chat Completions → Claude Messages, streamed | 1.11 ms | 1.39 ms | 7.93 ms | 41.5 ms |
| Claude Messages → Chat Completions | 623 µs | 845 µs | 5.53 ms | 17.0 ms |
| Claude Messages → Chat Completions, streamed | 1.34 ms | 1.39 ms | 13.5 ms | 37.5 ms |
| Responses → Chat Completions | 693 µs | 858 µs | 5.92 ms | 21.7 ms |
| Responses → Chat Completions, streamed | 1.42 ms | 1.80 ms | 6.88 ms | 24.1 ms |

#### Start time

From starting the process to its first answer on `GET /v1/models`.

| Proxy | Median | Fastest | Slowest |
|---|---:|---:|---:|
| open-ferry | 16.9 ms | 16.3 ms | 17.1 ms |
| CLIProxyAPI | 27.2 ms | 26.3 ms | 28.3 ms |

#### Short requests

A system prompt and a one-line question, sent again and again by each number of clients at once. Latency is to the end of the answer; the proxy adds the p50 minus the fake upstream's own p50 for the same requests. CPU is the proxy's own user and system time, divided by the requests it answered. Upstream connections are the connections the proxy opened to the fake upstream. Each closed one holds a port of the machine for up to a minute, and the machine has 28,232 ports for outgoing connections (Linux's ip_local_port_range: ports 32768 to 60999, and TIME_WAIT for a minute). So a level stops early once the proxy has opened 7,000 within that time, a quarter of them, and the next waits until fewer than 3,500 were.

Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.2 ms | 637 µs | 501 µs | 0 | 0 |
| 1 | CLIProxyAPI | 48 | 21.0 ms | 21.2 ms | 21.5 ms | 764 µs | 798 µs | 0 | 0 |
| 16 | open-ferry | 760 | 21.0 ms | 21.4 ms | 21.9 ms | 833 µs | 463 µs | 0 | 15 |
| 16 | CLIProxyAPI | 764 | 20.9 ms | 21.3 ms | 22.1 ms | 727 µs | 672 µs | 0 | 2,453 |
| 64 | open-ferry | 3,088 | 20.6 ms | 20.9 ms | 21.3 ms | 524 µs | 399 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,043* | 20.8 ms | 21.1 ms | 23.6 ms | 641 µs | 629 µs | 0 | 4,545 |

Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.3 ms | 21.5 ms | 21.7 ms | 952 µs | 1.13 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.7 ms | 21.9 ms | 22.3 ms | 1.28 ms | 1.67 ms | 0 | 0 |
| 16 | open-ferry | 744 | 21.4 ms | 21.9 ms | 22.6 ms | 1.10 ms | 863 µs | 0 | 0 |
| 16 | CLIProxyAPI | 745 | 21.3 ms | 21.6 ms | 22.7 ms | 1.11 ms | 1.37 ms | 0 | 1,275 |
| 64 | open-ferry | 2,980 | 21.2 ms | 22.0 ms | 23.1 ms | 1.01 ms | 838 µs | 0 | 0 |
| 64 | CLIProxyAPI | 2,822* | 22.1 ms | 24.2 ms | 29.2 ms | 1.78 ms | 1.21 ms | 0 | 5,727 |

Claude Messages:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 644 µs | 564 µs | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.1 ms | 21.3 ms | 21.4 ms | 794 µs | 863 µs | 0 | 0 |
| 16 | open-ferry | 758 | 21.0 ms | 21.5 ms | 22.0 ms | 886 µs | 501 µs | 0 | 15 |
| 16 | CLIProxyAPI | 761 | 20.9 ms | 21.3 ms | 22.3 ms | 788 µs | 754 µs | 0 | 2,195 |
| 64 | open-ferry | 3,075 | 20.7 ms | 21.1 ms | 21.6 ms | 594 µs | 440 µs | 0 | 48 |
| 64 | CLIProxyAPI | 2,973* | 21.1 ms | 22.5 ms | 25.4 ms | 982 µs | 749 µs | 0 | 4,806 |

Claude Messages, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.4 ms | 21.6 ms | 21.8 ms | 1.04 ms | 1.31 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.6 ms | 21.8 ms | 22.1 ms | 1.22 ms | 1.55 ms | 0 | 0 |
| 16 | open-ferry | 738 | 21.6 ms | 22.1 ms | 22.9 ms | 1.32 ms | 1.05 ms | 0 | 0 |
| 16 | CLIProxyAPI | 749 | 21.2 ms | 21.5 ms | 22.5 ms | 1.05 ms | 1.30 ms | 0 | 1,222 |
| 64 | open-ferry | 2,962 | 21.4 ms | 22.2 ms | 23.5 ms | 1.18 ms | 1.01 ms | 0 | 1 |
| 64 | CLIProxyAPI | 2,895* | 21.6 ms | 23.1 ms | 26.5 ms | 1.40 ms | 1.16 ms | 0 | 5,780 |

Chat Completions → Claude Messages:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 47 | 21.2 ms | 21.4 ms | 21.5 ms | 786 µs | 762 µs | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.3 ms | 21.5 ms | 21.9 ms | 962 µs | 1.15 ms | 0 | 0 |
| 16 | open-ferry | 749 | 21.3 ms | 21.8 ms | 22.6 ms | 1.04 ms | 724 µs | 0 | 0 |
| 16 | CLIProxyAPI | 759 | 21.0 ms | 21.2 ms | 22.2 ms | 823 µs | 964 µs | 0 | 725 |
| 64 | open-ferry | 3,021 | 21.1 ms | 21.5 ms | 22.4 ms | 870 µs | 683 µs | 0 | 0 |
| 64 | CLIProxyAPI | 2,957* | 21.3 ms | 22.5 ms | 24.6 ms | 1.09 ms | 1.00 ms | 0 | 6,276 |

Chat Completions → Claude Messages, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.5 ms | 21.7 ms | 21.9 ms | 1.11 ms | 1.42 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.7 ms | 22.0 ms | 22.5 ms | 1.39 ms | 1.90 ms | 0 | 0 |
| 16 | open-ferry | 734 | 21.7 ms | 22.2 ms | 23.0 ms | 1.42 ms | 1.17 ms | 0 | 0 |
| 16 | CLIProxyAPI | 741 | 21.4 ms | 21.7 ms | 22.9 ms | 1.24 ms | 1.65 ms | 0 | 1,213 |
| 64 | open-ferry | 2,927 | 21.6 ms | 22.5 ms | 23.8 ms | 1.40 ms | 1.12 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,768* | 22.5 ms | 24.9 ms | 29.7 ms | 2.14 ms | 1.48 ms | 0 | 5,787 |

Claude Messages → Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 20.9 ms | 21.1 ms | 21.3 ms | 623 µs | 543 µs | 0 | 1 |
| 1 | CLIProxyAPI | 47 | 21.0 ms | 21.2 ms | 21.5 ms | 845 µs | 863 µs | 0 | 0 |
| 16 | open-ferry | 760 | 21.0 ms | 21.4 ms | 22.0 ms | 854 µs | 497 µs | 0 | 15 |
| 16 | CLIProxyAPI | 764 | 20.9 ms | 21.3 ms | 22.2 ms | 725 µs | 707 µs | 0 | 2,298 |
| 64 | open-ferry | 3,075 | 20.7 ms | 21.1 ms | 21.6 ms | 602 µs | 437 µs | 0 | 48 |
| 64 | CLIProxyAPI | 3,054* | 20.8 ms | 21.2 ms | 22.5 ms | 676 µs | 667 µs | 0 | 4,703 |

Claude Messages → Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.7 ms | 22.0 ms | 22.2 ms | 1.34 ms | 1.74 ms | 0 | 0 |
| 1 | CLIProxyAPI | 46 | 21.8 ms | 22.0 ms | 22.2 ms | 1.39 ms | 1.87 ms | 0 | 0 |
| 16 | open-ferry | 735 | 21.6 ms | 22.3 ms | 23.4 ms | 1.36 ms | 1.38 ms | 0 | 0 |
| 16 | CLIProxyAPI | 741 | 21.4 ms | 21.8 ms | 23.2 ms | 1.23 ms | 1.56 ms | 0 | 858 |
| 64 | open-ferry | 2,872 | 22.0 ms | 23.3 ms | 25.1 ms | 1.74 ms | 1.28 ms | 0 | 1 |
| 64 | CLIProxyAPI | 2,680* | 22.9 ms | 27.1 ms | 33.3 ms | 2.55 ms | 1.40 ms | 0 | 6,144 |

Responses → Chat Completions:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 48 | 21.0 ms | 21.2 ms | 21.4 ms | 693 µs | 588 µs | 0 | 0 |
| 1 | CLIProxyAPI | 47 | 21.0 ms | 21.2 ms | 21.5 ms | 858 µs | 884 µs | 0 | 0 |
| 16 | open-ferry | 758 | 21.0 ms | 21.4 ms | 22.0 ms | 903 µs | 541 µs | 0 | 0 |
| 16 | CLIProxyAPI | 764 | 20.9 ms | 21.2 ms | 22.1 ms | 732 µs | 755 µs | 0 | 1,971 |
| 64 | open-ferry | 3,057 | 20.8 ms | 21.2 ms | 21.9 ms | 709 µs | 491 µs | 0 | 0 |
| 64 | CLIProxyAPI | 3,034* | 20.9 ms | 21.5 ms | 23.0 ms | 740 µs | 724 µs | 0 | 5,032 |

Responses → Chat Completions, streamed:

| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | Errors | Upstream connections |
|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | open-ferry | 46 | 21.8 ms | 22.0 ms | 22.2 ms | 1.42 ms | 1.94 ms | 0 | 0 |
| 1 | CLIProxyAPI | 45 | 22.2 ms | 22.5 ms | 23.0 ms | 1.80 ms | 2.52 ms | 0 | 0 |
| 16 | open-ferry | 734 | 21.6 ms | 22.1 ms | 23.2 ms | 1.40 ms | 1.71 ms | 0 | 0 |
| 16 | CLIProxyAPI | 715 | 22.0 ms | 23.0 ms | 24.7 ms | 1.80 ms | 2.29 ms | 0 | 2,255 |
| 64 | open-ferry | 2,803 | 22.5 ms | 24.0 ms | 26.0 ms | 2.19 ms | 1.54 ms | 0 | 0 |
| 64 | CLIProxyAPI | 2,232* | 27.2 ms | 35.9 ms | 41.6 ms | 6.67 ms | 2.04 ms | 0 | 4,748 |

\* Stopped early, at the limit of upstream connections:

- CLIProxyAPI, Chat Completions, 64 clients: after 6.07 s
- CLIProxyAPI, Chat Completions, streamed, 64 clients: after 4.12 s
- CLIProxyAPI, Claude Messages, 64 clients: after 4.13 s
- CLIProxyAPI, Claude Messages, streamed, 64 clients: after 4.66 s
- CLIProxyAPI, Chat Completions → Claude Messages, 64 clients: after 5.69 s
- CLIProxyAPI, Chat Completions → Claude Messages, streamed, 64 clients: after 4.48 s
- CLIProxyAPI, Claude Messages → Chat Completions, 64 clients: after 5.93 s
- CLIProxyAPI, Claude Messages → Chat Completions, streamed, 64 clients: after 4.39 s
- CLIProxyAPI, Responses → Chat Completions, 64 clients: after 6.06 s
- CLIProxyAPI, Responses → Chat Completions, streamed, 64 clients: after 3.44 s

#### Memory

Resident memory (the working set on Windows), sampled every 50 ms.

| Proxy | Idle after start | Under load (median) | Peak |
|---|---:|---:|---:|
| open-ferry | 32.9 MiB | 104.7 MiB | 136.0 MiB |
| CLIProxyAPI | 50.2 MiB | 85.2 MiB | 107.1 MiB |

#### Long conversation

A coding agent's conversation: a system prompt, two tools, and 241 messages with a tool call every fourth turn; 300 KiB as a Chat Completions request (302 messages there), 303 KiB as a Claude Messages one, and 310 KiB as a Responses one (301 input items).

| Request | Proxy | p50 | p90 | Slowest | First bytes p50 | Proxy adds | Errors |
|---|---|---:|---:|---:|---:|---:|---:|
| Chat Completions | open-ferry | 24.6 ms | 24.7 ms | 26.0 ms | 24.5 ms | 3.98 ms | 0 |
| Chat Completions | CLIProxyAPI | 39.0 ms | 40.1 ms | 41.2 ms | 39.0 ms | 18.4 ms | 0 |
| Chat Completions, streamed | open-ferry | 25.8 ms | 26.1 ms | 26.6 ms | 25.1 ms | 4.99 ms | 0 |
| Chat Completions, streamed | CLIProxyAPI | 39.7 ms | 40.7 ms | 41.0 ms | 39.0 ms | 18.9 ms | 0 |
| Claude Messages | open-ferry | 27.6 ms | 28.0 ms | 29.4 ms | 27.6 ms | 6.97 ms | 0 |
| Claude Messages | CLIProxyAPI | 46.5 ms | 47.2 ms | 47.7 ms | 46.5 ms | 25.8 ms | 0 |
| Claude Messages, streamed | open-ferry | 28.7 ms | 29.1 ms | 29.4 ms | 27.9 ms | 7.80 ms | 0 |
| Claude Messages, streamed | CLIProxyAPI | 47.0 ms | 48.0 ms | 48.5 ms | 46.3 ms | 26.1 ms | 0 |
| Chat Completions → Claude Messages | open-ferry | 27.7 ms | 28.0 ms | 28.1 ms | 27.7 ms | 6.77 ms | 0 |
| Chat Completions → Claude Messages | CLIProxyAPI | 62.4 ms | 63.3 ms | 63.7 ms | 62.4 ms | 41.5 ms | 0 |
| Chat Completions → Claude Messages, streamed | open-ferry | 28.8 ms | 29.2 ms | 29.3 ms | 28.1 ms | 7.93 ms | 0 |
| Chat Completions → Claude Messages, streamed | CLIProxyAPI | 62.6 ms | 63.4 ms | 64.4 ms | 61.9 ms | 41.5 ms | 0 |
| Claude Messages → Chat Completions | open-ferry | 26.2 ms | 26.4 ms | 26.7 ms | 26.2 ms | 5.53 ms | 0 |
| Claude Messages → Chat Completions | CLIProxyAPI | 37.7 ms | 38.5 ms | 39.5 ms | 37.7 ms | 17.0 ms | 0 |
| Claude Messages → Chat Completions, streamed | open-ferry | 34.4 ms | 35.1 ms | 35.3 ms | 33.8 ms | 13.5 ms | 0 |
| Claude Messages → Chat Completions, streamed | CLIProxyAPI | 58.4 ms | 59.5 ms | 60.6 ms | 57.8 ms | 37.5 ms | 0 |
| Responses → Chat Completions | open-ferry | 26.6 ms | 26.7 ms | 27.1 ms | 26.6 ms | 5.92 ms | 0 |
| Responses → Chat Completions | CLIProxyAPI | 42.4 ms | 43.2 ms | 43.9 ms | 42.4 ms | 21.7 ms | 0 |
| Responses → Chat Completions, streamed | open-ferry | 27.8 ms | 28.2 ms | 28.5 ms | 27.0 ms | 6.88 ms | 0 |
| Responses → Chat Completions, streamed | CLIProxyAPI | 45.0 ms | 46.0 ms | 46.3 ms | 43.8 ms | 24.1 ms | 0 |

The fake upstream answered 600,591 requests, after 20.2 ms on average.
<!-- bench-results:end -->
