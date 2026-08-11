use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::manifest::SourceTrace;

use crate::frozen::FrozenValue;
use crate::{Result, WombatError};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingSystemName {
    Macos,
    Linux,
}

impl OperatingSystemName {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "macos" => Ok(Self::Macos),
            "linux" => Ok(Self::Linux),
            _ => Err(WombatError::configuration(format!(
                "unsupported target operating system `{value}`; expected macos or linux"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Aarch64,
    X86_64,
}

impl Architecture {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            "x86_64" | "amd64" => Ok(Self::X86_64),
            _ => Err(WombatError::configuration(format!(
                "unsupported target architecture `{value}`; expected aarch64 or x86_64"
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LooseVersion {
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub major: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minor: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<u64>,
}

impl LooseVersion {
    pub fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let mut parts = raw.split('.').map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u64>()
                .ok()
        });
        Self {
            major: parts.next().flatten(),
            minor: parts.next().flatten(),
            patch: parts.next().flatten(),
            raw,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kernel {
    pub name: String,
    pub release: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub id: String,
    pub id_like: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<LooseVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatingSystem {
    pub name: OperatingSystemName,
    pub family: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<LooseVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<Kernel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distribution: Option<Distribution>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPlatform {
    pub os: OperatingSystem,
    pub arch: Architecture,
}

impl TargetPlatform {
    pub fn minimal(os: OperatingSystemName, arch: Architecture) -> Self {
        Self {
            os: OperatingSystem {
                name: os,
                family: "unix".to_string(),
                version: None,
                kernel: None,
                distribution: None,
            },
            arch,
        }
    }

    pub fn parse_compact(value: &str) -> Result<Self> {
        let (os, arch) = value.split_once('/').ok_or_else(|| {
            WombatError::configuration(format!(
                "invalid target `{value}`; expected <os>/<arch>, such as macos/aarch64"
            ))
        })?;
        if arch.contains('/') {
            return Err(WombatError::configuration(format!(
                "invalid target `{value}`; expected exactly <os>/<arch>"
            )));
        }
        Ok(Self::minimal(
            OperatingSystemName::parse(os)?,
            Architecture::parse(arch)?,
        ))
    }

    pub fn compact(&self) -> String {
        format!("{}/{}", self.os.name.as_str(), self.arch.as_str())
    }

    pub fn to_frozen(&self) -> FrozenValue {
        FrozenValue::Map(
            [
                (
                    "arch".to_string(),
                    FrozenValue::String(self.arch.as_str().to_string()),
                ),
                ("os".to_string(), os_to_frozen(&self.os)),
            ]
            .into(),
        )
    }

    pub fn from_frozen(value: &FrozenValue) -> Result<Self> {
        let FrozenValue::Map(map) = value else {
            return Err(WombatError::configuration(
                "target must be a string like linux/x86_64 or a table",
            ));
        };
        reject_unknown(map, &["os", "arch"], "target")?;
        let arch = string_field(map, "arch", "target")?;
        let os_value = map
            .get("os")
            .ok_or_else(|| WombatError::configuration("target requires an `os` field"))?;
        let os = match os_value {
            FrozenValue::String(name) => OperatingSystem {
                name: OperatingSystemName::parse(name)?,
                family: "unix".to_string(),
                version: None,
                kernel: None,
                distribution: None,
            },
            FrozenValue::Map(os) => parse_os(os)?,
            _ => {
                return Err(WombatError::configuration(
                    "target `os` must be a normalized string or table",
                ));
            }
        };
        Ok(Self {
            os,
            arch: Architecture::parse(arch)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetOrigin {
    HostDefault,
    RootOverride,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedTarget {
    #[serde(flatten)]
    pub platform: TargetPlatform,
    pub origin: TargetOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_at: Option<SourceTrace>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostContext {
    pub platform: TargetPlatform,
    pub hostname: Option<String>,
    pub username: Option<String>,
    pub home: Option<PathBuf>,
}

impl HostContext {
    pub fn observe() -> Result<Self> {
        let os_name = OperatingSystemName::parse(env::consts::OS)?;
        let arch = Architecture::parse(env::consts::ARCH)?;
        let kernel = observe_kernel();
        let (version, distribution) = match os_name {
            OperatingSystemName::Macos => (
                command_line("sw_vers", &["-productVersion"]).map(LooseVersion::parse),
                None,
            ),
            OperatingSystemName::Linux => {
                (None, read_linux_distribution(Path::new("/etc/os-release"))?)
            }
        };
        Ok(Self {
            platform: TargetPlatform {
                os: OperatingSystem {
                    name: os_name,
                    family: "unix".to_string(),
                    version,
                    kernel,
                    distribution,
                },
                arch,
            },
            hostname: observe_hostname(),
            username: clean_value(env::var("USER").ok()),
            home: clean_value(env::var("HOME").ok()).map(PathBuf::from),
        })
    }

    pub fn fixture(platform: TargetPlatform) -> Self {
        Self {
            platform,
            hostname: Some("fixture-host".to_string()),
            username: Some("fixture-user".to_string()),
            home: Some(PathBuf::from("/fixture/home")),
        }
    }

    pub fn resolved_target(&self) -> ResolvedTarget {
        ResolvedTarget {
            platform: TargetPlatform::minimal(self.platform.os.name, self.platform.arch),
            origin: TargetOrigin::HostDefault,
            declared_at: None,
        }
    }

    pub fn to_frozen(&self) -> FrozenValue {
        let mut map = BTreeMap::from([
            (
                "arch".to_string(),
                FrozenValue::String(self.platform.arch.as_str().to_string()),
            ),
            ("os".to_string(), os_to_frozen(&self.platform.os)),
        ]);
        if let Some(value) = &self.hostname {
            map.insert("hostname".to_string(), FrozenValue::String(value.clone()));
        }
        if let Some(value) = &self.username {
            map.insert("username".to_string(), FrozenValue::String(value.clone()));
        }
        if let Some(value) = &self.home {
            map.insert(
                "home".to_string(),
                FrozenValue::String(value.to_string_lossy().into_owned()),
            );
        }
        FrozenValue::Map(map)
    }
}

fn os_to_frozen(os: &OperatingSystem) -> FrozenValue {
    let mut map = BTreeMap::from([
        ("family".to_string(), FrozenValue::String(os.family.clone())),
        (
            "name".to_string(),
            FrozenValue::String(os.name.as_str().to_string()),
        ),
    ]);
    if let Some(value) = &os.version {
        map.insert("version".to_string(), version_to_frozen(value));
    }
    if let Some(kernel) = &os.kernel {
        map.insert(
            "kernel".to_string(),
            FrozenValue::Map(
                [
                    ("name".to_string(), FrozenValue::String(kernel.name.clone())),
                    (
                        "release".to_string(),
                        FrozenValue::String(kernel.release.clone()),
                    ),
                ]
                .into(),
            ),
        );
    }
    if let Some(distribution) = &os.distribution {
        let mut value = BTreeMap::from([
            (
                "id".to_string(),
                FrozenValue::String(distribution.id.clone()),
            ),
            (
                "id_like".to_string(),
                FrozenValue::Array(
                    distribution
                        .id_like
                        .iter()
                        .cloned()
                        .map(FrozenValue::String)
                        .collect(),
                ),
            ),
        ]);
        if let Some(version) = &distribution.version {
            value.insert("version".to_string(), version_to_frozen(version));
        }
        if let Some(pretty_name) = &distribution.pretty_name {
            value.insert(
                "pretty_name".to_string(),
                FrozenValue::String(pretty_name.clone()),
            );
        }
        map.insert("distribution".to_string(), FrozenValue::Map(value));
    }
    FrozenValue::Map(map)
}

fn version_to_frozen(version: &LooseVersion) -> FrozenValue {
    let mut map = BTreeMap::from([("raw".to_string(), FrozenValue::String(version.raw.clone()))]);
    for (name, value) in [
        ("major", version.major),
        ("minor", version.minor),
        ("patch", version.patch),
    ] {
        if let Some(value) = value.and_then(|value| i64::try_from(value).ok()) {
            map.insert(name.to_string(), FrozenValue::Integer(value));
        }
    }
    FrozenValue::Map(map)
}

fn parse_os(map: &BTreeMap<String, FrozenValue>) -> Result<OperatingSystem> {
    reject_unknown(
        map,
        &["name", "family", "version", "kernel", "distribution"],
        "target os",
    )?;
    let name = OperatingSystemName::parse(string_field(map, "name", "target os")?)?;
    let family = optional_string(map, "family", "target os")?.unwrap_or_else(|| "unix".to_string());
    if family != "unix" {
        return Err(WombatError::configuration(
            "target os family must be `unix`",
        ));
    }
    let version = map.get("version").map(parse_version).transpose()?;
    let kernel = map.get("kernel").map(parse_kernel).transpose()?;
    let distribution = map
        .get("distribution")
        .map(parse_distribution)
        .transpose()?;
    if name != OperatingSystemName::Linux && distribution.is_some() {
        return Err(WombatError::configuration(
            "target os distribution is supported only for linux",
        ));
    }
    Ok(OperatingSystem {
        name,
        family,
        version,
        kernel,
        distribution,
    })
}

fn parse_version(value: &FrozenValue) -> Result<LooseVersion> {
    let FrozenValue::Map(map) = value else {
        return Err(WombatError::configuration("version must be a table"));
    };
    reject_unknown(map, &["raw", "major", "minor", "patch"], "version")?;
    let raw = string_field(map, "raw", "version")?.to_string();
    let mut version = LooseVersion::parse(raw);
    for (name, slot) in [
        ("major", &mut version.major),
        ("minor", &mut version.minor),
        ("patch", &mut version.patch),
    ] {
        if let Some(value) = map.get(name) {
            let FrozenValue::Integer(value) = value else {
                return Err(WombatError::configuration(format!(
                    "version `{name}` must be an integer"
                )));
            };
            *slot = Some(u64::try_from(*value).map_err(|_| {
                WombatError::configuration(format!("version `{name}` must be non-negative"))
            })?);
        }
    }
    Ok(version)
}

fn parse_kernel(value: &FrozenValue) -> Result<Kernel> {
    let FrozenValue::Map(map) = value else {
        return Err(WombatError::configuration("kernel must be a table"));
    };
    reject_unknown(map, &["name", "release"], "kernel")?;
    Ok(Kernel {
        name: string_field(map, "name", "kernel")?.to_string(),
        release: string_field(map, "release", "kernel")?.to_string(),
    })
}

fn parse_distribution(value: &FrozenValue) -> Result<Distribution> {
    let FrozenValue::Map(map) = value else {
        return Err(WombatError::configuration("distribution must be a table"));
    };
    reject_unknown(
        map,
        &["id", "id_like", "version", "pretty_name"],
        "distribution",
    )?;
    let id = normalize_identifier(string_field(map, "id", "distribution")?)?;
    let id_like = match map.get("id_like") {
        None => Vec::new(),
        Some(FrozenValue::Array(values)) => {
            let mut values = values
                .iter()
                .map(|value| match value {
                    FrozenValue::String(value) => normalize_identifier(value),
                    _ => Err(WombatError::configuration(
                        "distribution `id_like` must contain strings",
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            values.sort();
            values.dedup();
            values
        }
        Some(_) => {
            return Err(WombatError::configuration(
                "distribution `id_like` must be an array",
            ));
        }
    };
    Ok(Distribution {
        id,
        id_like,
        version: map.get("version").map(parse_version).transpose()?,
        pretty_name: optional_string(map, "pretty_name", "distribution")?,
    })
}

fn reject_unknown(
    map: &BTreeMap<String, FrozenValue>,
    allowed: &[&str],
    subject: &str,
) -> Result<()> {
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(WombatError::configuration(format!(
            "{subject} does not support field `{key}`"
        )));
    }
    Ok(())
}

fn string_field<'a>(
    map: &'a BTreeMap<String, FrozenValue>,
    name: &str,
    subject: &str,
) -> Result<&'a str> {
    match map.get(name) {
        Some(FrozenValue::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(WombatError::configuration(format!(
            "{subject} requires a non-empty string `{name}`"
        ))),
    }
}

fn optional_string(
    map: &BTreeMap<String, FrozenValue>,
    name: &str,
    subject: &str,
) -> Result<Option<String>> {
    match map.get(name) {
        None | Some(FrozenValue::Null) => Ok(None),
        Some(FrozenValue::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        _ => Err(WombatError::configuration(format!(
            "{subject} `{name}` must be a non-empty string"
        ))),
    }
}

fn command_line(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())??
        .trim()
        .to_string()
        .into()
}

fn observe_kernel() -> Option<Kernel> {
    Some(Kernel {
        name: command_line("uname", &["-s"])?.to_ascii_lowercase(),
        release: command_line("uname", &["-r"])?,
    })
}

fn observe_hostname() -> Option<String> {
    if cfg!(target_os = "linux") {
        clean_value(fs::read_to_string("/proc/sys/kernel/hostname").ok())
    } else {
        clean_value(command_line("hostname", &[]))
    }
}

fn clean_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value.len() <= 4096 && !value.contains('\0'))
}

fn read_linux_distribution(path: &Path) -> Result<Option<Distribution>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WombatError::io(path, error)),
    };
    parse_os_release(&contents).map(Some)
}

fn parse_os_release(contents: &str) -> Result<Distribution> {
    let mut fields = BTreeMap::new();
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, raw) = line.split_once('=').ok_or_else(|| {
            WombatError::configuration(format!("invalid os-release line {}", line_number + 1))
        })?;
        if !key
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        {
            return Err(WombatError::configuration(format!(
                "invalid os-release key `{key}`"
            )));
        }
        let value = unquote_os_release(raw).ok_or_else(|| {
            WombatError::configuration(format!("invalid os-release value for `{key}`"))
        })?;
        if value.len() > 4096 || value.contains('\0') {
            return Err(WombatError::configuration(format!(
                "invalid os-release value for `{key}`"
            )));
        }
        fields.insert(key.to_string(), value);
    }
    let id = fields
        .get("ID")
        .ok_or_else(|| WombatError::configuration("os-release is missing ID"))?;
    let mut id_like = fields.get("ID_LIKE").map_or_else(Vec::new, |value| {
        value.split_ascii_whitespace().map(str::to_string).collect()
    });
    id_like = id_like
        .into_iter()
        .map(|value| normalize_identifier(&value))
        .collect::<Result<Vec<_>>>()?;
    id_like.sort();
    id_like.dedup();
    Ok(Distribution {
        id: normalize_identifier(id)?,
        id_like,
        version: fields.get("VERSION_ID").cloned().map(LooseVersion::parse),
        pretty_name: fields.get("PRETTY_NAME").cloned(),
    })
}

fn unquote_os_release(raw: &str) -> Option<String> {
    if let Some(value) = raw
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return (!value.contains('\'')).then(|| value.to_string());
    }
    if let Some(value) = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        let mut output = String::new();
        let mut escaped = false;
        for character in value.chars() {
            if escaped {
                if !matches!(character, '"' | '\\' | '$' | '`') {
                    return None;
                }
                output.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                output.push(character);
            }
        }
        (!escaped).then_some(output)
    } else if raw
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        Some(raw.to_string())
    } else {
        None
    }
}

fn normalize_identifier(value: &str) -> Result<String> {
    let value = value.to_ascii_lowercase();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(WombatError::configuration(format!(
            "invalid normalized system identifier `{value}`"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{
        Architecture, LooseVersion, OperatingSystemName, TargetPlatform, parse_os_release,
    };

    #[test]
    fn normalizes_target_aliases_and_compact_identity() {
        let target = TargetPlatform::parse_compact("macos/arm64").unwrap();
        assert_eq!(target.os.name, OperatingSystemName::Macos);
        assert_eq!(target.arch, Architecture::Aarch64);
        assert_eq!(target.compact(), "macos/aarch64");
    }

    #[test]
    fn parses_loose_versions_without_semver_claims() {
        let version = LooseVersion::parse("24.04-LTS");
        assert_eq!(version.major, Some(24));
        assert_eq!(version.minor, Some(4));
        assert_eq!(version.patch, None);
    }

    #[test]
    fn parses_and_normalizes_os_release() {
        let distribution = parse_os_release(
            "ID=fedora\nID_LIKE=\"rhel fedora\"\nVERSION_ID=42\nPRETTY_NAME='Fedora Linux 42'\n",
        )
        .unwrap();
        assert_eq!(distribution.id, "fedora");
        assert_eq!(distribution.id_like, ["fedora", "rhel"]);
        assert_eq!(distribution.version.unwrap().major, Some(42));
        assert_eq!(distribution.pretty_name.as_deref(), Some("Fedora Linux 42"));
    }
}
