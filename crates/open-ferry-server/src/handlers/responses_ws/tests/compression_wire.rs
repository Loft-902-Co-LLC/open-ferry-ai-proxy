// Test-only raw wire probes shared by both WebSocket directions.
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(super) fn configured(setting: Option<bool>) -> open_ferry_core::config::Config {
    let mut raw = String::from("proxy-url: direct\n");
    if let Some(enabled) = setting {
        raw.push_str(&format!(
            "upstream: {{codex: {{websocket-compression: {enabled}}}}}\n"
        ));
    }
    let cfg = open_ferry_core::config::Config::parse(raw.as_bytes()).unwrap();
    assert_eq!(cfg.codex.websocket_compression, setting);
    assert_eq!(
        cfg.codex.websocket_compression_enabled(),
        setting.unwrap_or(true)
    );
    cfg
}

pub(super) async fn head(tcp: &mut TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 64 * 1024, "unexpected fixture head size");
            let mut byte = [0];
            tcp.read_exact(&mut byte).await.unwrap();
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .expect("handshake timeout")
}

pub(super) fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then(|| value.trim()))
}

pub(super) async fn frame(tcp: &mut TcpStream, masked: bool) -> (u8, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut head = [0; 2];
        tcp.read_exact(&mut head).await.unwrap();
        assert_eq!(
            head[0] & 0x70,
            0,
            "RSV1/extension bits must stay clear on the wire"
        );
        assert_ne!(head[0] & 0x80, 0, "fixture messages fit one frame");
        assert_eq!(head[1] & 0x80 != 0, masked);
        let size = match head[1] & 0x7f {
            126 => {
                let mut n = [0; 2];
                tcp.read_exact(&mut n).await.unwrap();
                u64::from(u16::from_be_bytes(n))
            }
            127 => {
                let mut n = [0; 8];
                tcp.read_exact(&mut n).await.unwrap();
                u64::from_be_bytes(n)
            }
            n => u64::from(n),
        };
        assert!(size <= 1 << 20, "unexpected fixture frame size");
        let mut key = [0; 4];
        if masked {
            tcp.read_exact(&mut key).await.unwrap();
        }
        let mut payload = vec![0; usize::try_from(size).unwrap()];
        tcp.read_exact(&mut payload).await.unwrap();
        if masked {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[i % 4];
            }
        }
        (head[0] & 0xf, payload)
    })
    .await
    .expect("frame timeout")
}

async fn send(tcp: &mut TcpStream, opcode: u8, payload: &[u8], masked: bool) {
    let mut bytes = vec![0x80 | opcode];
    let mask = if masked { 0x80 } else { 0 };
    match payload.len() {
        n @ 0..=125 => bytes.push(mask | u8::try_from(n).unwrap()),
        n @ 126..=65535 => {
            bytes.push(mask | 126);
            bytes.extend_from_slice(&u16::try_from(n).unwrap().to_be_bytes());
        }
        n => {
            bytes.push(mask | 127);
            bytes.extend_from_slice(&u64::try_from(n).unwrap().to_be_bytes());
        }
    }
    let key = [0x11, 0x23, 0x35, 0x47];
    if masked {
        bytes.extend_from_slice(&key);
    }
    bytes.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, byte)| if masked { byte ^ key[i % 4] } else { *byte }),
    );
    assert_eq!(bytes[0] & 0x40, 0);
    tcp.write_all(&bytes).await.unwrap();
}

pub(super) async fn text(tcp: &mut TcpStream, payload: &str, masked: bool) {
    send(tcp, 1, payload.as_bytes(), masked).await;
}
pub(super) async fn close(tcp: &mut TcpStream, masked: bool) {
    send(tcp, 8, &1000u16.to_be_bytes(), masked).await;
}
