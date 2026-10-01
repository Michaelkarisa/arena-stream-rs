//! Sets the OS-level receive buffer (`SO_RCVBUF`) on a UDP socket, sized
//! via [`crate::config::UDP_RECV_BUFFER_BYTES`]. Video/audio packets
//! arrive in bursts (a full video frame's worth of fragments back-to-back
//! — see `frame_assembler`), so the default kernel buffer can overflow
//! and silently drop packets under load; a larger buffer absorbs that.
//!
//! Deliberately dependency-free (raw FFI against the platform socket API
//! via `extern` blocks) rather than pulling in `socket2` or `libc` as a
//! new direct dependency — this project's `Cargo.toml` isn't something
//! this pass has visibility into, so adding a crate there blind was ruled
//! out. `setsockopt`'s C ABI is stable and small enough that hand-writing
//! the two platform bindings is the safer choice here.
//!
//! Best-effort: failures are logged, not propagated — a socket that
//! didn't get a bigger buffer still works, just with more risk of drops
//! under a burst, so ingest shouldn't refuse to start over it.

use tracing::{info, warn};

#[cfg(unix)]
fn set_recv_buffer_size(fd: std::os::unix::io::RawFd, size: usize) -> std::io::Result<()> {
    // Linux `SOL_SOCKET`/`SO_RCVBUF` values (`<sys/socket.h>` /
    // `<asm-generic/socket.h>`); this project targets a Linux/Windows
    // GStreamer service, not macOS/BSD, where these constants differ.
    const SOL_SOCKET: i32 = 1;
    const SO_RCVBUF: i32 = 8;

    extern "C" {
        fn setsockopt(
            socket: std::os::raw::c_int,
            level: std::os::raw::c_int,
            name: std::os::raw::c_int,
            value: *const std::os::raw::c_void,
            option_len: u32,
        ) -> std::os::raw::c_int;
    }

    let value: i32 = size as i32;
    let ret = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_RCVBUF,
            &value as *const i32 as *const std::os::raw::c_void,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn set_recv_buffer_size(sock: std::os::windows::io::RawSocket, size: usize) -> std::io::Result<()> {
    // Winsock `SOL_SOCKET`/`SO_RCVBUF` values (`<winsock2.h>`).
    const SOL_SOCKET: i32 = 0xffff;
    const SO_RCVBUF: i32 = 0x1002;

    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(
            s: usize,
            level: i32,
            optname: i32,
            optval: *const u8,
            optlen: i32,
        ) -> i32;
    }

    let value: i32 = size as i32;
    let ret = unsafe {
        setsockopt(
            sock as usize,
            SOL_SOCKET,
            SO_RCVBUF,
            &value as *const i32 as *const u8,
            std::mem::size_of::<i32>() as i32,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Applies [`crate::config::UDP_RECV_BUFFER_BYTES`] to `socket`. Call
/// once right after binding, before the receive loop starts.
pub fn grow_recv_buffer(socket: &tokio::net::UdpSocket, label: &str) {
    #[cfg(unix)]
    let result = {
        use std::os::unix::io::AsRawFd;
        set_recv_buffer_size(socket.as_raw_fd(), crate::config::UDP_RECV_BUFFER_BYTES)
    };
    #[cfg(windows)]
    let result = {
        use std::os::windows::io::AsRawSocket;
        set_recv_buffer_size(socket.as_raw_socket(), crate::config::UDP_RECV_BUFFER_BYTES)
    };
    #[cfg(not(any(unix, windows)))]
    let result: std::io::Result<()> = Ok(()); // unsupported target: no-op rather than a hard build failure

    match result {
        Ok(()) => info!(socket = label, bytes = crate::config::UDP_RECV_BUFFER_BYTES, "grew socket receive buffer"),
        Err(e) => warn!(socket = label, "failed to grow socket receive buffer: {e}"),
    }
}
