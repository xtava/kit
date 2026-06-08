//! Live proof of the Electron main-process capture path against a real `node --inspect`.
//!
//!   node --inspect=0 -e 'setInterval(()=>console.log("tick"),300)' &
//!   cargo run --example main_inspector -- <node-pid>
//!
//! Exercises exactly what the daemon's `main_process_pump` does: discover the node endpoint by pid,
//! connect, enable Runtime, and decode an emitted `console.log` into a Timeline Track.

use kit::cdp::{self, CdpConnection, Track};
use serde_json::json;

#[tokio::main]
async fn main() {
    let pid: u32 = std::env::args().nth(1).and_then(|a| a.parse().ok()).expect("usage: main_inspector <pid>");

    let endpoint = cdp::node_endpoint(pid).await.expect("no node inspector on that pid — launch it with --inspect");
    println!("discovered node inspector on :{} ({})", endpoint.port, endpoint.ws_url);

    let (conn, mut events) = CdpConnection::connect(&endpoint.ws_url).await.expect("connect");
    conn.call(None, "Runtime.enable", json!({})).await.expect("enable Runtime");
    println!("connected + enabled — waiting for a console event…");

    while let Some(event) = events.recv().await {
        if let Some(Track::Console(line)) = Track::from_event(&event) {
            println!("captured main console.{}: {}", line.level, line.text);
            return;
        }
    }
    panic!("socket closed before any console event");
}
