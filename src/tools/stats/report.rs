use super::model::StatsSnapshot;

pub fn print(snapshot: &StatsSnapshot) {
    let system = &snapshot.system;
    println!(
        "CPU {:>5.1}%  load {:.2} {:.2} {:.2}  RAM {} / {}  {} processes",
        system.global_cpu_percent,
        system.load_average[0],
        system.load_average[1],
        system.load_average[2],
        bytes(system.used_memory_bytes),
        bytes(system.total_memory_bytes),
        system.process_count,
    );
    println!("{:>7} {:>8} {:>9} {:>6}  COMMAND", "PID", "CPU", "RAM", "CORE");
    let mut processes = snapshot.processes.iter().collect::<Vec<_>>();
    processes.sort_by(|left, right| right.cpu_percent.total_cmp(&left.cpu_percent));
    for process in processes.into_iter().take(30) {
        let core = process.last_cpu.map(|core| core.to_string()).unwrap_or_else(|| "-".into());
        println!(
            "{:>7} {:>7.1}% {:>9} {:>6}  {}",
            process.key.pid,
            process.cpu_percent,
            bytes(process.rss_bytes),
            core,
            truncate(&process.command, 100),
        );
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub fn bytes(value: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut value = value as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

pub fn duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}
