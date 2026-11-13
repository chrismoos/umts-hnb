// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ConfigurationError, HarqConfiguration};
use std::num::NonZeroUsize;
use umts_rrc_schema::information_elements as ie;

const MAX_QUEUES: usize = 8;
const SIZE_INDICES: usize = 8;
const MAX_PDU_BITS: usize = 5000;

#[derive(Clone)]
pub struct QueueConfiguration {
    id: u8,
    flow: u8,
    sizes: [Option<NonZeroUsize>; SIZE_INDICES],
    timer: ie::T1ReleaseTimer,
    window: ie::MACHsWindowSize,
}

impl QueueConfiguration {
    pub fn new(
        id: u8,
        flow: u8,
        sizes: [Option<NonZeroUsize>; SIZE_INDICES],
        timer_ms: u16,
        window: u8,
    ) -> Result<Self, ConfigurationError> {
        if usize::from(id) >= MAX_QUEUES || usize::from(flow) >= MAX_QUEUES {
            return Err(ConfigurationError::Queue);
        }
        if sizes.iter().all(Option::is_none)
            || sizes.iter().flatten().any(|size| size.get() > MAX_PDU_BITS)
        {
            return Err(ConfigurationError::QueueSize);
        }
        let timer = match timer_ms {
            10 => ie::T1ReleaseTimer::rt10,
            20 => ie::T1ReleaseTimer::rt20,
            30 => ie::T1ReleaseTimer::rt30,
            40 => ie::T1ReleaseTimer::rt40,
            50 => ie::T1ReleaseTimer::rt50,
            60 => ie::T1ReleaseTimer::rt60,
            70 => ie::T1ReleaseTimer::rt70,
            80 => ie::T1ReleaseTimer::rt80,
            90 => ie::T1ReleaseTimer::rt90,
            100 => ie::T1ReleaseTimer::rt100,
            120 => ie::T1ReleaseTimer::rt120,
            140 => ie::T1ReleaseTimer::rt140,
            160 => ie::T1ReleaseTimer::rt160,
            200 => ie::T1ReleaseTimer::rt200,
            300 => ie::T1ReleaseTimer::rt300,
            400 => ie::T1ReleaseTimer::rt400,
            _ => return Err(ConfigurationError::Reordering),
        };
        let window = match window {
            4 => ie::MACHsWindowSize::mws4,
            6 => ie::MACHsWindowSize::mws6,
            8 => ie::MACHsWindowSize::mws8,
            12 => ie::MACHsWindowSize::mws12,
            16 => ie::MACHsWindowSize::mws16,
            24 => ie::MACHsWindowSize::mws24,
            32 => ie::MACHsWindowSize::mws32,
            _ => return Err(ConfigurationError::Reordering),
        };
        Ok(Self {
            id,
            flow,
            sizes,
            timer,
            window,
        })
    }

    pub fn id(&self) -> u8 {
        self.id
    }

    pub fn flow(&self) -> u8 {
        self.flow
    }

    pub fn sizes(&self) -> &[Option<NonZeroUsize>; SIZE_INDICES] {
        &self.sizes
    }

    pub fn wire(&self) -> ie::MACHsAddReconfQueue {
        let sizes = self
            .sizes
            .iter()
            .enumerate()
            .filter_map(|(index, size)| {
                size.map(|size| ie::MACDPDUsizeInfo::new(size.get() as u16, index as u8))
            })
            .collect();
        ie::MACHsAddReconfQueue::new(
            self.id,
            ie::MACDFlowIdentity(self.flow),
            self.timer,
            self.window,
            Some(ie::MACDPDUSizeInfoList(sizes)),
        )
    }
}

#[derive(Clone)]
pub struct QueueSet {
    queues: Box<[QueueConfiguration]>,
}

impl QueueSet {
    pub fn new(queues: Vec<QueueConfiguration>) -> Result<Self, ConfigurationError> {
        if queues.is_empty() || queues.len() > MAX_QUEUES {
            return Err(ConfigurationError::Queue);
        }
        let mut ids = 0_u8;
        for queue in &queues {
            let bit = 1 << queue.id();
            if ids & bit != 0 {
                return Err(ConfigurationError::Queue);
            }
            ids |= bit;
        }
        Ok(Self {
            queues: queues.into_boxed_slice(),
        })
    }

    pub fn queues(&self) -> &[QueueConfiguration] {
        &self.queues
    }

    pub fn initial_transport_info_r7(&self, harq: &HarqConfiguration) -> ie::HSDSCHInfoR7 {
        let original = harq.wire();
        let count = match original.number_of_processes {
            1 => ie::HARQInfoR7NumberOfProcesses::n1,
            2 => ie::HARQInfoR7NumberOfProcesses::n2,
            3 => ie::HARQInfoR7NumberOfProcesses::n3,
            4 => ie::HARQInfoR7NumberOfProcesses::n4,
            5 => ie::HARQInfoR7NumberOfProcesses::n5,
            6 => ie::HARQInfoR7NumberOfProcesses::n6,
            7 => ie::HARQInfoR7NumberOfProcesses::n7,
            _ => ie::HARQInfoR7NumberOfProcesses::n8,
        };
        let partition = match original.memory_partitioning {
            ie::HARQInfoMemoryPartitioning::implicit(()) => {
                ie::HARQInfoR7MemoryPartitioning::implicit(())
            }
            ie::HARQInfoMemoryPartitioning::explicit(sizes) => {
                ie::HARQInfoR7MemoryPartitioning::explicit(
                    ie::HARQInfoR7MemoryPartitioningExplicit::new(sizes, None),
                )
            }
        };
        let queues = self
            .queues
            .iter()
            .map(|queue| {
                ie::MACEhsAddReconfReordQ::new(
                    ie::MACEhsQueueId(queue.id),
                    queue.timer,
                    None,
                    queue.window,
                )
            })
            .collect();
        ie::HSDSCHInfoR7::new(
            Some(ie::HARQInfoR7::new(count, partition)),
            Some(ie::HSDSCHInfoR7DlMACHeaderType::mac_ehs(
                ie::AddOrReconfMACEhsReordQ::new(Some(ie::MACEhsAddReconfReordQList(queues)), None),
            )),
        )
    }

    pub fn initial_transport_info(&self, harq: &HarqConfiguration) -> ie::HSDSCHInfo {
        let queues = self.queues.iter().map(QueueConfiguration::wire).collect();
        ie::HSDSCHInfo::new(
            Some(harq.wire()),
            Some(ie::AddOrReconfMACDFlow::new(
                Some(ie::MACHsAddReconfQueueList(queues)),
                None,
            )),
        )
    }
}
