// SPDX-License-Identifier: Apache-2.0

use cli::client::repo_events::{RepoEventClient, SubscribeRepoEventsRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let server = arguments
        .next()
        .ok_or("usage: repo_events <server> <repo-id> [after-event-id]")?;
    let repo_id = arguments
        .next()
        .ok_or("usage: repo_events <server> <repo-id> [after-event-id]")?;
    let after_event_id = arguments.next().as_deref().unwrap_or("0").parse::<i64>()?;

    let client = RepoEventClient::connect(&server).await?;
    let mut events = client
        .subscribe(SubscribeRepoEventsRequest {
            repo_id,
            thread: String::new(),
            after_event_id,
            event_types: Vec::new(),
        })
        .await?;
    let event = events.next().await?;
    println!(
        "{}",
        serde_json::json!({
            "event_id": event.event_id,
            "repo_id": event.repo_id,
            "event_type": event.event_type,
            "thread": event.thread,
            "ref_name": event.ref_name,
            "is_thread": event.is_thread,
            "actor_subject": event.actor_subject,
            "payload_json": event.payload_json,
        })
    );
    client.close().await;
    Ok(())
}
