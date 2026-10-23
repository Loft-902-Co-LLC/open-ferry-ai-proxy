# Proxy benchmark

Run the benchmark in release mode with an explicitly unused proxy port:

```sh
cargo run --release --locked -p open-ferry-bench -- --port 19327
```

Use `--help` for binaries, workloads, duration and output options. The complete methodology and recorded results are in [docs/benchmarks.md](../../docs/benchmarks.md).

Each proxy run gets an isolated directory under the configured work directory. It contains `config.yaml`, `auth/`, `home/`, `tmp/` and `logs/`. Captured process output is in `logs/stdout.log` and `logs/stderr.log`.

The harness creates the logs directory and opens both files before starting the proxy. Both proxies watch their config directory nonrecursively, so appending process output in a subdirectory avoids artificial watcher wakeups. The proxy watcher and captured output bytes are unchanged. Compare binaries using the same harness version and log layout.
