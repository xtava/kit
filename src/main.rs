use anyhow::Result;
use kit::framework::Registry;
use kit::tools::{
    build, deploy, diff, domain, monitor, ops, process, record, render, secrets, settings, stats,
    swarm, tail, update,
};

#[cfg(unix)]
use kit::tools::{cdp, scout};

#[tokio::main]
async fn main() -> Result<()> {
    // Behave like a normal Unix filter under a closed pipe (`… | head`): exit on SIGPIPE instead of
    // panicking on EPIPE. Rust ignores SIGPIPE by default.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if let Some(code) = kit::framework::process::run_detached_io_host_entry().await {
        std::process::exit(code);
    }

    if update::startup().await? {
        return Ok(());
    }

    let registry = Registry::new()
        .register(build::tool())
        .register(deploy::tool())
        .register(diff::tool())
        .register(domain::tool())
        .register(monitor::tool())
        .register(ops::tool())
        .register(process::tool())
        .register(record::tool())
        .register(render::tool())
        .register(secrets::tool())
        .register(stats::tool())
        .register(swarm::tool())
        .register(tail::tool())
        .register(update::tool());
    #[cfg(unix)]
    let registry = registry.register(scout::tool()).register(cdp::tool());
    registry.register_settings(settings::tool).dispatch().await
}
