// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ConfigurationError;
use std::num::{NonZeroU8, NonZeroUsize};
use umts_rrc_schema::information_elements as ie;
use umts_types::hsdpa::{CapabilityError, Category};

const MAX_PROCESSES: usize = 8;

#[derive(Clone)]
enum Partition {
    Implicit,
    Explicit(Box<[ie::HARQMemorySize]>),
}

#[derive(Clone)]
pub struct HarqConfiguration {
    category: Category,
    memory: Box<[NonZeroUsize]>,
    partition: Partition,
}

impl HarqConfiguration {
    pub fn implicit(category: Category, count: NonZeroU8) -> Result<Self, ConfigurationError> {
        let count = usize::from(count.get());
        if count > MAX_PROCESSES {
            return Err(ConfigurationError::Capability(
                CapabilityError::ProcessCount,
            ));
        }
        // TS 25.331 8.6.5.6b rounds down each equal single-cell, non-MIMO partition.
        let size = NonZeroUsize::new(category.limits().total_soft_bits / count)
            .ok_or(ConfigurationError::MemorySize)?;
        Ok(Self {
            category,
            memory: vec![size; count].into_boxed_slice(),
            partition: Partition::Implicit,
        })
    }

    pub fn explicit(
        category: Category,
        memory: &[NonZeroUsize],
    ) -> Result<Self, ConfigurationError> {
        category
            .validate_process_memory(memory)
            .map_err(ConfigurationError::Capability)?;
        let encoded = memory
            .iter()
            .map(|size| explicit_size(size.get()))
            .collect::<Result<Box<[_]>, _>>()?;
        Ok(Self {
            category,
            memory: memory.into(),
            partition: Partition::Explicit(encoded),
        })
    }

    pub fn category(&self) -> Category {
        self.category
    }

    pub fn process_memory(&self) -> &[NonZeroUsize] {
        &self.memory
    }

    pub fn wire(&self) -> ie::HARQInfo {
        let partition = match &self.partition {
            Partition::Implicit => ie::HARQInfoMemoryPartitioning::implicit(()),
            Partition::Explicit(sizes) => ie::HARQInfoMemoryPartitioning::explicit(sizes.to_vec()),
        };
        ie::HARQInfo::new(self.memory.len() as u8, partition)
    }
}

fn explicit_size(bits: usize) -> Result<ie::HARQMemorySize, ConfigurationError> {
    // TS 25.331 10.3.5.7a has five step sizes, not one linear enum.
    macro_rules! sizes {
        ($($bits:literal => $wire:ident),+ $(,)?) => {
            match bits {
                $($bits => Ok(ie::HARQMemorySize::$wire),)+
                _ => Err(ConfigurationError::MemorySize),
            }
        };
    }
    sizes! {
        800 => hms800, 1600 => hms1600, 2400 => hms2400, 3200 => hms3200,
        4000 => hms4000, 4800 => hms4800, 5600 => hms5600, 6400 => hms6400,
        7200 => hms7200, 8000 => hms8000, 8800 => hms8800, 9600 => hms9600,
        10400 => hms10400, 11200 => hms11200, 12000 => hms12000, 12800 => hms12800,
        13600 => hms13600, 14400 => hms14400, 15200 => hms15200, 16000 => hms16000,
        17600 => hms17600, 19200 => hms19200, 20800 => hms20800, 22400 => hms22400,
        24000 => hms24000, 25600 => hms25600, 27200 => hms27200, 28800 => hms28800,
        30400 => hms30400, 32000 => hms32000, 36000 => hms36000, 40000 => hms40000,
        44000 => hms44000, 48000 => hms48000, 52000 => hms52000, 56000 => hms56000,
        60000 => hms60000, 64000 => hms64000, 68000 => hms68000, 72000 => hms72000,
        76000 => hms76000, 80000 => hms80000, 88000 => hms88000, 96000 => hms96000,
        104000 => hms104000, 112000 => hms112000, 120000 => hms120000,
        128000 => hms128000, 136000 => hms136000, 144000 => hms144000,
        152000 => hms152000, 160000 => hms160000, 176000 => hms176000,
        192000 => hms192000, 208000 => hms208000, 224000 => hms224000,
        240000 => hms240000, 256000 => hms256000, 272000 => hms272000,
        288000 => hms288000, 304000 => hms304000,
    }
}
