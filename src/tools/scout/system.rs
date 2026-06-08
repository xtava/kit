//! System memory totals from `/proc/meminfo`.

use crate::tools::scout::model::SystemMemory;

pub fn system_memory() -> SystemMemory {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total_kib = 0;
    let mut available_kib = 0;
    let mut swap_total_kib = 0;
    let mut swap_free_kib = 0;
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let key = tokens.next().unwrap_or_default();
        let value = tokens.next().and_then(|kib| kib.parse::<u64>().ok()).unwrap_or(0);
        match key {
            "MemTotal:" => total_kib = value,
            "MemAvailable:" => available_kib = value,
            "SwapTotal:" => swap_total_kib = value,
            "SwapFree:" => swap_free_kib = value,
            _ => {}
        }
    }
    SystemMemory {
        total_kib,
        available_kib,
        swap_total_kib,
        swap_used_kib: swap_total_kib.saturating_sub(swap_free_kib),
    }
}
