use std::error::Error;
use reqwest_wrap_log::{example_step, TrackedClient};

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let tracked_client = TrackedClient::new()?;

        // Цикл агрегатора (пример: несколько шагов)
        for step in 1..=1 {
            example_step(&tracked_client, &format!("step_{}", step)).await?;
        }

        Ok(())
    })
}