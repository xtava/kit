//! Turning a process's `cmdline` into a [`Role`] — the load-bearing knowledge, grounded in the
//! live fleet's real flags (`--type=fileWatcher`, `--type=broker`, `node.mojom.NodeService`, …).

use crate::tools::scout::model::{Role, UtilityKind};

/// A `--type=zygote` process above this RSS is a forked renderer, not an idle template.
const ZYGOTE_RENDERER_RSS_FLOOR_KIB: u64 = 30 * 1024;

pub fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    args.iter().find_map(|arg| arg.strip_prefix(prefix.as_str()))
}

pub fn process_type(args: &[String]) -> Option<&str> {
    arg_value(args, "--type")
}

pub fn wm_class(args: &[String]) -> Option<&str> {
    arg_value(args, "--class")
}

pub fn debug_port(args: &[String]) -> Option<u16> {
    arg_value(args, "--remote-debugging-port").and_then(|value| value.parse().ok())
}

pub fn role(args: &[String], rss_kib: u64) -> Role {
    match process_type(args) {
        None => Role::Browser,
        Some("renderer") => Role::Renderer,
        Some("gpu-process") => Role::Gpu,
        Some("broker") => Role::Broker,
        Some("fileWatcher") => Role::FileWatcher,
        Some("utility") => Role::Utility(utility_kind(args)),
        Some("zygote") if rss_kib >= ZYGOTE_RENDERER_RSS_FLOOR_KIB => Role::Renderer,
        Some("zygote") => Role::Zygote,
        Some(_) => Role::Unknown,
    }
}

fn utility_kind(args: &[String]) -> UtilityKind {
    let sub = arg_value(args, "--utility-sub-type").unwrap_or_default();
    match sub.split('.').next().unwrap_or(sub) {
        "network" => UtilityKind::Network,
        "storage" => UtilityKind::Storage,
        "audio" => UtilityKind::Audio,
        "node" => UtilityKind::Node,
        "" => UtilityKind::Other("unknown".to_owned()),
        other => UtilityKind::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn classifies_chromium_process_types() {
        assert_eq!(role(&args(&["/app/electron"]), 200_000), Role::Browser);
        assert_eq!(role(&args(&["x", "--type=renderer"]), 200_000), Role::Renderer);
        assert_eq!(role(&args(&["x", "--type=gpu-process"]), 50_000), Role::Gpu);
        assert_eq!(role(&args(&["x", "--type=fileWatcher"]), 5_000), Role::FileWatcher);
        assert_eq!(role(&args(&["x", "--type=broker"]), 1_000), Role::Broker);
        assert_eq!(
            role(&args(&["x", "--type=utility", "--utility-sub-type=node.mojom.NodeService"]), 50_000),
            Role::Utility(UtilityKind::Node)
        );
    }

    #[test]
    fn big_zygote_is_a_forked_renderer() {
        assert_eq!(role(&args(&["x", "--type=zygote"]), 200_000), Role::Renderer);
        assert_eq!(role(&args(&["x", "--type=zygote"]), 4_000), Role::Zygote);
    }

}
