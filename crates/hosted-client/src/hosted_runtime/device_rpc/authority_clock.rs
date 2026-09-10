//! One CPU-only clock per device server. Arbitrary Biscuit caveats cannot be
//! safely reduced to authority `expires_at`; idle streams recheck them here.
use std::{sync::Mutex, time::Duration};

use anyhow::{Context, Result};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Debug, Default)]
pub(super) struct AuthorityClock {
    running: Mutex<Option<Running>>,
}
#[derive(Debug)]
struct Running {
    changes: watch::Sender<()>,
    task: JoinHandle<()>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl AuthorityClock {
    /// Lazy startup permits constructing DeviceRpc outside a Tokio runtime.
    pub fn subscribe(&self) -> Result<Subscription> {
        let mut running = self
            .running
            .lock()
            .map_err(|_| anyhow::anyhow!("authority clock guard poisoned"))?;
        if running.is_none() {
            let (changes, _) = watch::channel(());
            let sender = changes.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    sender.send_replace(());
                }
            });
            *running = Some(Running { changes, task });
        }
        let running = running
            .as_ref()
            .context("authority clock not initialized")?;
        Ok(Subscription {
            changes: running.changes.subscribe(),
        })
    }
}

pub(super) struct Subscription {
    changes: watch::Receiver<()>,
}
impl Subscription {
    /// Valid ticks remain inside this future: no projection read or filesystem
    /// query is prompted. Canceling a stream drops its receiver immediately.
    pub async fn expired(&mut self, check_clock: impl Fn() -> Result<()>) -> Result<()> {
        loop {
            self.changes
                .changed()
                .await
                .context("authority clock closed")?;
            check_clock()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_idle_clock_enforces_signed_check_only_delegated_expiry() {
        let key = biscuit_auth::KeyPair::new();
        let expiry = chrono::Utc::now() + chrono::Duration::seconds(2);
        let root = biscuit_auth::Biscuit::builder()
            .fact(
                format!(
                    "expires_at({})",
                    (expiry + chrono::Duration::days(1)).to_rfc3339()
                )
                .as_str(),
            )
            .expect("authority expiration")
            .build(&key)
            .expect("signed root");
        let token = root
            .append(
                biscuit_verifier::key_delegation::device_restrictions(expiry)
                    .expect("check-only expiry"),
            )
            .expect("signed attenuation");
        let clock = AuthorityClock::default();
        let mut first = clock.subscribe().expect("first idle observer");
        let second = clock.subscribe().expect("second idle observer");
        assert_eq!(
            clock
                .running
                .lock()
                .expect("clock")
                .as_ref()
                .expect("running")
                .changes
                .receiver_count(),
            2
        );
        let check = || -> Result<()> {
            let mut authorizer = biscuit_auth::builder::AuthorizerBuilder::new()
                .fact(format!("time({})", chrono::Utc::now().to_rfc3339()).as_str())?
                .code("allow if true;")?
                .build(&token)?;
            authorizer.authorize()?;
            Ok(())
        };
        check().expect("valid before delegated expiry");
        let ended = tokio::time::timeout(Duration::from_secs(5), first.expired(check))
            .await
            .expect("idle observation ends without data changes");
        assert!(
            ended.is_err(),
            "earlier child caveat is enforced despite later root expiry"
        );
        drop(second);
        assert_eq!(
            clock
                .running
                .lock()
                .expect("clock")
                .as_ref()
                .expect("running")
                .changes
                .receiver_count(),
            1
        );
        drop(clock);
        tokio::time::timeout(Duration::from_secs(1), async {
            while first.changes.changed().await.is_ok() {}
        })
        .await
        .expect("device drop aborts clock and closes subscribers");
    }
}
