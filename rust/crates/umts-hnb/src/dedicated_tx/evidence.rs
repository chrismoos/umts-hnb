// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::sync::atomic::{AtomicU8, Ordering};

const RESERVED: u8 = 0;
const PREPARED: u8 = 1;
const GENERATED: u8 = 2;
const FAILED: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmissionProgress {
    Reserved,
    Prepared,
    Generated,
    Failed,
    Closed,
}

#[derive(Clone)]
pub(crate) struct DedicatedFrameSource {
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
    link: Option<Arc<super::runtime::Link>>,
}
impl DedicatedFrameSource {
    pub(super) fn paired_progress(&self, progress: &crate::tx_evidence::Reader) -> bool {
        self.progress.paired_with(progress)
    }
    pub(super) fn belongs_to(&self, source: &crate::common_tx::CommonTx) -> bool {
        source.submission_paired_with(&self.progress)
    }
    pub(crate) fn new(
        progress: crate::tx_evidence::Reader,
        rate: usize,
        group_delay: usize,
    ) -> Self {
        Self {
            progress,
            rate,
            group_delay,
            link: None,
        }
    }
    pub(super) fn with_link(mut self, link: Arc<super::runtime::Link>) -> Self {
        self.link = Some(link);
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EmissionReceipt(Arc<AtomicU8>, Arc<super::runtime::Link>);
impl EmissionReceipt {
    pub(crate) fn snapshot(&self) -> EmissionProgress {
        match self.0.load(Ordering::Acquire) {
            GENERATED => EmissionProgress::Generated,
            FAILED => EmissionProgress::Failed,
            _ if !self.1.available() => EmissionProgress::Closed,
            RESERVED => EmissionProgress::Reserved,
            PREPARED => EmissionProgress::Prepared,
            _ => EmissionProgress::Failed,
        }
    }
    pub(crate) fn viable(&self) -> bool {
        let state = self.0.load(Ordering::Acquire);
        state != FAILED && (state == GENERATED || self.1.available())
    }
    pub(crate) fn generated(&self) -> bool {
        self.0.load(Ordering::Acquire) == GENERATED
    }
}

pub(crate) struct Emission {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub end: ChipInstant,
    pub tfci: Tfci,
    pub plan: Arc<CompiledRel99>,
    pub source: crate::tx_evidence::Reader,
    receipt: EmissionReceipt,
    pool: Option<Arc<()>>,
}
impl Emission {
    #[cfg(test)]
    pub(crate) fn new(
        identity: ReceiveIdentity,
        start: ChipInstant,
        end: ChipInstant,
        tfci: Tfci,
        plan: Arc<CompiledRel99>,
        source: &DedicatedFrameSource,
    ) -> Result<(Self, crate::tx_evidence::TransportSubmission), TransmitError> {
        let progress = source.progress.clone();
        let link = source
            .link
            .as_ref()
            .filter(|link| link.available())
            .ok_or(TransmitError::Closed)?;
        let receipt = EmissionReceipt(Arc::new(AtomicU8::new(RESERVED)), link.clone());
        let submission = progress
            .interval_receipt(start, end, source.rate, source.group_delay)
            .ok_or(TransmitError::Time)?
            .with_emission(receipt.clone());
        if !submission.viable() {
            return Err(TransmitError::SubmissionLost);
        }
        Ok((
            Self {
                identity,
                start,
                end,
                tfci,
                plan,
                source: progress,
                receipt,
                pool: None,
            },
            submission,
        ))
    }
    pub(crate) fn prepared(&mut self) {
        self.receipt.0.store(PREPARED, Ordering::Release);
    }
    pub(crate) fn pool(&self) -> Option<&Arc<()>> {
        self.pool.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn allocation_id(&self) -> usize {
        Arc::as_ptr(&self.receipt.0) as usize
    }
    pub(super) fn paired_runtime(&self, link: &Arc<super::runtime::Link>) -> bool {
        Arc::ptr_eq(&self.receipt.1, link)
    }
    pub(crate) fn generated_state(&self) -> bool {
        self.receipt.generated()
    }
    pub(crate) fn reusable(&self) -> bool {
        Arc::strong_count(&self.receipt.0) == 1
            && matches!(self.receipt.0.load(Ordering::Acquire), GENERATED | FAILED)
    }
    pub(crate) fn idle(
        identity: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        source: &DedicatedFrameSource,
        pool: Arc<()>,
    ) -> Result<Self, TransmitError> {
        if identity.ue.epoch != source.progress.origin().epoch {
            return Err(TransmitError::Identity);
        }
        let link = source
            .link
            .as_ref()
            .filter(|link| link.available())
            .ok_or(TransmitError::Closed)?;
        Ok(Self {
            identity,
            start: ChipInstant {
                epoch: identity.ue.epoch,
                index: 0,
            },
            end: ChipInstant {
                epoch: identity.ue.epoch,
                index: 0,
            },
            tfci: Tfci::new(0)?,
            plan,
            source: source.progress.clone(),
            receipt: EmissionReceipt(Arc::new(AtomicU8::new(FAILED)), link.clone()),
            pool: Some(pool),
        })
    }
    pub(crate) fn reset(
        &mut self,
        start: ChipInstant,
        end: ChipInstant,
        tfci: Tfci,
        source: &DedicatedFrameSource,
    ) -> Result<crate::tx_evidence::TransportSubmission, TransmitError> {
        if !self.reusable() || !self.source.paired_with(&source.progress) {
            return Err(TransmitError::Identity);
        }
        if source.link.as_ref().is_none_or(|link| !link.available()) {
            return Err(TransmitError::Closed);
        }
        if source
            .link
            .as_ref()
            .is_none_or(|link| !Arc::ptr_eq(link, &self.receipt.1))
        {
            return Err(TransmitError::Identity);
        }
        let submission = self
            .source
            .interval_receipt(start, end, source.rate, source.group_delay)
            .ok_or(TransmitError::Time)?;
        if !submission.viable() {
            return Err(TransmitError::SubmissionLost);
        }
        self.start = start;
        self.end = end;
        self.tfci = tfci;
        // A sole strong owner and no exposed Weak handles prevent any old reader from observing this reset.
        self.receipt.0.store(RESERVED, Ordering::Release);
        Ok(submission.with_emission(self.receipt.clone()))
    }
    pub(crate) fn generated(&mut self) {
        self.receipt.0.store(GENERATED, Ordering::Release);
    }
    pub(crate) fn fail(&mut self) {
        if !self.receipt.generated() {
            self.receipt.0.store(FAILED, Ordering::Release);
        }
    }
    pub(crate) fn viable(&self) -> bool {
        self.receipt.viable()
    }
}
impl Drop for Emission {
    fn drop(&mut self) {
        self.fail();
    }
}
