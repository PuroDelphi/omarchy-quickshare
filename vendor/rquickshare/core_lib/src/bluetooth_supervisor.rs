//! Keeps one live Bluetooth backend published for the lifetime of the engine.
//!
//! A `Bluetooth` instance is bound to one BlueZ session and adapter, and it fails
//! closed when the adapter is powered off, BlueZ restarts or the adapter disappears.
//! This module rebuilds it through the ordinary startup path (`Bluetooth::new`,
//! including recovery of the adapter-settings journal) once the adapter is usable
//! again. It never powers the adapter on: `Bluetooth::new` still refuses an unpowered
//! adapter, and the supervisor only retries.
//!
//! Rebuilds are bounded. Delays back off, an unchanged error is reported once, and
//! consecutive short-lived instances end supervision, which fails closed exactly as
//! before. The policy is generic over the instance type so it is testable without BlueZ.

use anyhow::Result;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;

/// The currently published backend, or `None` while it is down.
pub(crate) type Slot<T> = Arc<watch::Sender<Option<Arc<T>>>>;

/// How the first instance fared before supervision started.
pub(crate) enum Start<T> {
    /// The caller already built it.
    Running(T),
    /// The caller's attempt failed with this message, which the caller already reported.
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pace {
    /// First delay before a rebuild. It doubles after each consecutive failure.
    pub min: Duration,
    /// Upper bound for the delay.
    pub max: Duration,
    /// An instance that ran this long counts as healthy and starts a new failure streak.
    pub healthy: Duration,
    /// How long to wait for the old instance's last reference before rebuilding.
    pub release: Duration,
    /// Consecutive short-lived instances after which supervision stops.
    pub give_up: u32,
}

impl Pace {
    pub(crate) const DEFAULT: Pace = Pace {
        min: Duration::from_secs(2),
        max: Duration::from_secs(10),
        healthy: Duration::from_secs(30),
        release: Duration::from_secs(30),
        give_up: 5,
    };
}

struct Backoff {
    current: Duration,
    pace: Pace,
}

impl Backoff {
    fn new(pace: Pace) -> Self {
        Self { current: pace.min, pace }
    }

    fn reset(&mut self) {
        self.current = self.pace.min;
    }

    /// Sleeps for the current delay, growing it for next time. False when cancelled.
    async fn wait(&mut self, ctk: &CancellationToken) -> bool {
        let delay = self.current;
        self.current = (self.current * 2).min(self.pace.max);
        pause(ctk, delay).await
    }
}

/// Sleeps for `delay` unless cancelled. False when cancelled.
async fn pause(ctk: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = ctk.cancelled() => false,
        _ = sleep(delay) => true,
    }
}

/// Reports `message` unless it repeats the previous report.
fn note(report: &impl Fn(String), reported: &mut Option<String>, message: String) {
    if reported.as_deref() != Some(message.as_str()) {
        report(message.clone());
        *reported = Some(message);
    }
}

/// Waits (bounded) until every other holder of the old instance has dropped it.
/// The adapter journal's exclusive lock and the RFCOMM profile registration are
/// both single-owner, so a rebuild cannot succeed while a straggler is alive.
async fn wait_released<T>(released: &Weak<T>, ctk: &CancellationToken, pace: Pace) {
    let poll = pace.min.min(Duration::from_millis(50));
    let deadline = Instant::now() + pace.release;
    while released.strong_count() > 0 && Instant::now() < deadline {
        if !pause(ctk, poll).await {
            return;
        }
    }
}

/// Publishes a live instance in `slot` until `ctk` is cancelled or supervision gives up.
///
/// `make` builds an instance, `run` drives it until it ends, and `report` receives
/// user-visible errors with the same prefixes the engine used before supervision
/// ("Bluetooth initialization: ..." and "Bluetooth transport: ...").
/// `Bluetooth::new` is never aborted mid-call. Cancellation is honoured between steps.
pub(crate) async fn supervise<T, Make, MakeFut, Run, RunFut, Report>(
    slot: Slot<T>,
    start: Start<T>,
    mut make: Make,
    mut run: Run,
    report: Report,
    pace: Pace,
    ctk: CancellationToken,
) where
    Make: FnMut() -> MakeFut,
    MakeFut: Future<Output = Result<T>>,
    Run: FnMut(Arc<T>) -> RunFut,
    RunFut: Future<Output = Result<()>>,
    Report: Fn(String),
{
    let mut reported = None;
    let mut next = None;
    // Whether the next published instance is a recovery, for the log line.
    let mut rebuilt = false;
    match start {
        Start::Running(instance) => next = Some(instance),
        Start::Failed(message) => {
            reported = Some(message);
            rebuilt = true;
        }
    }
    let mut backoff = Backoff::new(pace);
    let mut quick = 0_u32;
    let supervision = async {
        // The caller's own attempt just failed, so wait before the first retry.
        if next.is_none() && !backoff.wait(&ctk).await {
            return;
        }
        while !ctk.is_cancelled() {
            let instance = match next.take() {
                Some(instance) => instance,
                None => match make().await {
                    Ok(instance) => instance,
                    Err(error) => {
                        note(&report, &mut reported, format!("Bluetooth initialization: {error:#}"));
                        if !backoff.wait(&ctk).await {
                            return;
                        }
                        continue;
                    }
                },
            };
            let instance = Arc::new(instance);
            let released = Arc::downgrade(&instance);
            if rebuilt {
                info!("Bluetooth backend re-initialized");
            }
            slot.send_replace(Some(instance.clone()));
            let started = Instant::now();
            let result = run(instance).await;
            slot.send_replace(None);
            if ctk.is_cancelled() {
                return;
            }
            rebuilt = true;
            if started.elapsed() >= pace.healthy {
                backoff.reset();
                quick = 0;
                reported = None;
            } else {
                quick += 1;
            }
            match result {
                Err(error) => note(&report, &mut reported, format!("Bluetooth transport: {error:#}")),
                Ok(()) => debug!("Bluetooth backend ended without an error; rebuilding"),
            }
            if quick >= pace.give_up {
                report(format!(
                    "Bluetooth backend stopped after {quick} consecutive short-lived runs; restart the service to try again"
                ));
                return;
            }
            wait_released(&released, &ctk, pace).await;
            if !backoff.wait(&ctk).await {
                return;
            }
        }
    };
    supervision.await;
    slot.send_replace(None);
}

/// Runs `discover` against each published instance in turn.
///
/// The run is cancelled and awaited when the slot is replaced or withdrawn, so the
/// old instance's references (and its radio) are released before the next one starts.
/// A `discover` that ends on its own is not restarted on the same instance: a radio
/// that failed closed stays failed closed.
pub(crate) async fn follow<T, Discover, DiscoverFut, Report>(
    mut slot: watch::Receiver<Option<Arc<T>>>,
    mut discover: Discover,
    report: Report,
    ctk: CancellationToken,
) where
    Discover: FnMut(Arc<T>, CancellationToken) -> DiscoverFut,
    DiscoverFut: Future<Output = Result<()>>,
    Report: Fn(String),
{
    loop {
        let instance = loop {
            if let Some(instance) = slot.borrow_and_update().clone() {
                break instance;
            }
            tokio::select! {
                _ = ctk.cancelled() => return,
                changed = slot.changed() => if changed.is_err() { return },
            }
        };
        let current = ctk.child_token();
        let mut discovery = std::pin::pin!(discover(instance, current.clone()));
        let mut ended = false;
        let mut closed = false;
        loop {
            tokio::select! {
                result = &mut discovery, if !ended => {
                    ended = true;
                    if let Err(error) = result {
                        report(format!("Bluetooth discovery: {error:#}"));
                    }
                }
                _ = ctk.cancelled() => break,
                changed = slot.changed() => {
                    closed = changed.is_err();
                    break;
                }
            }
        }
        if !ended {
            current.cancel();
            if let Err(error) = discovery.await {
                report(format!("Bluetooth discovery: {error:#}"));
            }
        }
        if closed || ctk.is_cancelled() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Notify;
    use tokio::time::timeout;

    const FAST: Pace = Pace {
        min: Duration::from_millis(5),
        max: Duration::from_millis(20),
        healthy: Duration::from_millis(150),
        release: Duration::from_secs(5),
        give_up: 3,
    };

    #[derive(Debug)]
    struct Probe(u32);

    type Seen = Arc<Mutex<Vec<String>>>;

    fn slot() -> (Slot<Probe>, watch::Receiver<Option<Arc<Probe>>>) {
        let (sender, receiver) = watch::channel(None);
        (Arc::new(sender), receiver)
    }

    fn collector() -> (Seen, impl Fn(String)) {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        (seen, move |message| sink.lock().unwrap().push(message))
    }

    async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            sleep(Duration::from_millis(2)).await;
        }
    }

    fn published(receiver: &watch::Receiver<Option<Arc<Probe>>>, id: u32) -> bool {
        receiver.borrow().as_ref().is_some_and(|probe| probe.0 == id)
    }

    #[tokio::test]
    async fn rebuilds_after_a_run_error_and_withdraws_the_old_instance_first() {
        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let (reports, report) = collector();
        let empty_when_built = Arc::new(Mutex::new(Vec::new()));
        let runs = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(supervise(
            sender,
            Start::Running(Probe(1)),
            {
                let receiver = receiver.clone();
                let empty_when_built = empty_when_built.clone();
                move || {
                    empty_when_built.lock().unwrap().push(receiver.borrow().is_none());
                    async { Ok(Probe(2)) }
                }
            },
            {
                let runs = runs.clone();
                let ctk = ctk.clone();
                move |_: Arc<Probe>| {
                    let first = runs.fetch_add(1, Ordering::SeqCst) == 0;
                    let ctk = ctk.clone();
                    async move {
                        if first {
                            return Err(anyhow!("Bluetooth powered off while backend was running"));
                        }
                        ctk.cancelled().await;
                        Ok(())
                    }
                }
            },
            report,
            FAST,
            ctk.clone(),
        ));
        eventually("the rebuilt instance", || published(&receiver, 2)).await;
        assert_eq!(*empty_when_built.lock().unwrap(), vec![true]);
        assert_eq!(
            *reports.lock().unwrap(),
            vec!["Bluetooth transport: Bluetooth powered off while backend was running"]
        );
        ctk.cancel();
        task.await.unwrap();
        assert!(receiver.borrow().is_none());
    }

    #[tokio::test]
    async fn an_unchanged_init_error_is_reported_once_and_a_seeded_one_never() {
        for (seed, expected) in [
            ("Bluetooth initialization: something else", 1),
            ("Bluetooth initialization: adapter powered off", 0),
        ] {
            let (sender, receiver) = slot();
            let ctk = CancellationToken::new();
            let (reports, report) = collector();
            let attempts = Arc::new(AtomicUsize::new(0));
            let task = tokio::spawn(supervise(
                sender,
                Start::Failed(seed.into()),
                {
                    let attempts = attempts.clone();
                    move || {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if attempt < 3 {
                                Err(anyhow!("adapter powered off"))
                            } else {
                                Ok(Probe(9))
                            }
                        }
                    }
                },
                {
                    let ctk = ctk.clone();
                    move |_: Arc<Probe>| {
                        let ctk = ctk.clone();
                        async move {
                            ctk.cancelled().await;
                            Ok(())
                        }
                    }
                },
                report,
                FAST,
                ctk.clone(),
            ));
            eventually("recovery after three failed attempts", || published(&receiver, 9)).await;
            assert_eq!(reports.lock().unwrap().len(), expected, "seed: {seed}");
            if expected == 1 {
                assert_eq!(
                    reports.lock().unwrap()[0],
                    "Bluetooth initialization: adapter powered off"
                );
            }
            ctk.cancel();
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rebuild_waits_until_the_old_instance_is_dropped() {
        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let fail = Arc::new(Notify::new());
        let makes = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(supervise(
            sender,
            Start::Running(Probe(1)),
            {
                let makes = makes.clone();
                move || {
                    makes.fetch_add(1, Ordering::SeqCst);
                    async { Ok(Probe(2)) }
                }
            },
            {
                let fail = fail.clone();
                let ctk = ctk.clone();
                move |probe: Arc<Probe>| {
                    let fail = fail.clone();
                    let ctk = ctk.clone();
                    async move {
                        if probe.0 == 1 {
                            fail.notified().await;
                            return Err(anyhow!("powered off"));
                        }
                        ctk.cancelled().await;
                        Ok(())
                    }
                }
            },
            |_| {},
            FAST,
            ctk.clone(),
        ));
        eventually("the first instance", || published(&receiver, 1)).await;
        // A straggler, such as an in-flight connect, still holds the old instance.
        let held = receiver.borrow().clone().unwrap();
        fail.notify_one();
        eventually("withdrawal", || receiver.borrow().is_none()).await;
        sleep(Duration::from_millis(150)).await;
        assert_eq!(makes.load(Ordering::SeqCst), 0, "rebuilt while the old instance was referenced");
        drop(held);
        eventually("the rebuild", || published(&receiver, 2)).await;
        ctk.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_ends_supervision_during_a_run_and_during_backoff() {
        let prompt = Duration::from_secs(1);

        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let task = tokio::spawn(supervise(
            sender,
            Start::Running(Probe(1)),
            || async { Ok(Probe(2)) },
            {
                let ctk = ctk.clone();
                move |_: Arc<Probe>| {
                    let ctk = ctk.clone();
                    async move {
                        ctk.cancelled().await;
                        Ok(())
                    }
                }
            },
            |_| {},
            FAST,
            ctk.clone(),
        ));
        eventually("the instance", || published(&receiver, 1)).await;
        ctk.cancel();
        timeout(prompt, task).await.expect("stopped promptly during a run").unwrap();
        assert!(receiver.borrow().is_none());

        let (sender, _receiver) = slot();
        let ctk = CancellationToken::new();
        let slow = Pace { min: Duration::from_secs(60), max: Duration::from_secs(60), ..FAST };
        let task = tokio::spawn(supervise(
            sender,
            Start::Failed("Bluetooth initialization: adapter powered off".into()),
            || async { Err::<Probe, _>(anyhow!("adapter powered off")) },
            |_: Arc<Probe>| async { Ok(()) },
            |_| {},
            slow,
            ctk.clone(),
        ));
        ctk.cancel();
        timeout(prompt, task).await.expect("stopped promptly during backoff").unwrap();
    }

    #[tokio::test]
    async fn only_a_healthy_run_starts_a_new_error_streak() {
        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let (reports, report) = collector();
        let built = Arc::new(AtomicUsize::new(1));
        let task = tokio::spawn(supervise(
            sender,
            Start::Running(Probe(1)),
            {
                let built = built.clone();
                move || {
                    let id = built.fetch_add(1, Ordering::SeqCst) as u32 + 1;
                    async move { Ok(Probe(id)) }
                }
            },
            {
                let ctk = ctk.clone();
                move |probe: Arc<Probe>| {
                    let ctk = ctk.clone();
                    async move {
                        match probe.0 {
                            // Healthy, then short-lived, then healthy again: the same
                            // error is news after a healthy run, noise within a streak.
                            1 | 3 => {
                                sleep(Duration::from_millis(250)).await;
                                Err(anyhow!("boom"))
                            }
                            2 => Err(anyhow!("boom")),
                            _ => {
                                ctk.cancelled().await;
                                Ok(())
                            }
                        }
                    }
                }
            },
            report,
            FAST,
            ctk.clone(),
        ));
        eventually("the fourth instance", || published(&receiver, 4)).await;
        assert_eq!(
            *reports.lock().unwrap(),
            vec!["Bluetooth transport: boom", "Bluetooth transport: boom"]
        );
        ctk.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn consecutive_short_lived_runs_end_supervision_and_say_so() {
        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let (reports, report) = collector();
        let runs = Arc::new(AtomicUsize::new(0));
        let makes = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(supervise(
            sender,
            Start::Running(Probe(1)),
            {
                let makes = makes.clone();
                move || {
                    let id = makes.fetch_add(1, Ordering::SeqCst) as u32 + 2;
                    async move { Ok(Probe(id)) }
                }
            },
            {
                let runs = runs.clone();
                move |_: Arc<Probe>| {
                    let run = runs.fetch_add(1, Ordering::SeqCst);
                    async move { Err(anyhow!("failure {run}")) }
                }
            },
            report,
            FAST,
            ctk.clone(),
        ));
        timeout(Duration::from_secs(5), task)
            .await
            .expect("supervision ended by itself")
            .unwrap();
        assert!(receiver.borrow().is_none());
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(makes.load(Ordering::SeqCst), 2);
        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 4);
        assert_eq!(
            reports[3],
            "Bluetooth backend stopped after 3 consecutive short-lived runs; restart the service to try again"
        );
    }

    #[tokio::test]
    async fn follower_runs_discovery_per_instance_and_never_restarts_one_that_ended() {
        let (sender, receiver) = slot();
        let ctk = CancellationToken::new();
        let (reports, report) = collector();
        let started = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(follow(
            receiver,
            {
                let started = started.clone();
                let cancelled = cancelled.clone();
                move |probe: Arc<Probe>, token: CancellationToken| {
                    started.lock().unwrap().push(probe.0);
                    let cancelled = cancelled.clone();
                    async move {
                        if probe.0 == 3 {
                            return Err(anyhow!("scanner refused"));
                        }
                        token.cancelled().await;
                        cancelled.lock().unwrap().push(probe.0);
                        Ok(())
                    }
                }
            },
            report,
            ctk.clone(),
        ));
        sender.send_replace(Some(Arc::new(Probe(1))));
        eventually("discovery on 1", || *started.lock().unwrap() == [1]).await;
        // Replacing the instance cancels and awaits the old discovery first.
        sender.send_replace(Some(Arc::new(Probe(2))));
        eventually("discovery on 2", || *started.lock().unwrap() == [1, 2]).await;
        assert_eq!(*cancelled.lock().unwrap(), vec![1]);
        // Withdrawing it cancels discovery and starts nothing.
        sender.send_replace(None);
        eventually("discovery on 2 cancelled", || *cancelled.lock().unwrap() == [1, 2]).await;
        // A discovery that ends on its own is reported but not restarted.
        sender.send_replace(Some(Arc::new(Probe(3))));
        eventually("the discovery error", || reports.lock().unwrap().len() == 1).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(*started.lock().unwrap(), vec![1, 2, 3]);
        assert_eq!(*reports.lock().unwrap(), vec!["Bluetooth discovery: scanner refused"]);
        ctk.cancel();
        timeout(Duration::from_secs(1), task).await.expect("follower stopped").unwrap();
    }
}
