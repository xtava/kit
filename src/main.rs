use anyhow::Result;
use kit::framework::Registry;
use kit::tools::{domain, scout};

#[tokio::main]
async fn main() -> Result<()> {
    Registry::new()
        .register(scout::tool())
        .register(domain::tool())
        .dispatch()
        .await
}
