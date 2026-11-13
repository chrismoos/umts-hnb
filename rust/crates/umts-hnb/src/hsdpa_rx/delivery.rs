// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, Ordering},
};
use umts_runtime::mailbox::{self, Receiver, Sender};

const MAX_REQUESTS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum Request {
    OrderAck {
        start: ChipInstant,
        schedule: FeedbackModeSchedule,
    },
    AckCells {
        start: ChipInstant,
        repetitions: u8,
        schedule: FeedbackModeSchedule,
    },
    CqiCells {
        start: ChipInstant,
        repetitions: u8,
        schedule: FeedbackModeSchedule,
    },
    Ack {
        transmission: ScheduledTransmission,
        repetitions: u8,
    },
    Cqi {
        // HS-DPCCH subframe origin. The receiver applies the one-slot CQI offset.
        start: ChipInstant,
        repetitions: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId {
    slot: usize,
    serial: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum Evidence<T> {
    Missing,
    Measured(Observation<T>),
}

#[derive(Clone, Copy, Debug)]
pub enum Measurement {
    AckCells(Evidence<CarrierAcknowledgements>),
    CqiCells(Evidence<CarrierCqi>),
    Ack(Evidence<HarqAck>),
    Cqi(Evidence<Cqi>),
}

#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub id: RequestId,
    pub request: Request,
    pub result: Result<Measurement, DspError>,
    pub processed_until: Option<ChipInstant>,
}

#[derive(Clone, Copy)]
struct Pending {
    id: RequestId,
    request: Request,
}

struct Link {
    producer: AtomicBool,
    receiver: AtomicBool,
}

pub struct FeedbackPort {
    ue: UeKey,
    link: Arc<Link>,
    commands: Sender<Pending>,
    reports: Receiver<Report>,
    pending: Box<[Option<Pending>]>,
    next: Option<u64>,
}

impl FeedbackPort {
    pub fn ue(&self) -> UeKey {
        self.ue
    }
    pub fn available(&self) -> bool {
        self.link.receiver.load(Ordering::Acquire)
    }

    pub fn request(&mut self, request: Request) -> Result<RequestId, Request> {
        let (time, repetitions) = match request {
            Request::OrderAck { start, schedule } => {
                if schedule.window(start, 1).is_err() {
                    return Err(request);
                }
                (start, 1)
            }
            Request::Ack {
                transmission,
                repetitions,
            } if transmission.key.ue() == self.ue => (transmission.expected_ack, repetitions),
            Request::Ack { .. } => return Err(request),
            Request::Cqi { start, repetitions } => (start, repetitions),
            Request::AckCells {
                start,
                repetitions,
                schedule,
            }
            | Request::CqiCells {
                start,
                repetitions,
                schedule,
            } => {
                if schedule.window(start, repetitions).is_err() {
                    return Err(request);
                }
                (start, repetitions)
            }
        };
        if !self.available()
            || time.epoch != self.ue.epoch
            || !(1..=MAX_REPETITIONS).contains(&repetitions)
        {
            return Err(request);
        }
        let Some(serial) = self.next else {
            return Err(request);
        };
        let Some(slot) = self.pending.iter().position(Option::is_none) else {
            return Err(request);
        };
        let pending = Pending {
            id: RequestId { slot, serial },
            request,
        };
        self.commands.try_send(pending).map_err(|_| request)?;
        self.pending[slot] = Some(pending);
        self.next = serial.checked_add(1);
        Ok(pending.id)
    }

    pub fn try_report(&mut self) -> Option<Report> {
        let report = self.reports.try_recv()?;
        let pending = self
            .pending
            .get_mut(report.id.slot)
            .expect("owned request slot");
        assert!(
            pending
                .as_ref()
                .is_some_and(|pending| pending.id == report.id),
            "owned request identity"
        );
        *pending = None;
        Some(report)
    }

    pub fn outstanding(&self) -> usize {
        self.pending.iter().flatten().count()
    }

    #[cfg(test)]
    pub fn pending_requests(&self) -> impl Iterator<Item = (RequestId, Request)> + '_ {
        self.pending
            .iter()
            .flatten()
            .map(|pending| (pending.id, pending.request))
    }
}

impl Drop for FeedbackPort {
    fn drop(&mut self) {
        self.link.producer.store(false, Ordering::Release);
    }
}

pub(super) struct Delivery {
    link: Arc<Link>,
    commands: Receiver<Pending>,
    reports: Sender<Report>,
    pending: Box<[Option<Pending>]>,
    blocked: Option<Report>,
    budget: usize,
    cursor: usize,
}

impl Delivery {
    pub(super) fn new(
        ue: UeKey,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<(Self, FeedbackPort), DspError> {
        if capacity.get() > MAX_REQUESTS || budget.get() > MAX_REQUESTS {
            return Err(DspError::Capacity);
        }
        let (send, commands) = mailbox::bounded(capacity);
        let (reports, receive) = mailbox::bounded(capacity);
        let link = Arc::new(Link {
            producer: AtomicBool::new(true),
            receiver: AtomicBool::new(true),
        });
        Ok((
            Self {
                link: link.clone(),
                commands,
                reports,
                pending: vec![None; capacity.get()].into_boxed_slice(),
                blocked: None,
                budget: budget.get(),
                cursor: 0,
            },
            FeedbackPort {
                ue,
                link,
                commands: send,
                reports: receive,
                pending: vec![None; capacity.get()].into_boxed_slice(),
                next: Some(1),
            },
        ))
    }

    pub(super) fn retained_bytes(capacity: NonZeroUsize) -> Result<usize, DspError> {
        if capacity.get() > MAX_REQUESTS {
            return Err(DspError::Capacity);
        }
        // Queue payloads and both owners' request tables are charged before admission.
        Ok(std::mem::size_of::<Self>()
            + capacity.get()
                * (std::mem::size_of::<Pending>()
                    + std::mem::size_of::<Report>()
                    + 2 * std::mem::size_of::<Option<Pending>>()))
    }

    pub(super) fn service(&mut self, receiver: &FeedbackReceiver) {
        if !self.link.producer.load(Ordering::Acquire) {
            return;
        }
        if let Some(report) = self.blocked.take()
            && let Err(report) = self.reports.try_send(report)
        {
            self.blocked = Some(report);
            return;
        }
        for _ in 0..self.budget {
            let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_none()) else {
                break;
            };
            let Some(command) = self.commands.try_recv() else {
                break;
            };
            *slot = Some(command);
        }
        for _ in 0..self.budget.min(self.pending.len()) {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.pending.len();
            let Some(pending) = self.pending[index] else {
                continue;
            };
            let result = match pending.request {
                Request::OrderAck { start, schedule } => receiver
                    .ack_order(start, schedule)
                    .map(|value| complete(value).map(Measurement::AckCells)),
                Request::AckCells {
                    start,
                    repetitions,
                    schedule,
                } => receiver
                    .ack_cells(start, repetitions, schedule)
                    .map(|value| complete(value).map(Measurement::AckCells)),
                Request::CqiCells {
                    start,
                    repetitions,
                    schedule,
                } => receiver
                    .cqi_cells(start, repetitions, schedule)
                    .map(|value| complete(value).map(Measurement::CqiCells)),
                Request::Ack {
                    transmission,
                    repetitions,
                } => receiver
                    .observe(transmission, repetitions)
                    .map(|readout| readout.acknowledgement)
                    .map(|value| complete(value).map(Measurement::Ack)),
                Request::Cqi { start, repetitions } => receiver
                    .cqi(start, repetitions)
                    .map(|value| complete(value).map(Measurement::Cqi)),
            };
            let result = match result {
                Ok(None) => continue,
                Ok(Some(value)) => Ok(value),
                Err(error) => Err(error),
            };
            let report = Report {
                id: pending.id,
                request: pending.request,
                result,
                processed_until: receiver.processed_until(),
            };
            self.pending[index] = None;
            if let Err(report) = self.reports.try_send(report) {
                self.blocked = Some(report);
                break;
            }
        }
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.link.receiver.store(false, Ordering::Release);
    }
}

fn complete<T>(readout: Readout<T>) -> Option<Evidence<T>> {
    match readout {
        Readout::Pending => None,
        Readout::Missing => Some(Evidence::Missing),
        Readout::Observed(evidence) => Some(Evidence::Measured(evidence)),
    }
}

#[cfg(test)]
impl FeedbackReceiver {
    /// Inject already-decoded evidence at the receive/controller mailbox boundary.
    /// The request identity and immutable schedule come from the real producer.
    pub(crate) fn fixture_answer_next(&mut self, measurement: Measurement) -> Report {
        let delivery = self.delivery.as_mut().expect("connected fixture receiver");
        let pending = delivery
            .commands
            .try_recv()
            .expect("fixture feedback request");
        let report = Report {
            id: pending.id,
            request: pending.request,
            result: Ok(measurement),
            processed_until: None,
        };
        delivery
            .reports
            .try_send(report)
            .expect("fixture report capacity");
        report
    }
}
