//! Quota access for local backup management in-process or in a user worker.
use crate::worker::session::QuotaClient;
use desk_file_recovery::{
    EpochCoordinator, LockedVault, Policy, Record,
    quota::{LockedDeviceQuota, QuotaKey, UsageReport},
};
use std::io;

pub(crate) trait LocalQuota: EpochCoordinator {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)>;
    /// Two-part settlement; returns whether the absolute usage was applied.
    fn settle(
        &mut self,
        os_user: &str,
        clear: Option<&Record>,
        usage: &UsageReport,
    ) -> io::Result<bool>;
}

/// Settle the vault's current absolute usage and acknowledge it when applied.
/// Runs while the vault lock is held, like every quota request.
pub(crate) fn settle_usage(
    vault: &mut LockedVault,
    quota: &mut impl LocalQuota,
    os_user: &str,
) -> io::Result<()> {
    let usage = vault.usage_report();
    if quota.settle(os_user, None, &usage)? {
        vault.acknowledge_settlement(&usage)?;
    }
    Ok(())
}

impl LocalQuota for LockedDeviceQuota {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)> {
        Ok((self.policy(), self.used_bytes(), self.reserved_bytes()))
    }

    fn settle(
        &mut self,
        os_user: &str,
        clear: Option<&Record>,
        usage: &UsageReport,
    ) -> io::Result<bool> {
        let key = clear.map(QuotaKey::for_record).transpose()?;
        LockedDeviceQuota::settle(self, os_user, key.as_ref(), usage)
    }
}

impl LocalQuota for QuotaClient {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)> {
        self.policy()
    }

    fn settle(
        &mut self,
        os_user: &str,
        clear: Option<&Record>,
        usage: &UsageReport,
    ) -> io::Result<bool> {
        if super::platform_user::current()? != os_user {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup quota user changed",
            ));
        }
        QuotaClient::settle(self, clear, usage)
    }
}
