use anyhow::Result;
use kit::framework::Registry;
use kit::tools::{cdp, deploy, domain, record, render, scout, stats, update};

#[tokio::main]
async fn main() -> Result<()> {
    // Behave like a normal Unix filter under a closed pipe (`… | head`): exit on SIGPIPE instead of
    // panicking on EPIPE. Rust ignores SIGPIPE by default.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    Registry::new()
        .register(cdp::tool())
        .register(deploy::tool())
        .register(scout::tool())
        .register(domain::tool())
        .register(record::tool())
        .register(render::tool())
        .register(stats::tool())
        .register(update::tool())
        .dispatch()
        .await
}
