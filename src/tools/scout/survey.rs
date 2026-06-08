//! Build a [`Survey`] from both planes: the process plane (sync, from `/proc`) and the CDP target
//! plane (async, enriching instances that expose a debug port), plus system totals.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::tools::scout::model::Survey;
use crate::tools::scout::{cdp, proc, system};

pub async fn collect(marker: &str) -> Survey {
    let mut instances = proc::scan_fleet(marker);
    cdp::enrich(&mut instances).await;

    Survey {
        instances,
        system: system::system_memory(),
        taken_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0),
    }
}
