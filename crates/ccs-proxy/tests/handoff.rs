//! The fd-3 handoff boundary, driven against the real `ccs-proxy` binary: the
//! Go control plane's daemonkit `ChannelHandoff` spawn dup2's the parent end of
//! a socketpair onto fd 3 before exec, and `seam_channel` adopts it there.
//! Nothing else in either suite crosses that boundary — `tests/seam.rs` and the
//! Go seam tests both construct the stream in-process and call
//! `run_seam`/`Serve` directly — so a regression in fd numbering, CLOEXEC
//! handling, or the adoption itself breaks every real proxy launch with the
//! rest of the suite green.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ccs_proxy::build_version::BUILD_VERSION;
use serde_json::Value;

/// The descriptor daemonkit's `ChannelHandoff` places the seam on.
const HANDOFF_FD: i32 = 3;

#[test]
fn spawned_proxy_registers_over_the_inherited_fd3_channel() {
    let (parent, child) = UnixStream::pair().expect("socketpair");
    let temp = tempfile::tempdir().expect("temp dir");

    let child_fd = child.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccs-proxy"));
    command
        .arg("--port")
        .arg("0")
        .arg("--refs-db")
        .arg(temp.path().join("refs-v1.db"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_fd, HANDOFF_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // dup2 clears CLOEXEC only when it actually copies; oldfd == newfd
            // returns early and leaves the flag Rust set on the socket.
            if libc::fcntl(HANDOFF_FD, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut proxy = command.spawn().expect("spawn ccs-proxy");
    // The parent must hold the only remaining copy of its end, so a proxy that
    // never adopts fd 3 shows up as EOF rather than a hang.
    drop(child);

    parent
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let mut reader = BufReader::new(parent.try_clone().expect("clone parent end"));
    let mut line = String::new();
    let read = reader.read_line(&mut line);
    let read = match read {
        Ok(n) => n,
        Err(e) => {
            kill(&mut proxy);
            panic!("no register frame arrived on fd {HANDOFF_FD}: {e}");
        }
    };
    if read == 0 {
        kill(&mut proxy);
        panic!("the spawned proxy closed fd {HANDOFF_FD} without registering");
    }

    let register: Value = serde_json::from_str(&line).expect("register frame is JSON");
    assert_eq!(register["type"], "register");
    assert_eq!(register["protocol"], 1);
    assert_eq!(register["version"], BUILD_VERSION);
    assert_eq!(
        register["pid"].as_u64().expect("pid is a number"),
        u64::from(proxy.id()),
        "the register frame must come from the spawned proxy itself",
    );
    assert_ne!(
        register["port"].as_u64().expect("port is a number"),
        0,
        "the register frame must carry the port the proxy actually bound",
    );

    // The adopted channel is bidirectional: a shutdown frame written to the
    // parent end steps the spawned proxy down.
    (&parent)
        .write_all(b"{\"type\":\"shutdown\",\"protocol\":1}\n")
        .expect("send shutdown");
    let status = wait_bounded(&mut proxy, Duration::from_secs(30));
    assert!(
        status.success(),
        "proxy exited {status} after a seam shutdown over fd {HANDOFF_FD}",
    );
}

fn wait_bounded(proxy: &mut Child, budget: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + budget;
    loop {
        match proxy.try_wait().expect("wait for ccs-proxy") {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                kill(proxy);
                panic!("the spawned proxy did not exit within {budget:?} of a seam shutdown");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn kill(proxy: &mut Child) {
    let _ = proxy.kill();
    let _ = proxy.wait();
}
