use crate::services::Route;
use anyhow::{ensure, Result};
use lianli_shared::installation::{
    CheckState, FindingSeverity, InstallationFinding, InstallationGuide,
};
use lianli_shared::services::{ServiceProbe, ServiceReport};

pub fn guidance(uid: u32) -> String {
    format!("On the host, run sudo loginctl enable-linger {uid}, then Recheck. Lingering allows this account's user services to run before login and after logout. Lian Li Linux does not change it automatically.")
}

fn parse(value: &str) -> Result<bool> {
    match value.trim() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => anyhow::bail!("The host did not report a valid lingering setting"),
    }
}

fn inspect(route: &Route, uid: u32) -> Result<bool> {
    let output = route.output(
        "/usr/bin/loginctl",
        &[
            "--no-pager",
            "--no-ask-password",
            "show-user",
            &uid.to_string(),
            "--property=Linger",
            "--value",
        ],
    )?;
    ensure!(
        output.status.success(),
        "Cannot check host user lingering: {}",
        output.stderr.trim()
    );
    parse(&output.stdout)
}

pub(crate) fn require(route: &Route, uid: u32) -> Result<()> {
    let enabled =
        inspect(route, uid).map_err(|error| anyhow::anyhow!("{error:#}. {}", guidance(uid)))?;
    ensure!(
        enabled,
        "Distrobox system mode requires user lingering. {}",
        guidance(uid)
    );
    Ok(())
}

pub fn finding(report: &ServiceReport) -> Option<InstallationFinding> {
    let ServiceProbe::Known { value } = &report.system else {
        return None;
    };
    value.distrobox_name.as_ref()?;
    let mut owner_uid = None;
    let result = Route::detect(&report.context).and_then(|route| {
        let uid = crate::distrobox_service::system_owner_uid(&route)?;
        owner_uid = Some(uid);
        inspect(&route, uid)
    });
    let (state, evidence) = match result {
        Ok(true) => (CheckState::Passed, "Host user lingering is enabled.".into()),
        Ok(false) => (
            CheckState::Failed,
            "Without lingering, the boxed system daemon stops after logout.".into(),
        ),
        Err(error) => (CheckState::Unavailable, format!("{error:#}")),
    };
    Some(InstallationFinding {
        code: "services.distrobox_lingering".into(),
        state,
        severity: FindingSeverity::Warning,
        feature: "Services".into(),
        context: owner_uid.map_or_else(
            || "Host account".into(),
            |uid| format!("Host account {uid}"),
        ),
        title: "Distrobox system startup".into(),
        evidence,
        remediation: owner_uid.map_or_else(
            || {
                "Verify the host system unit's account, then follow the Distrobox lingering guide."
                    .into()
            },
            guidance,
        ),
        guide: InstallationGuide::Distrobox,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn accepts_only_explicit_host_lingering_state() {
        assert!(super::parse("yes\n").unwrap());
        assert!(!super::parse("no\n").unwrap());
        for value in ["", "1", "true", "Linger=yes", "yes\nno", "unknown"] {
            assert!(super::parse(value).is_err(), "{value:?}");
        }
    }
}
