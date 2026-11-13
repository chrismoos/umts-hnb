// SPDX-License-Identifier: AGPL-3.0-or-later

use std::io;

/// Locks all current and future process memory so realtime threads never
/// page-fault. Needs an unlimited memlock limit for the process.
#[cfg(target_os = "linux")]
pub fn lock_all() -> io::Result<()> {
    use rustix::mm::MlockAllFlags;
    rustix::mm::mlockall(MlockAllFlags::CURRENT | MlockAllFlags::FUTURE).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
pub fn lock_all() -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn lock_all_succeeds_or_reports_the_limit() {
        if let Err(error) = lock_all() {
            assert!(matches!(
                error.raw_os_error(),
                Some(libc::ENOMEM | libc::EPERM)
            ));
        }
    }
}
