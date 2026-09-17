//! Side-effect-free, machine-readable identity for executable artifacts.

use std::ffi::OsStr;
use std::io::Write;

use serde::Serialize;

pub const BUILD_INFO_SCHEMA: &str = "astra.build_info.v1";
pub const BUILD_TARGET: &str = env!("ASTRA_BUILD_TARGET");
pub const BUILD_PROFILE: &str = env!("ASTRA_BUILD_PROFILE");

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BuildInfo {
    pub schema: &'static str,
    pub git_sha: &'static str,
    pub git_dirty: bool,
    pub target: &'static str,
    pub profile: &'static str,
}

pub fn current() -> BuildInfo {
    BuildInfo {
        schema: BUILD_INFO_SCHEMA,
        git_sha: crate::history_work_baseline::BUILD_GIT_SHA,
        git_dirty: crate::history_work_baseline::BUILD_GIT_DIRTY == "true",
        target: BUILD_TARGET,
        profile: BUILD_PROFILE,
    }
}

/// Print build identity only for the exact one-argument probe invocation.
///
/// Entrypoints call this before logging, async runtime, config, network, or DB
/// initialization. Extra arguments deliberately fall through to their normal
/// parser rather than broadening this diagnostic surface.
pub fn write_json_if_requested() -> Result<bool, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os();
    let _executable = args.next();
    if args.next().as_deref() != Some(OsStr::new("--build-info-json")) || args.next().is_some() {
        return Ok(false);
    }
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &current())?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_build_info_has_a_closed_machine_readable_schema() {
        let value = serde_json::to_value(super::current()).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(
            object
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "git_dirty",
                "git_sha",
                "profile",
                "schema",
                "target",
            ])
        );
        assert_eq!(value["schema"], super::BUILD_INFO_SCHEMA);
        assert!(value["git_dirty"].is_boolean());
        assert!(!super::BUILD_TARGET.is_empty());
        assert!(!super::BUILD_PROFILE.is_empty());
    }
}
