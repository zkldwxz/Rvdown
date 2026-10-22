//! Public-data integration example: JSONL jobs, concurrent SDK calls, observable progress.
use std::io::Write;

use futures::{stream, StreamExt};
use rvdown::{Client, Job, ProgressSink, TransferEvent};

struct Events;
impl ProgressSink for Events {
    fn on_event(&self, event: &TransferEvent) {
        let value = serde_json::json!({
            "event":"progress", "id":event.task_id, "asset":event.asset_id,
            "attempt":event.attempt_id, "phase":event.phase.code(),
            "validated_bytes":event.validated_bytes, "network_bytes":event.network_bytes,
            "resumed_bytes":event.resumed_bytes, "total_bytes":event.total_bytes,
            "percent":event.percent()
        });
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = std::env::args()
        .nth(1)
        .ok_or("expected a JSONL task file")?;
    let jobs: Vec<Job> = std::fs::read_to_string(file)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let client = Client::builder()
        .connections(8)
        .job_concurrency(2)
        .no_proxy()
        .build()?;
    let sink = Events;
    let mut running = stream::iter(
        jobs.iter()
            .map(|job| async { client.run_with_progress(job, Some(&sink)).await }),
    )
    .buffer_unordered(2);
    let mut failed = false;
    while let Some(outcome) = running.next().await {
        failed |= !outcome.ok;
        println!(
            "{}",
            serde_json::json!({"event":"result", "outcome":outcome})
        );
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
