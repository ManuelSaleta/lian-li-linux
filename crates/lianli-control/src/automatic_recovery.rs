use crate::account::Account;
use crate::reservation::ServiceOperationLock;
use crate::switch_journal::{Journal, Record};
use anyhow::{ensure, Result};
use lianli_shared::installation::InstallationContext;
use lianli_shared::services::ServiceScope;
use std::path::Path;

fn caller_uid(record: &Record) -> u32 {
    if record.intent.source.scope == ServiceScope::User {
        record.intent.source.uid
    } else {
        record.intent.destination.uid
    }
}

pub fn run() -> Result<String> {
    ensure!(
        unsafe { libc::geteuid() } == 0
            && InstallationContext::detect() == InstallationContext::Native,
        "Automatic recovery requires the installed native system service"
    );
    let (caller, id) =
        {
            let operation = ServiceOperationLock::acquire(&InstallationContext::Native)?;
            let Some(journal) = Journal::load(&operation)? else {
                return Ok("No service switch needs recovery.".into());
            };
            let caller = Account::user(caller_uid(journal.record()))?;
            ensure!(caller.name == journal.record().intent.caller_name,
            "The pending switch account changed. Preserve its journal for administrator recovery");
            (caller, journal.record().intent.id.clone())
        };
    crate::switch_job::recover_automatically(caller, &id)
}

pub fn trigger() -> Result<()> {
    if InstallationContext::detect() != InstallationContext::Native
        || !Path::new("/usr/lib/systemd/system/lianli-control-recovery.service").is_file()
    {
        return Ok(());
    }
    crate::recovery_trigger::trigger()
}
