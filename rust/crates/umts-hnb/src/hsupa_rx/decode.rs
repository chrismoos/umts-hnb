// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{HarqReceiver, ReceiveError, Received};
use crate::dedicated_rx::ReceiveIdentity;
use std::{
    collections::VecDeque,
    io,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};
use umts_dsp::{coding::DecodeWork, hsupa::ControlInformation, transport_blocks::BlockOutcome};
use umts_mac::hsupa::Reception;
use umts_runtime::mailbox::{self, Receiver, Sender};

const QUANTUM: usize = 192 * 1024;
const STACK_BYTES: usize = 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    identity: ReceiveIdentity,
    tti: u64,
}
struct Job {
    wake: thread::Thread,
    key: Key,
    harq: HarqReceiver,
    reply: Sender<Reply>,
    alive: Arc<AtomicBool>,
}
struct Meta {
    feedback: Reception,
    outcome: BlockOutcome,
    combined: bool,
    reset_reason: Option<umts_mac::hsupa::ResetReason>,
    payload_len: Option<usize>,
}
struct Reply {
    key: Key,
    harq: HarqReceiver,
    result: Result<Meta, ReceiveError>,
}
#[derive(Default)]
struct Counts {
    updates: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WorkerCounters {
    pub state_updates: u64,
    pub completed: u64,
    pub cancelled: u64,
}
struct Shared {
    sender: Sender<Job>,
    thread: OnceLock<thread::Thread>,
    stop: AtomicBool,
    failed: AtomicBool,
    producers: AtomicUsize,
    counts: Counts,
    #[cfg(test)]
    paused: AtomicBool,
    #[cfg(test)]
    panic_next: AtomicBool,
}
#[derive(Default)]
struct Registry {
    worker: Mutex<Weak<Shared>>,
}
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}
impl Registry {
    fn lookup(&self) -> Result<Option<Arc<Shared>>, ReceiveError> {
        let worker = self
            .worker
            .lock()
            .map_err(|_| ReceiveError::Budget)?
            .upgrade();
        if worker
            .as_ref()
            .is_some_and(|v| v.failed.load(Ordering::Acquire) || v.stop.load(Ordering::Acquire))
        {
            return Err(ReceiveError::Budget);
        }
        Ok(worker)
    }
}
pub(crate) struct Worker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}
impl Worker {
    fn capacity(maximum_ues: u32) -> io::Result<NonZeroUsize> {
        usize::try_from(maximum_ues)
            .ok()
            .and_then(|v| v.checked_mul(2))
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid HSUPA decoder capacity",
                )
            })
    }
    pub(crate) fn required_storage_bytes(maximum_ues: u32) -> Option<usize> {
        let capacity = Self::capacity(maximum_ues).ok()?;
        mailbox::retained_storage_bound::<Job>(capacity)?
            .checked_add(
                capacity
                    .get()
                    .checked_next_power_of_two()?
                    .checked_mul(std::mem::size_of::<Job>())?,
            )?
            .checked_add(std::mem::size_of::<Shared>() + STACK_BYTES + 65536)
    }
    pub(crate) fn start(maximum_ues: u32) -> io::Result<Self> {
        Self::start_in(Self::capacity(maximum_ues)?, registry(), true)
    }
    fn start_in(capacity: NonZeroUsize, registry: &Registry, configure: bool) -> io::Result<Self> {
        let mut registered = registry
            .worker
            .lock()
            .map_err(|_| io::Error::other("HSUPA decoder registry poisoned"))?;
        if registered
            .upgrade()
            .is_some_and(|v| !v.stop.load(Ordering::Acquire) && !v.failed.load(Ordering::Acquire))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "HSUPA decoder already running",
            ));
        }
        let (sender, mut receiver) = mailbox::bounded(capacity);
        let mut pending = VecDeque::new();
        pending.try_reserve_exact(capacity.get()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "HSUPA decoder queue allocation failed",
            )
        })?;
        let shared = Arc::new(Shared {
            sender,
            thread: OnceLock::new(),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            producers: AtomicUsize::new(0),
            counts: Counts::default(),
            #[cfg(test)]
            paused: AtomicBool::new(false),
            #[cfg(test)]
            panic_next: AtomicBool::new(false),
        });
        let owner = shared.clone();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("hnb-fec".into())
            .stack_size(STACK_BYTES)
            .spawn(move || {
                if configure
                    && let Err(error) = umts_runtime::scheduling::configure_current(
                        umts_runtime::scheduling::Role::RxDecode,
                    )
                {
                    let _ = ready.send(Err(error));
                    return;
                }
                owner.thread.set(thread::current()).ok();
                if ready.send(Ok(())).is_err() {
                    return;
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&owner, &mut receiver, pending, capacity.get())
                }));
                if result.is_err() || !owner.stop.load(Ordering::SeqCst) {
                    owner.failed.store(true, Ordering::Release);
                }
                owner.stop.store(true, Ordering::SeqCst);
                while owner.producers.load(Ordering::SeqCst) != 0 {
                    thread::yield_now();
                }
                for _ in 0..capacity.get() {
                    if receiver.try_recv().is_none() {
                        break;
                    }
                }
            })?;
        match started.recv() {
            Ok(Ok(())) => {}
            other => {
                let _ = join.join();
                return Err(match other {
                    Ok(Err(e)) => e,
                    _ => io::Error::other("HSUPA decoder startup disconnected"),
                });
            }
        }
        *registered = Arc::downgrade(&shared);
        Ok(Self {
            shared,
            join: Some(join),
        })
    }
    pub(crate) fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire) || self.shared.stop.load(Ordering::Acquire)
    }
    pub(crate) fn counters(&self) -> WorkerCounters {
        WorkerCounters {
            state_updates: self.shared.counts.updates.load(Ordering::Relaxed),
            completed: self.shared.counts.completed.load(Ordering::Relaxed),
            cancelled: self.shared.counts.cancelled.load(Ordering::Relaxed),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            if join.join().is_err() {
                tracing::error!("HSUPA decoder thread failed during shutdown");
            }
        }
    }
}
fn run(shared: &Shared, input: &mut Receiver<Job>, mut pending: VecDeque<Job>, capacity: usize) {
    while !shared.stop.load(Ordering::SeqCst) {
        #[cfg(test)]
        if shared.panic_next.swap(false, Ordering::AcqRel) {
            panic!("requested decoder test panic")
        }
        #[cfg(test)]
        if shared.paused.load(Ordering::Acquire) {
            thread::park_timeout(std::time::Duration::from_millis(1));
            continue;
        }
        for _ in 0..capacity {
            if pending.len() == capacity {
                break;
            }
            let Some(job) = input.try_recv() else { break };
            pending.push_back(job);
        }
        let Some(mut job) = pending.pop_front() else {
            thread::park();
            continue;
        };
        if !job.alive.load(Ordering::Acquire) {
            shared.counts.cancelled.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let mut work = DecodeWork::new(NonZeroUsize::new(QUANTUM).unwrap());
        let before = work.remaining();
        let result = match job.harq.service(&mut work) {
            Ok(Some(v)) => Some(Ok(Meta {
                feedback: v.feedback,
                outcome: v.outcome,
                combined: v.combined,
                reset_reason: v.reset_reason,
                payload_len: v.payload.map(<[_]>::len),
            })),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        };
        shared
            .counts
            .updates
            .fetch_add((before - work.remaining()) as u64, Ordering::Relaxed);
        if let Some(result) = result {
            shared.counts.completed.fetch_add(1, Ordering::Relaxed);
            if job.alive.load(Ordering::Acquire) && !shared.stop.load(Ordering::SeqCst) {
                let published = job
                    .reply
                    .try_send(Reply {
                        key: job.key,
                        harq: job.harq,
                        result,
                    })
                    .is_ok();
                if !published {
                    shared.failed.store(true, Ordering::Release);
                }
                job.wake.unpark();
                if !published {
                    break;
                }
            } else {
                shared.counts.cancelled.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            pending.push_back(job);
        }
    }
}

pub(crate) struct Backend {
    identity: ReceiveIdentity,
    owned: Option<HarqReceiver>,
    shared: Option<Arc<Shared>>,
    reply_tx: Sender<Reply>,
    reply_rx: Receiver<Reply>,
    alive: Arc<AtomicBool>,
    key: Option<Key>,
    started: Option<Key>,
    systematic_left: usize,
    bytes: usize,
    maximum_payload: usize,
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(shared) = &self.shared
            && let Some(thread) = shared.thread.get()
        {
            thread.unpark();
        }
    }
}
impl Backend {
    pub(crate) fn new(identity: ReceiveIdentity, harq: HarqReceiver) -> Result<Self, ReceiveError> {
        Self::with_registry(identity, harq, registry())
    }
    fn with_registry(
        identity: ReceiveIdentity,
        harq: HarqReceiver,
        registry: &Registry,
    ) -> Result<Self, ReceiveError> {
        let maximum_payload = harq.payload.len();
        let bytes = harq
            .retained_bytes()?
            .checked_sub(std::mem::size_of::<HarqReceiver>())
            .and_then(|v| v.checked_add(std::mem::size_of::<Self>()))
            .and_then(|v| {
                v.checked_add(mailbox::retained_storage_bound::<Reply>(NonZeroUsize::MIN)?)
            })
            .and_then(|v| v.checked_add(64))
            .ok_or(ReceiveError::Budget)?;
        let (reply_tx, reply_rx) = mailbox::bounded(NonZeroUsize::MIN);
        Ok(Self {
            identity,
            owned: Some(harq),
            shared: registry.lookup()?,
            reply_tx,
            reply_rx,
            alive: Arc::new(AtomicBool::new(true)),
            key: None,
            started: None,
            systematic_left: 0,
            bytes,
            maximum_payload,
        })
    }
    pub(crate) fn retained_bytes(&self) -> Result<usize, ReceiveError> {
        Ok(self.bytes)
    }
    pub(crate) fn maximum_payload(&self) -> usize {
        self.maximum_payload
    }
    pub(crate) fn expected_retransmission(
        &self,
        tti: u64,
    ) -> Option<(umts_types::hsupa::Etfci, umts_types::hsupa::Rsn)> {
        self.owned
            .as_ref()
            .and_then(|h| h.expected_retransmission(tti))
    }
    pub(crate) fn begin(
        &mut self,
        ue: umts_types::UeKey,
        tti: u64,
        control: ControlInformation,
        physical: &[f32],
    ) -> Result<(), ReceiveError> {
        let harq = self.owned.as_mut().ok_or(ReceiveError::NotReady)?;
        harq.begin(ue, tti, control, physical)?;
        self.started = Some(Key {
            identity: self.identity,
            tti,
        });
        self.systematic_left = harq.pending.map_or(0, |pending| {
            harq.formats[pending.format].plan.coded_bits() / 3
        });
        Ok(())
    }
    fn view(&self, meta: Meta) -> Received<'_> {
        Received {
            feedback: meta.feedback,
            outcome: meta.outcome,
            combined: meta.combined,
            reset_reason: meta.reset_reason,
            payload: meta
                .payload_len
                .map(|n| &self.owned.as_ref().unwrap().payload[..n]),
        }
    }
    pub(crate) fn service(
        &mut self,
        work: &mut DecodeWork,
    ) -> Result<Option<Received<'_>>, ReceiveError> {
        if self
            .shared
            .as_ref()
            .is_some_and(|s| s.failed.load(Ordering::Acquire) || s.stop.load(Ordering::Acquire))
        {
            return Err(ReceiveError::Budget);
        }
        if self.key.is_none() {
            if self.shared.is_none() {
                return self
                    .owned
                    .as_mut()
                    .ok_or(ReceiveError::NotReady)?
                    .service(work);
            }
            if self.started.is_none() {
                return Ok(None);
            }
            if self.systematic_left != 0 {
                let before = work.remaining();
                let allowance = self.systematic_left;
                let limit = NonZeroUsize::new(allowance).unwrap();
                let result = work.with_limit(limit, |stage| {
                    self.owned.as_mut().unwrap().service(stage).map(|v| {
                        v.map(|v| Meta {
                            feedback: v.feedback,
                            outcome: v.outcome,
                            combined: v.combined,
                            reset_reason: v.reset_reason,
                            payload_len: v.payload.map(<[_]>::len),
                        })
                    })
                });
                let spent = before - work.remaining();
                self.systematic_left = allowance.saturating_sub(spent);
                if before >= allowance && spent < allowance {
                    self.systematic_left = 0;
                }
                match result {
                    Ok(Some(meta)) => {
                        self.started = None;
                        self.systematic_left = 0;
                        return Ok(Some(self.view(meta)));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.started = None;
                        return Err(e);
                    }
                }
                if self.systematic_left != 0 {
                    return Ok(None);
                }
            }
            if self.owned.as_ref().unwrap().pending.is_none() {
                self.started = None;
                return Ok(None);
            }
            let shared = self.shared.as_ref().unwrap();
            shared.producers.fetch_add(1, Ordering::SeqCst);
            if shared.stop.load(Ordering::SeqCst) || shared.failed.load(Ordering::Acquire) {
                shared.producers.fetch_sub(1, Ordering::SeqCst);
                return Err(ReceiveError::Budget);
            }
            let key = self.started.unwrap();
            let job = Job {
                wake: thread::current(),
                key,
                harq: self.owned.take().unwrap(),
                reply: self.reply_tx.clone(),
                alive: self.alive.clone(),
            };
            let queued = shared.sender.try_send(job);
            shared.producers.fetch_sub(1, Ordering::SeqCst);
            if let Err(job) = queued {
                self.owned = Some(job.harq);
                self.started = None;
                self.owned.as_mut().unwrap().cancel_pending()?;
                return Err(ReceiveError::Budget);
            }
            self.key = Some(key);
            if let Some(thread) = shared.thread.get() {
                thread.unpark();
            }
            return Ok(None);
        }
        let Some(reply) = self.reply_rx.try_recv() else {
            if self
                .shared
                .as_ref()
                .is_some_and(|s| s.failed.load(Ordering::Acquire) || s.stop.load(Ordering::Acquire))
            {
                return Err(ReceiveError::Budget);
            }
            return Ok(None);
        };
        if Some(reply.key) != self.key {
            return Err(ReceiveError::Harq(umts_mac::hsupa::HarqError::Identity));
        }
        self.key = None;
        self.started = None;
        self.owned = Some(reply.harq);
        let meta = reply.result?;
        Ok(Some(self.view(meta)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        num::{NonZeroU8, NonZeroU64},
        time::{Duration, Instant},
    };
    use umts_dsp::{
        bits::Bit,
        hsupa::{TransportEncoder, TransportPlan},
    };
    use umts_types::{
        RadioEpoch, UeKey,
        hsupa::{Etfci, Rel6Category, Rsn, Tti},
    };
    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }
    fn fixture(
        slot: u32,
        bits: usize,
    ) -> (
        ReceiveIdentity,
        HarqReceiver,
        HarqReceiver,
        ControlInformation,
        Vec<f32>,
        Vec<Bit>,
    ) {
        let identity = ReceiveIdentity {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::MIN),
                slot,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        };
        let spreads: &[u16] = if bits > 1000 { &[2, 2, 4, 4] } else { &[16] };
        let plan = Arc::new(
            TransportPlan::new(
                nz(bits),
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                spreads,
                nz(100_000),
            )
            .unwrap(),
        );
        let format = Etfci::new(10).unwrap();
        let control = ControlInformation {
            format,
            sequence: Rsn::new(0).unwrap(),
            happy: true,
        };
        let payload: Vec<_> = (0..bits).map(|n| Bit::from(n % 3 == 0)).collect();
        let mut physical = vec![Bit::Zero; plan.physical_bits()];
        TransportEncoder::new(plan.clone())
            .unwrap()
            .encode(&payload, 0, &mut physical)
            .unwrap();
        let evidence = physical
            .iter()
            .map(|b| if *b == Bit::Zero { 5. } else { -5. })
            .collect();
        let make = || {
            HarqReceiver::new(
                identity.ue,
                Tti::TwoMilliseconds,
                true,
                vec![(format, plan.clone())],
                NonZeroU8::new(4).unwrap(),
                nz(8 * 1024 * 1024),
            )
            .unwrap()
        };
        (identity, make(), make(), control, evidence, payload)
    }
    fn result(v: Received<'_>) -> (Reception, BlockOutcome, bool, Option<Vec<Bit>>) {
        (
            v.feedback,
            v.outcome,
            v.combined,
            v.payload.map(|p| p.to_vec()),
        )
    }

    fn wait(b: &mut Backend) -> (Reception, BlockOutcome, bool, Option<Vec<Bit>>) {
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            let mut work = DecodeWork::new(nz(17));
            if let Some(v) = b.service(&mut work).unwrap() {
                return result(v);
            }
            assert!(Instant::now() < end);
            thread::yield_now();
        }
    }
    fn noise(input: &mut [f32]) {
        for (n, v) in input.iter_mut().enumerate() {
            *v = if n.wrapping_mul(1103515245).wrapping_add(12345) % 17 < 8 {
                0.2
            } else {
                -0.2
            };
        }
    }
    fn submit(b: &mut Backend) {
        let mut work = DecodeWork::new(NonZeroUsize::MAX);
        assert!(b.service(&mut work).unwrap().is_none());
        assert!(b.key.is_some());
    }
    #[test]
    fn crc_first_budget_and_harq_match_inline() {
        let registry = Registry::default();
        let pool = Worker::start_in(nz(4), &registry, false).unwrap();
        pool.shared.paused.store(true, Ordering::Release);
        let (id, raw, mut reference, c, mut evidence, _) = fixture(0, 11484);
        noise(&mut evidence);
        reference.begin(id.ue, 0, c, &evidence).unwrap();
        let expected = result(
            reference
                .service(&mut DecodeWork::new(NonZeroUsize::MAX))
                .unwrap()
                .unwrap(),
        );
        let mut b = Backend::with_registry(id, raw, &registry).unwrap();
        b.begin(id.ue, 0, c, &evidence).unwrap();
        let mut tiny = DecodeWork::new(nz(1));
        assert!(b.service(&mut tiny).unwrap().is_none());
        assert_eq!(tiny.remaining(), 0);
        assert!(b.key.is_none());
        submit(&mut b);
        let (ci, raw, _, cc, clean, payload) = fixture(1, 120);
        let mut quick = Backend::with_registry(ci, raw, &registry).unwrap();
        quick.begin(ci.ue, 0, cc, &clean).unwrap();
        assert_eq!(wait(&mut quick).3.as_deref(), Some(payload.as_slice()));
        assert_eq!(
            pool.counters().state_updates,
            0,
            "CRC-clean blocks must finish inline while the worker is paused"
        );
        pool.shared.paused.store(false, Ordering::Release);
        pool.shared.thread.get().unwrap().unpark();
        assert_eq!(wait(&mut b), expected);
        let mut retx = c;
        retx.sequence = Rsn::new(1).unwrap();
        reference.begin(id.ue, 8, retx, &evidence).unwrap();
        let expected = result(
            reference
                .service(&mut DecodeWork::new(NonZeroUsize::MAX))
                .unwrap()
                .unwrap(),
        );
        b.begin(id.ue, 8, retx, &evidence).unwrap();
        assert_eq!(wait(&mut b), expected);
        assert_eq!(
            quick.begin(id.ue, 1, cc, &clean),
            Err(ReceiveError::Harq(umts_mac::hsupa::HarqError::Identity))
        );
        assert!(pool.counters().state_updates > 0);
    }
    #[test]
    fn bounded_queue_returns_owned_state_and_shutdown_releases_jobs() {
        let registry = Registry::default();
        let pool = Worker::start_in(nz(1), &registry, false).unwrap();
        pool.shared.paused.store(true, Ordering::Release);
        let (ai, raw, _, ac, mut ap, _) = fixture(0, 11484);
        noise(&mut ap);
        let mut a = Backend::with_registry(ai, raw, &registry).unwrap();
        a.begin(ai.ue, 0, ac, &ap).unwrap();
        submit(&mut a);
        let (bi, raw, _, bc, mut bp, _) = fixture(1, 120);
        noise(&mut bp);
        let mut b = Backend::with_registry(bi, raw, &registry).unwrap();
        b.begin(bi.ue, 0, bc, &bp).unwrap();
        assert!(matches!(
            b.service(&mut DecodeWork::new(NonZeroUsize::MAX)),
            Err(ReceiveError::Budget)
        ));
        assert!(b.owned.is_some() && b.owned.as_ref().unwrap().pending.is_none());
        let alive = Arc::downgrade(&a.alive);
        drop(a);
        drop(pool);
        assert!(alive.upgrade().is_none());
    }
    #[test]
    fn stopped_worker_can_be_replaced_without_adopting_old_owners() {
        let registry = Registry::default();
        let first = Worker::start_in(nz(2), &registry, false).unwrap();
        assert!(Worker::start_in(nz(2), &registry, false).is_err());
        let (id, raw, _, c, evidence, payload) = fixture(0, 120);
        let mut old = Backend::with_registry(id, raw, &registry).unwrap();
        drop(first);
        let second = Worker::start_in(nz(2), &registry, false).unwrap();
        assert!(matches!(
            old.service(&mut DecodeWork::new(nz(17))),
            Err(ReceiveError::Budget)
        ));
        let (_, raw, _, _, _, _) = fixture(0, 120);
        let mut fresh = Backend::with_registry(id, raw, &registry).unwrap();
        fresh.begin(id.ue, 0, c, &evidence).unwrap();
        assert_eq!(wait(&mut fresh).3.as_deref(), Some(payload.as_slice()));
        assert!(!second.failed());
    }
    #[test]
    fn panic_drains_queued_owners_and_reports_failure() {
        let registry = Registry::default();
        let pool = Worker::start_in(nz(2), &registry, false).unwrap();
        pool.shared.paused.store(true, Ordering::Release);
        let (id, raw, _, c, mut input, _) = fixture(0, 11484);
        noise(&mut input);
        let mut b = Backend::with_registry(id, raw, &registry).unwrap();
        b.begin(id.ue, 0, c, &input).unwrap();
        submit(&mut b);
        let alive = Arc::downgrade(&b.alive);
        drop(b);
        pool.shared.panic_next.store(true, Ordering::Release);
        pool.shared.thread.get().unwrap().unpark();
        let end = Instant::now() + Duration::from_secs(2);
        while !pool.failed() {
            assert!(Instant::now() < end);
            thread::yield_now();
        }
        drop(pool);
        assert!(alive.upgrade().is_none());
    }
    #[test]
    fn stale_configuration_reply_is_rejected() {
        let registry = Registry::default();
        let _pool = Worker::start_in(nz(1), &registry, false).unwrap();
        let (id, raw, _, _, _, _) = fixture(0, 120);
        let mut b = Backend::with_registry(id, raw, &registry).unwrap();
        let key = Key {
            identity: id,
            tti: 9,
        };
        b.key = Some(key);
        let mut wrong = key;
        wrong.identity.configuration = NonZeroU64::new(2).unwrap();
        assert!(
            b.reply_tx
                .try_send(Reply {
                    key: wrong,
                    harq: b.owned.take().unwrap(),
                    result: Err(ReceiveError::Budget)
                })
                .is_ok()
        );
        assert!(matches!(
            b.service(&mut DecodeWork::new(nz(1))),
            Err(ReceiveError::Harq(umts_mac::hsupa::HarqError::Identity))
        ));
    }
}
