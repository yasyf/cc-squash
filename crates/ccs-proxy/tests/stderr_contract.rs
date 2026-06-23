//! The Go control plane scrapes the proxy's listen port from a fixed stderr line.
//! This locks that line's exact shape: `ccs-proxy listening on http://127.0.0.1:<port>`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn prints_listen_line_to_stderr() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ccs-proxy"))
        .arg("--port")
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ccs-proxy");

    let stderr = child.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();

    // tracing logs share stderr; read until the eprintln contract line appears.
    let listen = loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read stderr");
        assert!(n > 0, "process exited before printing the listen line");
        if line.starts_with("ccs-proxy listening on http://127.0.0.1:") {
            break line.trim_end().to_string();
        }
    };

    child.kill().ok();
    child.wait().ok();

    let port = listen
        .strip_prefix("ccs-proxy listening on http://127.0.0.1:")
        .expect("listen prefix");
    assert!(
        port.parse::<u16>().is_ok(),
        "expected a numeric port after the prefix, got {listen:?}",
    );
}
