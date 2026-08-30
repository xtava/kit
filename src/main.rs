use anyhow::{Context, Result};
use kit::framework::Registry;
use kit::tools::{
    build, deploy, diff, domain, monitor, ops, process, record, render, secrets, settings, stats,
    swarm, sync, update,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use kit::tools::stream;

#[cfg(target_os = "linux")]
use kit::tools::tsgo;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use kit::tools::{console, remote, tail};

#[cfg(unix)]
use kit::tools::{cdp, scout, skills};

fn main() -> Result<()> {
    // Behave like a normal Unix filter under a closed pipe (`… | head`): exit on SIGPIPE instead of
    // panicking on EPIPE. Rust ignores SIGPIPE by default.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(result) = console::run_hidden_entry_if_requested() {
        return result;
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the Kit application runtime")?
        .block_on(run())
}

async fn run() -> Result<()> {
    if let Some(code) = kit::framework::process::run_detached_io_host_entry().await {
        std::process::exit(code);
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
        .register(sync::tool())
        .register(update::tool());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let registry =
        registry.register(tail::tool()).register(console::tool()).register(remote::tool());
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let registry = registry.register(stream::tool());
    #[cfg(target_os = "linux")]
    let registry = registry.register(tsgo::tool());
    #[cfg(unix)]
    let registry = registry.register(scout::tool()).register(cdp::tool()).register(skills::tool());
    registry.register_settings(settings::tool).dispatch().await
}
