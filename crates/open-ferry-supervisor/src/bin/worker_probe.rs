//! Not upstream's: a stand-in worker for the control-channel tests. It takes
//! its channel the way the real worker does, attaches, writes what it was told
//! to `<config>/out`, serves one connection on each listener it was handed and
//! stops on `stop` or when the channel closes. `-config` is a directory here;
//! `<config>/mode` may say `min2` to demand supervisor protocol version 2,
//! `stall` to never greet, or `ignore` to ignore `stop` and the channel's end.
//! `delayed-stall` (Windows) connects 5 s in, writes `<config>/connected`
//! and then never greets. It ends itself after two minutes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use open_ferry_supervisor::protocol::Message;
use open_ferry_supervisor::worker::{self, Hello};
use tokio::io::AsyncWriteExt;

fn flag(args: &[OsString], name: &str) -> Option<OsString> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let (Some(config), Some(control)) = (flag(&args, "-config"), flag(&args, "-worker-control"))
    else {
        eprintln!("usage: worker_probe -worker -config <dir> -worker-control <control>");
        std::process::exit(2);
    };
    let dir = PathBuf::from(config);
    std::fs::write(dir.join("pid"), std::process::id().to_string()).expect("pid");
    let mode = std::fs::read_to_string(dir.join("mode")).unwrap_or_default();
    if mode.trim() == "early-exit" {
        std::process::exit(42);
    }
    #[cfg(windows)]
    if mode.trim() == "delayed-stall" {
        // Connects 5 s in: see `connect_and_hello_share_one_load_deadline`.
        std::thread::sleep(Duration::from_secs(5));
        let _pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&control)
            .expect("pipe");
        // The proof that the connect happened, for the test to require.
        std::fs::write(dir.join("connected"), "yes").expect("marker");
        std::thread::sleep(Duration::from_secs(120));
        std::process::exit(5);
    }
    let pending = match worker::take(&control) {
        Ok(pending) => pending,
        Err(error) => {
            write_out(&dir, &format!("take failed: {error}"));
            std::process::exit(4);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let code = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(120), run(pending, &dir))
            .await
            .unwrap_or(5)
    });
    std::process::exit(code);
}

fn write_out(dir: &Path, text: &str) {
    let _ = std::fs::write(dir.join("out"), text);
}

async fn run(pending: worker::Pending, dir: &Path) -> i32 {
    let mode = std::fs::read_to_string(dir.join("mode")).unwrap_or_default();
    if mode.trim() == "stall" {
        std::future::pending::<()>().await;
    }
    let ignore = mode.trim() == "ignore";
    let hello = Hello {
        max_version: 1,
        min_supervisor: if matches!(mode.trim(), "min2" | "min2-live") {
            2
        } else {
            1
        },
        version: "probe-1".to_owned(),
        caps: vec!["probe".to_owned()],
    };
    let mut attached = match pending.attach(hello).await {
        Ok(attached) => attached,
        Err(error) => {
            write_out(dir, &format!("attach failed: {error}"));
            if mode.trim() == "min2-live" {
                std::future::pending::<()>().await;
            }
            return 3;
        }
    };
    let names: Vec<&str> = attached.listeners.iter().map(|(n, _)| n.as_str()).collect();
    write_out(
        dir,
        &format!(
            "role={:?} version={} listeners={} switch={:?} load_ms={} topology={} nonblocking=ok",
            attached.welcome.role,
            attached.version,
            names.join(","),
            attached.welcome.switch_id,
            attached.welcome.budgets.load_ms,
            attached.welcome.topology.len(),
        ),
    );
    for (name, listener) in std::mem::take(&mut attached.listeners) {
        let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
            return 6;
        };
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(format!("probe-{name}").as_bytes()).await;
            }
        });
    }
    if attached
        .channel
        .send(&Message::Loaded { ms: 1 })
        .await
        .is_err()
    {
        return 7;
    }
    if mode.trim() == "no-read" {
        std::future::pending::<()>().await;
    }
    loop {
        match attached.channel.receive::<Message>().await {
            Ok(Some(frame)) => match frame.message {
                Message::Stop { .. } if !ignore => return 0,
                Message::Stop { .. } => {
                    let _ = std::fs::write(dir.join("ignored-stop"), "yes");
                }
                Message::Unknown => {
                    let _ = attached
                        .channel
                        .send(&Message::Unsupported {
                            message_type: frame.message_type,
                        })
                        .await;
                }
                _ => {}
            },
            Ok(None) if !ignore => return 0,
            Ok(None) => {
                let _ = std::fs::write(dir.join("ignored-eof"), "yes");
                std::future::pending::<()>().await;
            }
            Err(error) => {
                write_out(dir, &format!("receive failed: {error}"));
                return 8;
            }
        }
    }
}
