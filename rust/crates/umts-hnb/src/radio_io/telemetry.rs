// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{RxCounters, TxHealthSnapshot, TxRecoveryCounters, WallTimeSummary};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};
use umts_runtime::mailbox::{self, Receiver, Sender};

const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug)]
pub struct Observation<T> {
    pub observed: Instant,
    pub skipped: u64,
    pub value: T,
}

pub(crate) struct Publisher<T> {
    sender: Sender<Observation<T>>,
    next: Instant,
    skipped: u64,
}

pub(crate) struct Inbox<T> {
    receiver: Receiver<Observation<T>>,
    latest: Option<Observation<T>>,
}

pub(crate) fn channel<T: Copy>() -> (Publisher<T>, Inbox<T>) {
    let (sender, receiver) = mailbox::bounded(NonZeroUsize::MIN);
    (
        Publisher {
            sender,
            next: Instant::now() + INTERVAL,
            skipped: 0,
        },
        Inbox {
            receiver,
            latest: None,
        },
    )
}

impl<T> Publisher<T> {
    pub fn publish(&mut self, now: Instant, snapshot: impl FnOnce() -> T) {
        if now < self.next {
            return;
        }
        self.next = now + INTERVAL;
        if self
            .sender
            .try_send(Observation {
                observed: now,
                skipped: self.skipped,
                value: snapshot(),
            })
            .is_err()
        {
            self.skipped = self.skipped.saturating_add(1);
        }
    }
}

impl<T: Copy> Inbox<T> {
    pub fn latest(&mut self) -> Option<Observation<T>> {
        if let Some(observation) = self.receiver.try_recv() {
            self.latest = Some(observation);
        }
        self.latest
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Preparation {
    pub prepared: u64,
    pub synthesis: Option<WallTimeSummary>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Submission {
    pub submitted: u64,
    pub partial: u64,
    pub health: Option<TxHealthSnapshot>,
    pub recovery: TxRecoveryCounters,
    pub write: Option<WallTimeSummary>,
    pub ready_queue: Option<WallTimeSummary>,
}

#[derive(Clone, Copy, Debug)]
pub struct Receive {
    pub radio: RxCounters,
    pub access: crate::common_rx::AccessCounters,
    pub published: u64,
    pub cycle: Option<WallTimeSummary>,
    pub read_filter: Option<WallTimeSummary>,
    pub dedicated_feed: Option<WallTimeSummary>,
    pub access_search: Option<WallTimeSummary>,
    pub dedicated_service: Option<WallTimeSummary>,
    pub access_decode: Option<WallTimeSummary>,
    pub clock_sample: Option<WallTimeSummary>,
    pub dedicated_cpu_ns: Option<u128>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Radio {
    pub preparation: Option<Observation<Preparation>>,
    pub submission: Option<Observation<Submission>>,
    pub receive: Option<Observation<Receive>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_snapshots_preserve_provenance_and_never_run_on_every_cycle() {
        let (mut publisher, mut inbox) = channel();
        let first = publisher.next;
        publisher.publish(first - Duration::from_nanos(1), || panic!("not due"));
        assert!(inbox.latest().is_none());
        publisher.publish(first, || 1);
        publisher.publish(first + INTERVAL, || 2);
        let observation = inbox.latest().unwrap();
        assert_eq!(observation.value, 1);
        assert_eq!(observation.observed, first);
        publisher.publish(first + INTERVAL * 2, || 3);
        let observation = inbox.latest().unwrap();
        assert_eq!(observation.value, 3);
        assert_eq!(observation.skipped, 1);
        assert_eq!(inbox.latest().unwrap().observed, observation.observed);
    }
}
