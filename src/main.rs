use std::error::Error;
use reqwest_wrap_log::{example_step, TrackedClient};

fn main() -> Result<(), Box<dyn Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let tracked_client = TrackedClient::new();

        // Цикл агрегатора (пример: несколько шагов)
        for step in 1..=2 {
            example_step(&tracked_client, &format!("step_{}", step)).await?;
        }

        Ok(())
    })
}