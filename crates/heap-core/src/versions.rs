use crate::{analysis::HeapSnapshot, heap::Gc};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;

const SUPPORTED_MAJOR_MINOR: &str = "10.0";
const SUPPORTED_PATCHES: RangeInclusive<u32> = 1..=13;

fn is_supported(version: &str) -> bool {
    let Some((major_minor, patch)) = version.rsplit_once('.') else {
        return false;
    };
    major_minor == SUPPORTED_MAJOR_MINOR
        && (patch.len() == 1 || !patch.starts_with('0'))
        && patch.bytes().all(|byte| byte.is_ascii_digit())
        && patch
            .parse::<u32>()
            .is_ok_and(|patch| SUPPORTED_PATCHES.contains(&patch))
}

fn supported_range() -> String {
    format!(
        "{SUPPORTED_MAJOR_MINOR}.{}–{SUPPORTED_MAJOR_MINOR}.{}",
        SUPPORTED_PATCHES.start(),
        SUPPORTED_PATCHES.end()
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Versions {
    pub runtime: Option<String>,
    pub runtime_build_id: Option<String>,
    pub net_core: Option<String>,
    pub asp_net_core: Option<String>,
    pub warnings: Vec<String>,
}
impl HeapSnapshot {
    pub fn versions(&self) -> Versions {
        let text = self
            .runtime
            .as_ref()
            .and_then(|r| r.version.as_ref())
            .map(|s| s.replace("@(#)Version ", ""));
        let runtime = text.as_deref().and_then(product_version_file);
        let runtime_build_id = self.runtime.as_ref().and_then(|r| r.build_id.clone());
        let net_core = self
            .names
            .informational_version("System.Private.CoreLib")
            .as_deref()
            .and_then(product_version_info);
        let asp_net_core = self
            .names
            .informational_version("Microsoft.AspNetCore.Server.Kestrel.Core")
            .as_deref()
            .and_then(product_version_info);
        let mut warnings = Vec::new();
        let note = "Results may be incomplete or incorrect.";
        let supported = supported_range();
        if self.heap.kind.gc != Gc::Satori {
            if let Some(id) = &runtime_build_id
                && runtime.is_none()
            {
                warnings.push(format!(
                    "the runtime is a development build{} ({}, build ID {id}) \
                    that dotheap was not tested with. {note}",
                    net_core
                        .as_ref()
                        .map(|v| format!(" of {v}"))
                        .unwrap_or_default(),
                    text.as_deref().unwrap_or("no version")
                ));
            } else if let Some(v) = &runtime
                && !is_supported(v)
            {
                warnings.push(format!(
                    ".NET runtime {v} is outside the supported range ({supported}). {note}"
                ));
            }
        }
        if let Some(v) = &net_core {
            if !is_supported(v) {
                warnings.push(format!(
                    ".NET libraries (System.Private.CoreLib) are {v}, \
                    outside the supported range ({supported}). {note}"
                ));
            }
        } else if self.heap.kind.gc != Gc::None {
            warnings.push(format!(
                "the version of the .NET libraries is not known (there is no metadata for \
                System.Private.CoreLib). {note}"
            ));
        }
        if let Some(v) = &asp_net_core
            && !is_supported(v)
        {
            warnings.push(format!(
                "ASP.NET Core (Kestrel) {v} is outside the supported range ({supported}). \
                Connection and request results may be incomplete or incorrect."
            ));
        }
        Versions {
            runtime,
            runtime_build_id,
            net_core,
            asp_net_core,
            warnings,
        }
    }
}
pub fn product_version_file(value: &str) -> Option<String> {
    let parts: Vec<u32> = value
        .split_whitespace()
        .next()?
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if parts.len() < 3 || parts[0] >= 42 {
        return None;
    }
    Some(format!("{}.{}.{}", parts[0], parts[1], parts[2] / 100))
}
pub fn product_version_info(value: &str) -> Option<String> {
    let version = value.split('+').next()?;
    if version.split('.').next()?.parse::<u32>().ok()? >= 42 {
        return None;
    }
    Some(
        version
            .strip_suffix("-dev")
            .unwrap_or_else(|| version.split("-servicing.").next().unwrap_or(version))
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_range_includes_every_patch() {
        for patch in 1..=13 {
            assert!(is_supported(&format!("10.0.{patch}")));
        }
        assert_eq!(supported_range(), "10.0.1–10.0.13");
    }

    #[test]
    fn unsupported_and_prerelease_versions_are_not_included() {
        for version in [
            "10.0.0",
            "10.0.14",
            "10.0.100",
            "10.1.1",
            "9.0.13",
            "11.0.1",
            "10.0.13-preview.1",
            "10.0.13-rc.1",
            "10.0.13.0",
            "10.0.+13",
            "10.0.013",
            "10.0.4294967296",
            "10.0.",
            "10.0",
            "",
        ] {
            assert!(!is_supported(version), "{version}");
        }
    }

    #[test]
    fn supported_versions_keep_runtime_and_library_normalization() {
        assert_eq!(
            product_version_file("10.0.1325.12345"),
            Some("10.0.13".into())
        );
        for version in [
            "10.0.13+commit",
            "10.0.13-servicing.123+commit",
            "10.0.12-dev",
        ] {
            assert!(is_supported(&product_version_info(version).unwrap()));
        }
        assert!(!is_supported(
            &product_version_info("10.0.13-rc.1+commit").unwrap()
        ));
        assert_eq!(product_version_file("42.42.42.42424"), None);
    }
}
