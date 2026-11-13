// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, ie};

pub(super) fn bits(value: ie::MaxNoBits) -> Result<usize, Error> {
    match value {
        ie::MaxNoBits::b640 => Ok(640),
        ie::MaxNoBits::b1280 => Ok(1280),
        ie::MaxNoBits::b2560 => Ok(2560),
        ie::MaxNoBits::b3840 => Ok(3840),
        ie::MaxNoBits::b5120 => Ok(5120),
        ie::MaxNoBits::b6400 => Ok(6400),
        ie::MaxNoBits::b7680 => Ok(7680),
        ie::MaxNoBits::b8960 => Ok(8960),
        ie::MaxNoBits::b10240 => Ok(10240),
        ie::MaxNoBits::b20480 => Ok(20480),
        ie::MaxNoBits::b40960 => Ok(40960),
        ie::MaxNoBits::b81920 => Ok(81920),
        ie::MaxNoBits::b163840 => Ok(163840),
    }
}

pub(super) fn physical_ul(value: ie::MaxNoDPDCHBitsTransmitted) -> Result<usize, Error> {
    match value {
        ie::MaxNoDPDCHBitsTransmitted::b600 => Ok(600),
        ie::MaxNoDPDCHBitsTransmitted::b1200 => Ok(1200),
        ie::MaxNoDPDCHBitsTransmitted::b2400 => Ok(2400),
        ie::MaxNoDPDCHBitsTransmitted::b4800 => Ok(4800),
        ie::MaxNoDPDCHBitsTransmitted::b9600 => Ok(9600),
        ie::MaxNoDPDCHBitsTransmitted::b19200 => Ok(19200),
        ie::MaxNoDPDCHBitsTransmitted::b28800 => Ok(28800),
        ie::MaxNoDPDCHBitsTransmitted::b38400 => Ok(38400),
        ie::MaxNoDPDCHBitsTransmitted::b48000 => Ok(48000),
        ie::MaxNoDPDCHBitsTransmitted::b57600 => Ok(57600),
    }
}

pub(super) fn physical_dl(value: ie::MaxNoPhysChBitsReceived) -> Result<usize, Error> {
    match value {
        ie::MaxNoPhysChBitsReceived::dummy => Err(Error::InvalidValue("MaxNoPhysChBitsReceived")),
        ie::MaxNoPhysChBitsReceived::b1200 => Ok(1200),
        ie::MaxNoPhysChBitsReceived::b2400 => Ok(2400),
        ie::MaxNoPhysChBitsReceived::b3600 => Ok(3600),
        ie::MaxNoPhysChBitsReceived::b4800 => Ok(4800),
        ie::MaxNoPhysChBitsReceived::b7200 => Ok(7200),
        ie::MaxNoPhysChBitsReceived::b9600 => Ok(9600),
        ie::MaxNoPhysChBitsReceived::b14400 => Ok(14400),
        ie::MaxNoPhysChBitsReceived::b19200 => Ok(19200),
        ie::MaxNoPhysChBitsReceived::b28800 => Ok(28800),
        ie::MaxNoPhysChBitsReceived::b38400 => Ok(38400),
        ie::MaxNoPhysChBitsReceived::b48000 => Ok(48000),
        ie::MaxNoPhysChBitsReceived::b57600 => Ok(57600),
        ie::MaxNoPhysChBitsReceived::b67200 => Ok(67200),
        ie::MaxNoPhysChBitsReceived::b76800 => Ok(76800),
    }
}

pub(super) fn formats(value: ie::MaxNumberOfTF) -> Result<usize, Error> {
    match value {
        ie::MaxNumberOfTF::tf32 => Ok(32),
        ie::MaxNumberOfTF::tf64 => Ok(64),
        ie::MaxNumberOfTF::tf128 => Ok(128),
        ie::MaxNumberOfTF::tf256 => Ok(256),
        ie::MaxNumberOfTF::tf512 => Ok(512),
        ie::MaxNumberOfTF::tf1024 => Ok(1024),
    }
}

pub(super) fn combinations_dl(value: ie::MaxNumberOfTFCDL) -> Result<usize, Error> {
    match value {
        ie::MaxNumberOfTFCDL::tfc16 => Ok(16),
        ie::MaxNumberOfTFCDL::tfc32 => Ok(32),
        ie::MaxNumberOfTFCDL::tfc48 => Ok(48),
        ie::MaxNumberOfTFCDL::tfc64 => Ok(64),
        ie::MaxNumberOfTFCDL::tfc96 => Ok(96),
        ie::MaxNumberOfTFCDL::tfc128 => Ok(128),
        ie::MaxNumberOfTFCDL::tfc256 => Ok(256),
        ie::MaxNumberOfTFCDL::tfc512 => Ok(512),
        ie::MaxNumberOfTFCDL::tfc1024 => Ok(1024),
    }
}

pub(super) fn combinations_ul(value: ie::MaxNumberOfTFCUL) -> Result<usize, Error> {
    match value {
        ie::MaxNumberOfTFCUL::dummy1 => Err(Error::InvalidValue("MaxNumberOfTFCUL")),
        ie::MaxNumberOfTFCUL::dummy2 => Err(Error::InvalidValue("MaxNumberOfTFCUL")),
        ie::MaxNumberOfTFCUL::tfc16 => Ok(16),
        ie::MaxNumberOfTFCUL::tfc32 => Ok(32),
        ie::MaxNumberOfTFCUL::tfc48 => Ok(48),
        ie::MaxNumberOfTFCUL::tfc64 => Ok(64),
        ie::MaxNumberOfTFCUL::tfc96 => Ok(96),
        ie::MaxNumberOfTFCUL::tfc128 => Ok(128),
        ie::MaxNumberOfTFCUL::tfc256 => Ok(256),
        ie::MaxNumberOfTFCUL::tfc512 => Ok(512),
        ie::MaxNumberOfTFCUL::tfc1024 => Ok(1024),
    }
}

pub(super) fn channels_dl(value: ie::MaxSimultaneousTransChsDL) -> Result<usize, Error> {
    match value {
        ie::MaxSimultaneousTransChsDL::e4 => Ok(4),
        ie::MaxSimultaneousTransChsDL::e8 => Ok(8),
        ie::MaxSimultaneousTransChsDL::e16 => Ok(16),
        ie::MaxSimultaneousTransChsDL::e32 => Ok(32),
    }
}

pub(super) fn channels_ul(value: ie::MaxSimultaneousTransChsUL) -> Result<usize, Error> {
    match value {
        ie::MaxSimultaneousTransChsUL::dummy => {
            Err(Error::InvalidValue("MaxSimultaneousTransChsUL"))
        }
        ie::MaxSimultaneousTransChsUL::e4 => Ok(4),
        ie::MaxSimultaneousTransChsUL::e8 => Ok(8),
        ie::MaxSimultaneousTransChsUL::e16 => Ok(16),
        ie::MaxSimultaneousTransChsUL::e32 => Ok(32),
    }
}

pub(super) fn blocks_dl(value: ie::MaxTransportBlocksDL) -> Result<usize, Error> {
    match value {
        ie::MaxTransportBlocksDL::tb4 => Ok(4),
        ie::MaxTransportBlocksDL::tb8 => Ok(8),
        ie::MaxTransportBlocksDL::tb16 => Ok(16),
        ie::MaxTransportBlocksDL::tb32 => Ok(32),
        ie::MaxTransportBlocksDL::tb48 => Ok(48),
        ie::MaxTransportBlocksDL::tb64 => Ok(64),
        ie::MaxTransportBlocksDL::tb96 => Ok(96),
        ie::MaxTransportBlocksDL::tb128 => Ok(128),
        ie::MaxTransportBlocksDL::tb256 => Ok(256),
        ie::MaxTransportBlocksDL::tb512 => Ok(512),
    }
}

pub(super) fn blocks_ul(value: ie::MaxTransportBlocksUL) -> Result<usize, Error> {
    match value {
        ie::MaxTransportBlocksUL::dummy => Err(Error::InvalidValue("MaxTransportBlocksUL")),
        ie::MaxTransportBlocksUL::tb4 => Ok(4),
        ie::MaxTransportBlocksUL::tb8 => Ok(8),
        ie::MaxTransportBlocksUL::tb16 => Ok(16),
        ie::MaxTransportBlocksUL::tb32 => Ok(32),
        ie::MaxTransportBlocksUL::tb48 => Ok(48),
        ie::MaxTransportBlocksUL::tb64 => Ok(64),
        ie::MaxTransportBlocksUL::tb96 => Ok(96),
        ie::MaxTransportBlocksUL::tb128 => Ok(128),
        ie::MaxTransportBlocksUL::tb256 => Ok(256),
        ie::MaxTransportBlocksUL::tb512 => Ok(512),
    }
}

pub(super) fn entities(value: ie::MaximumAMEntityNumberRLCCap) -> Result<usize, Error> {
    match value {
        ie::MaximumAMEntityNumberRLCCap::dummy => {
            Err(Error::InvalidValue("MaximumAMEntityNumberRLCCap"))
        }
        ie::MaximumAMEntityNumberRLCCap::am4 => Ok(4),
        ie::MaximumAMEntityNumberRLCCap::am5 => Ok(5),
        ie::MaximumAMEntityNumberRLCCap::am6 => Ok(6),
        ie::MaximumAMEntityNumberRLCCap::am8 => Ok(8),
        ie::MaximumAMEntityNumberRLCCap::am16 => Ok(16),
        ie::MaximumAMEntityNumberRLCCap::am30 => Ok(30),
    }
}

pub(super) fn window(value: ie::MaximumRLCWindowSize) -> Result<usize, Error> {
    match value {
        ie::MaximumRLCWindowSize::mws2047 => Ok(2047),
        ie::MaximumRLCWindowSize::mws4095 => Ok(4095),
    }
}

pub(super) fn buffer(value: ie::TotalRLCAMBufferSize) -> Result<usize, Error> {
    match value {
        ie::TotalRLCAMBufferSize::dummy => Err(Error::InvalidValue("TotalRLCAMBufferSize")),
        ie::TotalRLCAMBufferSize::kb10 => Ok(10),
        ie::TotalRLCAMBufferSize::kb50 => Ok(50),
        ie::TotalRLCAMBufferSize::kb100 => Ok(100),
        ie::TotalRLCAMBufferSize::kb150 => Ok(150),
        ie::TotalRLCAMBufferSize::kb500 => Ok(500),
        ie::TotalRLCAMBufferSize::kb1000 => Ok(1000),
        ie::TotalRLCAMBufferSize::spare => Err(Error::InvalidValue("TotalRLCAMBufferSize")),
    }
}

pub(super) fn buffer_ext(value: ie::TotalRLCAMBufferSizeR5Ext) -> Result<usize, Error> {
    match value {
        ie::TotalRLCAMBufferSizeR5Ext::kb200 => Ok(200),
        ie::TotalRLCAMBufferSizeR5Ext::kb300 => Ok(300),
        ie::TotalRLCAMBufferSizeR5Ext::kb400 => Ok(400),
        ie::TotalRLCAMBufferSizeR5Ext::kb750 => Ok(750),
    }
}

pub(super) fn tx_window(value: ie::TransmissionWindowSize) -> Result<usize, Error> {
    match value {
        ie::TransmissionWindowSize::tw1 => Ok(1),
        ie::TransmissionWindowSize::tw8 => Ok(8),
        ie::TransmissionWindowSize::tw16 => Ok(16),
        ie::TransmissionWindowSize::tw32 => Ok(32),
        ie::TransmissionWindowSize::tw64 => Ok(64),
        ie::TransmissionWindowSize::tw128 => Ok(128),
        ie::TransmissionWindowSize::tw256 => Ok(256),
        ie::TransmissionWindowSize::tw512 => Ok(512),
        ie::TransmissionWindowSize::tw768 => Ok(768),
        ie::TransmissionWindowSize::tw1024 => Ok(1024),
        ie::TransmissionWindowSize::tw1536 => Ok(1536),
        ie::TransmissionWindowSize::tw2047 => Ok(2047),
        ie::TransmissionWindowSize::tw2560 => Ok(2560),
        ie::TransmissionWindowSize::tw3072 => Ok(3072),
        ie::TransmissionWindowSize::tw3584 => Ok(3584),
        ie::TransmissionWindowSize::tw4095 => Ok(4095),
    }
}

pub(super) fn rx_window(value: ie::ReceivingWindowSize) -> Result<usize, Error> {
    match value {
        ie::ReceivingWindowSize::rw1 => Ok(1),
        ie::ReceivingWindowSize::rw8 => Ok(8),
        ie::ReceivingWindowSize::rw16 => Ok(16),
        ie::ReceivingWindowSize::rw32 => Ok(32),
        ie::ReceivingWindowSize::rw64 => Ok(64),
        ie::ReceivingWindowSize::rw128 => Ok(128),
        ie::ReceivingWindowSize::rw256 => Ok(256),
        ie::ReceivingWindowSize::rw512 => Ok(512),
        ie::ReceivingWindowSize::rw768 => Ok(768),
        ie::ReceivingWindowSize::rw1024 => Ok(1024),
        ie::ReceivingWindowSize::rw1536 => Ok(1536),
        ie::ReceivingWindowSize::rw2047 => Ok(2047),
        ie::ReceivingWindowSize::rw2560 => Ok(2560),
        ie::ReceivingWindowSize::rw3072 => Ok(3072),
        ie::ReceivingWindowSize::rw3584 => Ok(3584),
        ie::ReceivingWindowSize::rw4095 => Ok(4095),
    }
}

pub(super) fn turbo(value: &ie::TurboSupport) -> Result<Option<usize>, Error> {
    match value {
        ie::TurboSupport::notSupported(()) => Ok(None),
        ie::TurboSupport::supported(value) => bits(*value).map(Some),
    }
}
