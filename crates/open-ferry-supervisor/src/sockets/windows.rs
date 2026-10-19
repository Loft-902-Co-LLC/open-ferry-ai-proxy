//! Windows listening sockets for the supervisor and its workers.
//!
//! The supervisor binds the listeners and keeps them for its whole life. A
//! worker gets each one as a `WSAPROTOCOL_INFOW` that the supervisor made with
//! `WSADuplicateSocketW` for that worker's process id, sent as hex over the
//! control channel. The worker turns it back into a socket with `WSASocketW`
//! and wraps that in a `std::net::TcpListener`. Both processes then hold one
//! underlying socket with one backlog.
//!
//! Every socket made here uses `WSA_FLAG_NO_HANDLE_INHERIT`. Sockets rebuilt
//! from protocol info also need [`set_no_inherit`], because Winsock can ignore
//! that creation flag for duplicates.

use std::io;
use std::mem::{MaybeUninit, size_of, zeroed};
use std::net::{Ipv6Addr, SocketAddr, TcpListener, ToSocketAddrs};
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};
use std::ptr::{copy_nonoverlapping, null};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::Networking::WinSock::{
    INVALID_SOCKET, SOCKADDR, SOCKET, WSADATA, WSADuplicateSocketW, WSAGetLastError,
    WSAPROTOCOL_INFOW, WSASocketW, WSAStartup, bind as winsock_bind, closesocket, getsockopt,
    listen, setsockopt,
};

/// The size of a `WSAPROTOCOL_INFOW`: the wide layout, 628 bytes. The ANSI
/// layout is 372. Both ends of the control channel require exactly this many
/// bytes.
pub const PROTOCOL_INFO_LEN: usize = size_of::<WSAPROTOCOL_INFOW>();

const AF_INET: i32 = 2;
const AF_INET6: i32 = 23;
const SOCK_STREAM: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IPV6: i32 = 41;
const IPV6_V6ONLY: i32 = 27;
const SOL_SOCKET: i32 = 0xffff;
const SO_ACCEPTCONN: i32 = 0x0002;
const SO_TYPE: i32 = 0x1008;
/// `!SO_REUSEADDR`: no other socket may bind the port, not even one that sets
/// `SO_REUSEADDR`.
const SO_EXCLUSIVEADDRUSE: i32 = !0x0004;
const WSA_FLAG_OVERLAPPED: u32 = 0x01;
const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
/// The value that tells `WSASocketW` to take the family, type and protocol
/// from the protocol info.
const FROM_PROTOCOL_INFO: i32 = -1;

/// How [`bind`] makes a listener. There is no `Default`: pick the one that
/// matches what a standalone run does for the same configured host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindOptions {
    pub backlog: i32,
    /// For an IPv6 address: also accept IPv4 (`IPV6_V6ONLY` off).
    pub dual_stack: bool,
}

impl BindOptions {
    /// What `std::net::TcpListener::bind` gives a named host: a backlog of
    /// 128, and `IPV6_V6ONLY` left on for an IPv6 address.
    pub const fn named_host() -> Self {
        Self {
            backlog: 128,
            dual_stack: false,
        }
    }

    /// What the standalone run gives an empty host (every interface): a
    /// backlog of 1024, and IPv4 mapped on an IPv6 socket.
    pub const fn every_interface() -> Self {
        Self {
            backlog: 1024,
            dual_stack: true,
        }
    }
}

#[repr(C, align(8))]
struct SockAddrBuf([u8; 28]);

fn wsa_error() -> io::Error {
    // SAFETY: reads the calling thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn ensure_winsock() -> io::Result<()> {
    static START: OnceLock<i32> = OnceLock::new();
    let code = *START.get_or_init(|| {
        // SAFETY: WSADATA is plain data and WSAStartup only writes into it.
        unsafe {
            let mut data: WSADATA = zeroed();
            WSAStartup(0x0202, &mut data)
        }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// A socket that is closed on drop until it is turned into a listener.
struct RawSock(SOCKET);

impl RawSock {
    fn into_listener(mut self) -> TcpListener {
        let socket = self.0;
        self.0 = INVALID_SOCKET;
        // SAFETY: `socket` is a valid, open, owned socket, and ownership moves
        // to the listener because `self` no longer holds it.
        unsafe { TcpListener::from_raw_socket(socket as RawSocket) }
    }
}

impl Drop for RawSock {
    fn drop(&mut self) {
        if self.0 != INVALID_SOCKET {
            // SAFETY: closes the socket this value owns, once.
            unsafe {
                closesocket(self.0);
            }
        }
    }
}

fn new_socket(family: i32) -> io::Result<RawSock> {
    ensure_winsock()?;
    // SAFETY: no protocol info is passed, and the flags are valid.
    let socket = unsafe {
        WSASocketW(
            family,
            SOCK_STREAM,
            IPPROTO_TCP,
            null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        Err(wsa_error())
    } else {
        Ok(RawSock(socket))
    }
}

fn set_option(socket: SOCKET, level: i32, name: i32, value: i32) -> io::Result<()> {
    // SAFETY: the pointer and length describe `value`, which outlives the call.
    let result = unsafe {
        setsockopt(
            socket,
            level,
            name,
            (&value as *const i32).cast::<u8>(),
            size_of::<i32>() as i32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(wsa_error())
    }
}

fn get_option(socket: SOCKET, level: i32, name: i32) -> io::Result<i32> {
    let mut value: i32 = 0;
    let mut length = size_of::<i32>() as i32;
    // SAFETY: the pointer and length describe `value`, which outlives the call.
    let result = unsafe {
        getsockopt(
            socket,
            level,
            name,
            (&mut value as *mut i32).cast::<u8>(),
            &mut length,
        )
    };
    if result == 0 {
        Ok(value)
    } else {
        Err(wsa_error())
    }
}

/// The raw `sockaddr_in` or `sockaddr_in6` bytes for an address, and their length.
fn sockaddr_bytes(addr: &SocketAddr) -> (SockAddrBuf, i32) {
    let mut buffer = SockAddrBuf([0; 28]);
    let bytes = &mut buffer.0;
    match addr {
        SocketAddr::V4(v4) => {
            bytes[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&v4.port().to_be_bytes());
            bytes[4..8].copy_from_slice(&v4.ip().octets());
            (buffer, 16)
        }
        SocketAddr::V6(v6) => {
            bytes[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
            bytes[2..4].copy_from_slice(&v6.port().to_be_bytes());
            bytes[4..8].copy_from_slice(&v6.flowinfo().to_be_bytes());
            bytes[8..24].copy_from_slice(&v6.ip().octets());
            bytes[24..28].copy_from_slice(&v6.scope_id().to_ne_bytes());
            (buffer, 28)
        }
    }
}

/// Binds and listens on `addr`.
///
/// The socket is not inheritable, and no other socket can bind the same port
/// (`SO_EXCLUSIVEADDRUSE`). Port 0 picks a free port; ask the listener for it.
pub fn bind(addr: SocketAddr, options: BindOptions) -> io::Result<TcpListener> {
    let family = if addr.is_ipv6() { AF_INET6 } else { AF_INET };
    let socket = new_socket(family)?;
    set_option(socket.0, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)?;
    if addr.is_ipv6() {
        set_option(
            socket.0,
            IPPROTO_IPV6,
            IPV6_V6ONLY,
            i32::from(!options.dual_stack),
        )?;
    }
    let (buffer, length) = sockaddr_bytes(&addr);
    // SAFETY: `buffer` holds a valid, aligned socket address of `length` bytes.
    let result = unsafe {
        winsock_bind(
            socket.0,
            (&buffer as *const SockAddrBuf).cast::<SOCKADDR>(),
            length,
        )
    };
    if result != 0 {
        return Err(wsa_error());
    }
    // SAFETY: the socket is open and bound.
    if unsafe { listen(socket.0, options.backlog) } != 0 {
        return Err(wsa_error());
    }
    Ok(socket.into_listener())
}

/// Listens on every interface the way a standalone run does: an IPv6 socket
/// with IPv4 mapped where the system has IPv6, and an IPv4 socket otherwise.
pub fn bind_any(port: u16) -> io::Result<TcpListener> {
    let options = BindOptions::every_interface();
    bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), options)
        .or_else(|_| bind(SocketAddr::from(([0, 0, 0, 0], port)), options))
}

/// Listens on `host`:`port` the way a standalone run does: an empty host is
/// every interface; anything else is resolved like `TcpListener::bind` does
/// and the first address that binds wins. A bare IPv6 address is bracketed.
pub fn bind_host(host: &str, port: u16) -> io::Result<TcpListener> {
    let host = host.trim();
    if host.is_empty() {
        return bind_any(port);
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut last = None;
    for address in format!("{host}:{port}").to_socket_addrs()? {
        match bind(address, BindOptions::named_host()) {
            Ok(listener) => return Ok(listener),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the host did not resolve to any address",
        )
    }))
}

/// Makes the protocol info a process with id `process_id` needs to open its
/// own copy of `socket`, as hex of exactly [`PROTOCOL_INFO_LEN`] bytes.
///
/// The process must already exist. The info is for that process alone, and
/// stays usable while the socket given here stays open.
pub fn duplicate_for_process(socket: &impl AsRawSocket, process_id: u32) -> io::Result<String> {
    ensure_winsock()?;
    // SAFETY: the info is plain data that the call fills in.
    let mut info: WSAPROTOCOL_INFOW = unsafe { zeroed() };
    // SAFETY: `info` is a valid out pointer, and the socket is open.
    let result =
        unsafe { WSADuplicateSocketW(socket.as_raw_socket() as SOCKET, process_id, &mut info) };
    if result != 0 {
        return Err(wsa_error());
    }
    // SAFETY: `info` is initialised plain data of PROTOCOL_INFO_LEN bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&info as *const WSAPROTOCOL_INFOW).cast::<u8>(),
            PROTOCOL_INFO_LEN,
        )
    };
    Ok(encode_info(bytes))
}

/// Opens the listener a supervisor described with [`duplicate_for_process`].
///
/// Refuses anything but exact protocol info for a listening stream socket. The
/// socket is not inheritable. The caller sets it non-blocking before giving it
/// to tokio.
pub fn listener_from_info(info: &str) -> io::Result<TcpListener> {
    ensure_winsock()?;
    let bytes = decode_info(info)?;
    let mut info = MaybeUninit::<WSAPROTOCOL_INFOW>::uninit();
    // SAFETY: `bytes` holds exactly PROTOCOL_INFO_LEN bytes, `info` has room
    // for as many, and the two do not overlap.
    unsafe {
        copy_nonoverlapping(
            bytes.as_ptr(),
            info.as_mut_ptr().cast::<u8>(),
            PROTOCOL_INFO_LEN,
        )
    };
    // SAFETY: WSAPROTOCOL_INFOW is plain integers and arrays, so any bytes are
    // a valid value, and every byte was written above.
    let info = unsafe { info.assume_init() };
    // TCP over IPv4 or IPv6 only: refuse before Winsock opens anything else.
    let tcp = matches!(info.iAddressFamily, AF_INET | AF_INET6)
        && info.iSocketType == SOCK_STREAM
        && info.iProtocol == IPPROTO_TCP;
    if !tcp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the protocol info is not for a TCP socket over IPv4 or IPv6",
        ));
    }
    // SAFETY: `info` is a valid protocol info; Winsock rejects a bad one.
    let socket = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &info,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(wsa_error());
    }
    let socket = RawSock(socket).into_listener();
    set_no_inherit(&socket)?;
    if get_option(socket.as_raw_socket() as SOCKET, SOL_SOCKET, SO_TYPE)? != SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the socket in the protocol info is not a stream socket",
        ));
    }
    if get_option(socket.as_raw_socket() as SOCKET, SOL_SOCKET, SO_ACCEPTCONN)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the socket in the protocol info is not listening",
        ));
    }
    Ok(socket)
}

/// Prevents a child from inheriting this socket. Call immediately after a
/// duplicate is created, before publishing it or spawning a child. Winsock
/// can ignore `WSA_FLAG_NO_HANDLE_INHERIT` when rebuilding protocol info,
/// including in std and socket2's `try_clone` (rust-lang/rust#70719).
/// This narrows the exposure but doesn't close it: a child spawned on
/// another thread between the duplicate's creation and this call still
/// inherits it, and stable std can't limit what a child inherits.
/// On failure the caller must drop the duplicate rather than use it.
pub fn set_no_inherit(socket: &impl AsRawSocket) -> io::Result<()> {
    // SAFETY: the borrowed socket stays open for the call.
    let ok = unsafe {
        SetHandleInformation(
            socket.as_raw_socket() as usize as HANDLE,
            HANDLE_FLAG_INHERIT,
            0,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Whether a child process could inherit the socket's handle.
pub fn is_inheritable(socket: &impl AsRawSocket) -> io::Result<bool> {
    let mut flags: u32 = 0;
    // SAFETY: `flags` is a valid out pointer; a bad handle only makes the call fail.
    let ok = unsafe { GetHandleInformation(socket.as_raw_socket() as usize as HANDLE, &mut flags) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags & HANDLE_FLAG_INHERIT != 0)
    }
}

/// Lower-case hex.
pub fn encode_info(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Decodes the hex of one protocol info, strictly: exactly
/// [`PROTOCOL_INFO_LEN`] bytes, only hex digits, nothing before, between or
/// after.
pub fn decode_info(text: &str) -> io::Result<Vec<u8>> {
    let invalid = |reason: &str| io::Error::new(io::ErrorKind::InvalidData, reason.to_owned());
    let digits = text.as_bytes();
    if digits.len() != PROTOCOL_INFO_LEN * 2 {
        return Err(invalid(&format!(
            "the socket protocol info must be {} bytes of hex, not {} characters",
            PROTOCOL_INFO_LEN,
            digits.len()
        )));
    }
    let mut bytes = Vec::with_capacity(PROTOCOL_INFO_LEN);
    let (pairs, rest) = digits.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(invalid("the socket protocol info is cut short"));
    }
    for [high, low] in pairs {
        match (hex_value(*high), hex_value(*low)) {
            (Some(high), Some(low)) => bytes.push(high << 4 | low),
            _ => {
                return Err(invalid(
                    "the socket protocol info has a character that is not hex",
                ));
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn loopback() -> TcpListener {
        bind("127.0.0.1:0".parse().unwrap(), BindOptions::named_host()).unwrap()
    }

    #[test]
    fn bind_options_match_a_standalone_run() {
        // Not upstream's: the supervised bind must not quietly differ from today's.
        assert_eq!(BindOptions::named_host().backlog, 128);
        assert!(!BindOptions::named_host().dual_stack);
        assert_eq!(BindOptions::every_interface().backlog, 1024);
        assert!(BindOptions::every_interface().dual_stack);
    }

    #[test]
    fn hosts_bind_like_a_standalone_run() {
        // Not upstream's: a padded host is trimmed. Loopback IPv4 only; the
        // empty-host wildcard bind is deliberately not exercised here.
        let padded = bind_host(" 127.0.0.1 ", 0).unwrap();
        assert!(padded.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn protocol_info_is_the_wide_layout() {
        // Not upstream's: 372 is the ANSI layout; the wide one is what we send.
        assert_eq!(PROTOCOL_INFO_LEN, 628);
    }

    #[test]
    fn hex_round_trips_and_refuses_anything_else() {
        // Not upstream's: the worker decodes strictly, so a bad info is load_failed.
        let bytes: Vec<u8> = (0..PROTOCOL_INFO_LEN).map(|n| (n % 251) as u8).collect();
        let text = encode_info(&bytes);
        assert_eq!(text.len(), PROTOCOL_INFO_LEN * 2);
        assert_eq!(decode_info(&text).unwrap(), bytes);
        assert_eq!(decode_info(&text.to_uppercase()).unwrap(), bytes);
        assert!(decode_info("").is_err());
        assert!(decode_info(&text[..text.len() - 2]).is_err());
        assert!(decode_info(&format!("{text}00")).is_err());
        assert!(decode_info(&format!("{text} ")).is_err());
        assert!(decode_info(&format!("{}g", &text[..text.len() - 1])).is_err());
        assert!(decode_info(&format!(" {}", &text[1..])).is_err());
    }

    #[test]
    fn a_bound_listener_is_not_inheritable() {
        // Not upstream's: a child must never inherit a listener by accident.
        let listener = loopback();
        assert!(!is_inheritable(&listener).unwrap());
    }

    #[test]
    fn nothing_else_can_bind_the_same_port() {
        // Not upstream's: SO_EXCLUSIVEADDRUSE keeps another process off our port.
        let listener = loopback();
        let taken = listener.local_addr().unwrap();
        assert!(bind(taken, BindOptions::named_host()).is_err());
    }

    #[test]
    fn a_duplicate_listens_on_the_same_socket() {
        // Not upstream's: a worker's copy shares the supervisor's backlog.
        let listener = loopback();
        let address = listener.local_addr().unwrap();
        let info = duplicate_for_process(&listener, std::process::id()).unwrap();
        let copy = listener_from_info(&info).unwrap();
        assert!(!is_inheritable(&copy).unwrap());
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(b"x").unwrap();
        let (mut accepted, _) = copy.accept().unwrap();
        let mut byte = [0u8; 1];
        accepted.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn info_for_anything_but_tcp_is_refused_before_a_socket_is_made() {
        // Not upstream's: a mutated family, type or protocol never reaches Winsock.
        let listener = loopback();
        let info = duplicate_for_process(&listener, std::process::id()).unwrap();
        let good = decode_info(&info).unwrap();
        let family = std::mem::offset_of!(WSAPROTOCOL_INFOW, iAddressFamily);
        let kind = std::mem::offset_of!(WSAPROTOCOL_INFOW, iSocketType);
        let protocol = std::mem::offset_of!(WSAPROTOCOL_INFOW, iProtocol);
        for (at, value) in [(family, 1i32), (kind, 2), (protocol, 132)] {
            let mut bytes = good.clone();
            bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
            let error = listener_from_info(&encode_info(&bytes)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(
                error.to_string().contains("not for a TCP socket"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_connected_socket_is_not_accepted_as_a_listener() {
        // Not upstream's: the worker refuses info for anything but a listener.
        let listener = loopback();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let info = duplicate_for_process(&client, std::process::id()).unwrap();
        assert!(listener_from_info(&info).is_err());
    }

    #[test]
    fn info_of_the_right_size_with_nothing_in_it_is_refused() {
        // Not upstream's: right-sized garbage must fail, not make a socket.
        let zeros = "0".repeat(PROTOCOL_INFO_LEN * 2);
        assert!(listener_from_info(&zeros).is_err());
    }
}
