//! Apt package binding and managed-source implementation.

use super::*;

pub(crate) fn apt_identity(binding: &ProviderBinding) -> Result<&str> {
    let FrozenValue::Map(data) = &binding.data else {
        return Err(WombatError::configuration("Apt binding data must be a map"));
    };
    match data.get("name") {
        Some(FrozenValue::String(value)) => Ok(value),
        _ => Err(WombatError::configuration("Apt binding lacks package name")),
    }
}

pub(crate) fn check_apt(binding: &ProviderBinding, minimum: Option<&str>) -> Result<CheckItem> {
    let name = apt_identity(binding)?;
    let Some(dpkg_query) = which("dpkg-query") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "dpkg-query is not available on PATH",
        ));
    };
    let output = run_bounded(
        &dpkg_query,
        &["-W", "-f=${Status}\t${Version}", name],
        &BTreeMap::new(),
    )?;
    if output.success {
        let text = String::from_utf8_lossy(&output.stdout.bytes);
        let Some((status, observed)) = parse_dpkg_record(&text) else {
            return Ok(provider_item(
                binding,
                CheckStatus::Unavailable,
                "dpkg-query returned an unrecognized package record",
            ));
        };
        if status != "install ok installed" {
            return Ok(provider_item(
                binding,
                CheckStatus::Missing,
                &format!("dpkg status is {status}"),
            ));
        }
        if let Some(minimum) = minimum {
            let Some(dpkg) = which("dpkg") else {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Unavailable,
                    "dpkg is unavailable for Debian version comparison",
                ));
            };
            let comparison = run_bounded(
                &dpkg,
                &["--compare-versions", observed, "ge", minimum],
                &BTreeMap::new(),
            )?;
            if !comparison.success {
                return Ok(provider_item(
                    binding,
                    CheckStatus::Outdated,
                    &format!("observed {observed}; needs at least {minimum}"),
                ));
            }
        }
        return Ok(provider_item(
            binding,
            CheckStatus::Satisfied,
            &format!("package {name} {observed}"),
        ));
    }

    let Some(apt_cache) = which("apt-cache") else {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "apt-cache is not available on PATH",
        ));
    };
    let policy = run_bounded(&apt_cache, &["policy", name], &BTreeMap::new())?;
    if !policy.success {
        return Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            &format!("apt-cache policy failed: {}", output_detail(&policy)),
        ));
    }
    let policy_text = String::from_utf8_lossy(&policy.stdout.bytes);
    let candidate = policy_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Candidate:").map(str::trim));
    match candidate {
        Some(candidate) if candidate != "(none)" => Ok(provider_item(
            binding,
            CheckStatus::Missing,
            &format!("not installed; candidate {candidate}"),
        )),
        _ if !binding.prerequisites.is_empty() => Ok(provider_item(
            binding,
            CheckStatus::Missing,
            "not installed; declared Apt source will provide the candidate",
        )),
        _ => Ok(provider_item(
            binding,
            CheckStatus::Unavailable,
            "no Apt candidate is available",
        )),
    }
}

pub(crate) fn prepare_apt(operation: &ProviderPreparation, noninteractive: bool) -> Result<()> {
    if operation.identity != "update-index" {
        return Err(WombatError::configuration(format!(
            "Apt does not recognize preparation `{}`",
            operation.identity
        )));
    }
    let apt_get = require_command("apt-get", "Apt preparation")?;
    run_mutating(
        &apt_get,
        &["update"],
        &apt_environment(),
        operation.elevated,
        noninteractive,
    )
}

pub(crate) fn preflight_apt_source(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<()> {
    let source = apt_source(prerequisite)?;
    if apt_source_needs_download(context, &source)? {
        require_command("curl", "Apt source key download")?;
    }
    for command in ["install", "mv", "rm"] {
        require_command(command, "Apt source publication")?;
    }
    preflight_elevation(prerequisite.elevated)
}

pub(crate) fn preflight_apt_preparation(operation: &ProviderPreparation) -> Result<()> {
    require_command("apt-get", "Apt preparation")?;
    preflight_elevation(operation.elevated)
}

pub(crate) fn preflight_apt_requirement(requirement: &Requirement) -> Result<()> {
    let name = apt_identity(&requirement.binding)?;
    let apt_get = require_command("apt-get", "Apt preflight")?;
    let output = run_bounded(
        &apt_get,
        &["--simulate", "install", name],
        &apt_environment(),
    )?;
    if !output.success {
        return Err(WombatError::configuration(format!(
            "Apt preflight failed for `{name}`: {}",
            output_detail(&output)
        )));
    }
    preflight_elevation(requirement.binding.elevated)
}

pub(crate) fn reconcile_apt_requirement(
    requirement: &Requirement,
    noninteractive: bool,
) -> Result<()> {
    let name = apt_identity(&requirement.binding)?;
    let apt_get = require_command("apt-get", "Apt bootstrap")?;
    run_mutating(
        &apt_get,
        &["install", "--yes", name],
        &apt_environment(),
        requirement.binding.elevated,
        noninteractive,
    )
}

pub(crate) fn parse_dpkg_record(text: &str) -> Option<(&str, &str)> {
    // An uninstalled package has an empty `${Version}`, so the trailing tab is
    // the only evidence that dpkg-query returned both requested fields.
    text.trim_end_matches(['\r', '\n']).rsplit_once('\t')
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AptSource {
    pub name: String,
    pub uri: String,
    pub suite: String,
    pub components: Vec<String>,
    pub architectures: Vec<String>,
    pub key_url: String,
    pub key_format: String,
    pub key_sha256: Option<String>,
    pub replace: bool,
}

impl AptSource {
    pub(crate) fn source_relative_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "etc/apt/sources.list.d/wombat-{}.sources",
            self.name
        ))
    }

    pub(crate) fn key_relative_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "etc/apt/keyrings/wombat-{}.{}",
            self.name, self.key_format
        ))
    }

    pub(crate) fn marker(&self) -> String {
        format!("# Managed by Wombat: apt-source:{}\n", self.name)
    }

    pub(crate) fn deb822(&self) -> String {
        let mut value = format!(
            "{}Types: deb\nURIs: {}\nSuites: {}\nComponents: {}\n",
            self.marker(),
            self.uri,
            self.suite,
            self.components.join(" ")
        );
        if !self.architectures.is_empty() {
            value.push_str(&format!(
                "Architectures: {}\n",
                self.architectures.join(" ")
            ));
        }
        value.push_str(&format!(
            "Signed-By: /etc/apt/keyrings/wombat-{}.{}\n",
            self.name, self.key_format
        ));
        value
    }
}

pub(crate) fn apt_source(prerequisite: &ProviderPrerequisite) -> Result<AptSource> {
    let FrozenValue::Map(data) = &prerequisite.data else {
        return Err(WombatError::configuration(
            "Apt source prerequisite data must be a map",
        ));
    };
    let string = |name: &str| match data.get(name) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite lacks `{name}`"
        ))),
    };
    let strings = |name: &str, required: bool| match data.get(name) {
        Some(FrozenValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                FrozenValue::String(value) => Ok(value.clone()),
                _ => Err(WombatError::configuration(format!(
                    "Apt source prerequisite `{name}` must contain strings"
                ))),
            })
            .collect(),
        None if !required => Ok(Vec::new()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite lacks `{name}` array"
        ))),
    };
    let FrozenValue::Map(key) = data
        .get("key")
        .ok_or_else(|| WombatError::configuration("Apt source prerequisite lacks `key`"))?
    else {
        return Err(WombatError::configuration(
            "Apt source prerequisite key must be a map",
        ));
    };
    let key_string = |name: &str| match key.get(name) {
        Some(FrozenValue::String(value)) => Ok(value.clone()),
        _ => Err(WombatError::configuration(format!(
            "Apt source prerequisite key lacks `{name}`"
        ))),
    };
    let source = AptSource {
        name: string("name")?,
        uri: string("uri")?,
        suite: string("suite")?,
        components: strings("components", true)?,
        architectures: strings("architectures", false)?,
        key_url: key_string("url")?,
        key_format: key_string("format")?,
        key_sha256: match key.get("sha256") {
            None => None,
            Some(FrozenValue::String(value)) => Some(value.clone()),
            Some(_) => {
                return Err(WombatError::configuration(
                    "Apt source prerequisite key sha256 must be a string",
                ));
            }
        },
        replace: match data.get("replace") {
            Some(FrozenValue::Boolean(value)) => *value,
            _ => {
                return Err(WombatError::configuration(
                    "Apt source prerequisite `replace` must be boolean",
                ));
            }
        },
    };
    if prerequisite.identity != format!("source:{}", source.name) {
        return Err(WombatError::configuration(
            "Apt source prerequisite identity does not match its source name",
        ));
    }
    validate_apt_source(&source, data, key)?;
    Ok(source)
}

fn validate_apt_source(
    source: &AptSource,
    data: &BTreeMap<String, FrozenValue>,
    key: &BTreeMap<String, FrozenValue>,
) -> Result<()> {
    let expected_data = [
        "architectures",
        "components",
        "key",
        "name",
        "replace",
        "suite",
        "uri",
    ];
    if let Some(field) = data
        .keys()
        .find(|field| !expected_data.contains(&field.as_str()))
    {
        return Err(WombatError::configuration(format!(
            "Apt source prerequisite does not support `{field}`"
        )));
    }
    let expected_key = ["format", "sha256", "url"];
    if let Some(field) = key
        .keys()
        .find(|field| !expected_key.contains(&field.as_str()))
    {
        return Err(WombatError::configuration(format!(
            "Apt source prerequisite key does not support `{field}`"
        )));
    }
    if source.name.len() > 64
        || !source
            .name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
        || !source.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return Err(WombatError::configuration("Apt source name is invalid"));
    }
    validate_http_url(&source.uri, "Apt source uri")?;
    validate_http_url(&source.key_url, "Apt source key url")?;
    if source.key_sha256.is_none() && !source.key_url.starts_with("https://") {
        return Err(WombatError::configuration(
            "Apt source key requires HTTPS unless sha256 is supplied",
        ));
    }
    if source.suite.is_empty() || !single_token(&source.suite) {
        return Err(WombatError::configuration("Apt source suite is invalid"));
    }
    validate_sorted_tokens(&source.components, false, "Apt source components")?;
    validate_sorted_tokens(&source.architectures, true, "Apt source architectures")?;
    if !matches!(source.key_format.as_str(), "gpg" | "asc") {
        return Err(WombatError::configuration(
            "Apt source key format must be `gpg` or `asc`",
        ));
    }
    if source.key_sha256.as_ref().is_some_and(|digest| {
        digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || digest.bytes().any(|byte| byte.is_ascii_uppercase())
    }) {
        return Err(WombatError::configuration(
            "Apt source key sha256 must be 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn validate_http_url(value: &str, label: &str) -> Result<()> {
    let parsed = url::Url::parse(value)
        .map_err(|error| WombatError::configuration(format!("{label} is invalid: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(WombatError::configuration(format!(
            "{label} must be an HTTP or HTTPS URL without credentials or a fragment"
        )));
    }
    Ok(())
}

fn single_token(value: &str) -> bool {
    !value.is_empty()
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

fn validate_sorted_tokens(values: &[String], allow_empty: bool, label: &str) -> Result<()> {
    if (!allow_empty && values.is_empty())
        || values.iter().any(|value| !single_token(value))
        || !values.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(WombatError::configuration(format!(
            "{label} must contain uniquely sorted non-empty tokens"
        )));
    }
    Ok(())
}

pub(crate) fn validate_apt_contract(
    requirements: &[Requirement],
    prerequisites: &[ProviderPrerequisite],
    preparations: &[ProviderPreparation],
) -> Result<()> {
    let apt_prerequisites = prerequisites
        .iter()
        .filter(|prerequisite| prerequisite.provider == BuiltinProvider::Apt.name())
        .map(|prerequisite| {
            if !prerequisite.elevated {
                return Err(WombatError::configuration(
                    "Apt source prerequisites must declare elevation",
                ));
            }
            Ok((prerequisite.identity.as_str(), apt_source(prerequisite)?))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for requirement in requirements
        .iter()
        .filter(|requirement| requirement.binding.provider == BuiltinProvider::Apt.name())
    {
        let binding = &requirement.binding;
        if !binding.elevated {
            return Err(WombatError::configuration(
                "Apt package bindings must declare elevation",
            ));
        }
        let FrozenValue::Map(data) = &binding.data else {
            return Err(WombatError::configuration("Apt binding data must be a map"));
        };
        if let Some(field) = data
            .keys()
            .find(|field| !matches!(field.as_str(), "name" | "source"))
        {
            return Err(WombatError::configuration(format!(
                "Apt binding does not support `{field}`"
            )));
        }
        let package = apt_identity(binding)?;
        if binding.identity != format!("package:{package}") {
            return Err(WombatError::configuration(
                "Apt binding identity does not match its package name",
            ));
        }
        match data.get("source") {
            None if binding.prerequisites.is_empty() => {}
            Some(FrozenValue::String(source))
                if binding.prerequisites == [format!("source:{source}")]
                    && apt_prerequisites.contains_key(format!("source:{source}").as_str()) => {}
            _ => {
                return Err(WombatError::configuration(
                    "Apt binding source and prerequisite identities are inconsistent",
                ));
            }
        }
    }
    for operation in preparations
        .iter()
        .filter(|operation| operation.provider == BuiltinProvider::Apt.name())
    {
        let FrozenValue::Map(data) = &operation.data else {
            return Err(WombatError::configuration(
                "Apt preparation data must be a map",
            ));
        };
        if operation.identity != "update-index"
            || !operation.elevated
            || data.len() != 1
            || !matches!(data.get("forced"), Some(FrozenValue::Boolean(_)))
        {
            return Err(WombatError::configuration(
                "Apt preparation must be the elevated `update-index` operation with boolean `forced` policy",
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_apt_source(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
) -> Result<(CheckStatus, String)> {
    let source = apt_source(prerequisite)?;
    let source_path = context.system_root.join(source.source_relative_path());
    let key_path = context.system_root.join(source.key_relative_path());
    for parent in [source_path.parent(), key_path.parent()]
        .into_iter()
        .flatten()
    {
        if !plain_directory_or_missing(parent)? {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} is not a plain directory; Apt source publication is unsafe",
                    parent.display()
                ),
            ));
        }
    }
    let expected = source.deb822();
    let source_bytes = read_optional(&source_path)?;
    let key_bytes = read_optional(&key_path)?;
    let owned = source_bytes
        .as_deref()
        .is_some_and(|bytes| bytes.starts_with(source.marker().as_bytes()));
    if source_bytes.as_deref() != Some(expected.as_bytes()) {
        if source_bytes.is_some() && !owned && !source.replace {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} contains unmanaged conflicting content; set replace = true to adopt it",
                    source_path.display()
                ),
            ));
        }
        if source_bytes.is_none() && key_bytes.is_some() && !source.replace {
            return Ok((
                CheckStatus::Unavailable,
                format!(
                    "{} exists without Wombat's source marker; set replace = true to adopt it",
                    key_path.display()
                ),
            ));
        }
        return Ok((
            if source_bytes.is_some() {
                CheckStatus::Outdated
            } else {
                CheckStatus::Missing
            },
            format!("Apt source {} needs reconciliation", source.name),
        ));
    }
    let Some(key_bytes) = key_bytes else {
        return Ok((
            CheckStatus::Missing,
            format!("Apt source {} signing key is absent", source.name),
        ));
    };
    if !apt_key_is_usable(&source, &key_bytes) {
        return Ok((
            CheckStatus::Outdated,
            format!("Apt source {} signing key is invalid", source.name),
        ));
    }
    if !plain_file_mode(&source_path, 0o644)? || !plain_file_mode(&key_path, 0o644)? {
        return Ok((
            CheckStatus::Outdated,
            format!("Apt source {} files need mode 0644", source.name),
        ));
    }
    Ok((
        CheckStatus::Satisfied,
        format!("Apt source {} is configured", source.name),
    ))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(crate) fn apt_key_is_usable(source: &AptSource, bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > OUTPUT_LIMIT {
        return false;
    }
    let armored = bytes.starts_with(b"-----BEGIN PGP PUBLIC KEY BLOCK-----");
    if (source.key_format == "asc") != armored {
        return false;
    }
    source
        .key_sha256
        .as_ref()
        .is_none_or(|expected| crate::storage::digest::hex_sha256(bytes) == *expected)
}

pub(crate) fn apt_source_needs_download(
    context: &RequirementContext<'_>,
    source: &AptSource,
) -> Result<bool> {
    Ok(
        read_optional(&context.system_root.join(source.key_relative_path()))?
            .as_deref()
            .is_none_or(|bytes| !apt_key_is_usable(source, bytes)),
    )
}

fn plain_file_mode(path: &Path, expected: u32) -> Result<bool> {
    let metadata = fs::symlink_metadata(path).map_err(|error| WombatError::io(path, error))?;
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(metadata.permissions().mode() & 0o777 == expected)
    }
    #[cfg(not(unix))]
    {
        let _ = expected;
        Ok(true)
    }
}

fn plain_directory_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(crate) fn reconcile_apt_source(
    context: &RequirementContext<'_>,
    prerequisite: &ProviderPrerequisite,
    noninteractive: bool,
) -> Result<()> {
    let source = apt_source(prerequisite)?;
    let (status, detail) = check_apt_source(context, prerequisite)?;
    if status == CheckStatus::Unavailable {
        return Err(WombatError::configuration(detail));
    }
    if status == CheckStatus::Satisfied {
        return Ok(());
    }

    let key_path = context.system_root.join(source.key_relative_path());
    let source_path = context.system_root.join(source.source_relative_path());
    let existing_key = read_optional(&key_path)?;
    let mut downloaded_key = None;
    let key_bytes = if let Some(bytes) = existing_key
        .as_deref()
        .filter(|bytes| apt_key_is_usable(&source, bytes))
    {
        bytes.to_vec()
    } else {
        let curl = require_command("curl", "Apt source key download")?;
        let download = download_apt_key(&curl, &source, noninteractive)?;
        let bytes =
            fs::read(download.path()).map_err(|error| WombatError::io(download.path(), error))?;
        downloaded_key = Some(download);
        bytes
    };

    let mut source_file = tempfile::NamedTempFile::new()
        .map_err(|error| WombatError::io(std::env::temp_dir(), error))?;
    std::io::Write::write_all(&mut source_file, source.deb822().as_bytes())
        .map_err(|error| WombatError::io(source_file.path(), error))?;
    source_file
        .as_file()
        .sync_all()
        .map_err(|error| WombatError::io(source_file.path(), error))?;

    let key_parent = key_path
        .parent()
        .ok_or_else(|| WombatError::configuration("Apt key path has no parent"))?;
    let source_parent = source_path
        .parent()
        .ok_or_else(|| WombatError::configuration("Apt source path has no parent"))?;
    let install = require_command("install", "Apt source installation")?;
    let mv = require_command("mv", "Apt source publication")?;
    let rm = require_command("rm", "Apt source publication cleanup")?;
    let elevated = prerequisite.elevated && context.system_root == Path::new("/");
    for parent in [key_parent, source_parent] {
        if !plain_directory_or_missing(parent)? {
            return Err(WombatError::configuration(format!(
                "{} is not a plain directory; Apt source publication is unsafe",
                parent.display()
            )));
        }
        if parent.exists() {
            continue;
        }
        run_mutating(
            &install,
            &["-d", "-m", "0755", &parent.to_string_lossy()],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        )?;
    }
    let nonce = std::process::id();
    let key_staging = key_path.with_extension(format!("{}.wombat-new-{nonce}", source.key_format));
    let source_staging = source_path.with_extension(format!("sources.wombat-new-{nonce}"));
    let key_input = if let Some(download) = &downloaded_key {
        download.path()
    } else {
        key_path.as_path()
    };
    let publish_key = !plain_file_mode_or_missing(&key_path, 0o644)?
        || read_optional(&key_path)?.as_deref() != Some(key_bytes.as_slice());
    let publish_source = !plain_file_mode_or_missing(&source_path, 0o644)?
        || read_optional(&source_path)?.as_deref() != Some(source.deb822().as_bytes());
    let publications = [
        publish_key.then_some((key_input, key_staging.as_path(), key_path.as_path())),
        publish_source.then_some((
            source_file.path(),
            source_staging.as_path(),
            source_path.as_path(),
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    for (input, staging, _) in &publications {
        if let Err(error) = run_mutating(
            &install,
            &[
                "-m",
                "0644",
                &input.to_string_lossy(),
                &staging.to_string_lossy(),
            ],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        ) {
            for (_, staged, _) in &publications {
                cleanup_apt_staging(&rm, staged, elevated, noninteractive);
            }
            return Err(error);
        }
    }
    let mut published = Vec::new();
    for (_, staging, final_path) in &publications {
        if let Err(error) = run_mutating(
            &mv,
            &[
                "-f",
                "--",
                &staging.to_string_lossy(),
                &final_path.to_string_lossy(),
            ],
            &BTreeMap::new(),
            elevated,
            noninteractive,
        ) {
            for (_, staged, _) in &publications {
                cleanup_apt_staging(&rm, staged, elevated, noninteractive);
            }
            return Err(error.with_note(format!(
                "Apt source `{}` publication completed: {}; remaining files were not rolled back",
                source.name,
                if published.is_empty() {
                    "none".to_string()
                } else {
                    published.join(", ")
                }
            )));
        }
        published.push(final_path.display().to_string());
    }
    Ok(())
}

pub(crate) fn download_apt_key(
    curl: &Path,
    source: &AptSource,
    noninteractive: bool,
) -> Result<tempfile::NamedTempFile> {
    let download = tempfile::NamedTempFile::new()
        .map_err(|error| WombatError::io(std::env::temp_dir(), error))?;
    let output = download.path().to_string_lossy().into_owned();
    let protocols = if source.key_sha256.is_some() {
        "=http,https"
    } else {
        "=https"
    };
    run_mutating(
        curl,
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--connect-timeout",
            "10",
            "--max-time",
            "60",
            "--max-filesize",
            "4194304",
            "--proto",
            protocols,
            "--proto-redir",
            protocols,
            "--output",
            &output,
            &source.key_url,
        ],
        &BTreeMap::new(),
        false,
        noninteractive,
    )?;
    let bytes =
        fs::read(download.path()).map_err(|error| WombatError::io(download.path(), error))?;
    if !apt_key_is_usable(source, &bytes) {
        let observed = crate::storage::digest::hex_sha256(&bytes);
        return Err(WombatError::configuration(format!(
            "Apt source `{}` downloaded an invalid {} signing key ({} bytes, sha256 {observed})",
            source.name,
            source.key_format,
            bytes.len()
        )));
    }
    Ok(download)
}

fn cleanup_apt_staging(rm: &Path, staging: &Path, elevated: bool, noninteractive: bool) {
    let _ = run_mutating(
        rm,
        &["-f", "--", &staging.to_string_lossy()],
        &BTreeMap::new(),
        elevated,
        noninteractive,
    );
}

fn plain_file_mode_or_missing(path: &Path, expected: u32) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => plain_file_mode(path, expected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(WombatError::io(path, error)),
    }
}

pub(crate) fn apt_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution::ladder::CoreRung;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn dpkg_records_preserve_empty_versions_for_uninstalled_packages() {
        for record in [
            "unknown ok not-installed\t",
            "unknown ok not-installed\t\n",
            "unknown ok not-installed\t\r\n",
        ] {
            assert_eq!(
                parse_dpkg_record(record),
                Some(("unknown ok not-installed", ""))
            );
        }
    }

    #[test]
    fn dpkg_records_keep_installed_versions_and_reject_missing_delimiters() {
        assert_eq!(
            parse_dpkg_record("install ok installed\t1.2.3-1\n"),
            Some(("install ok installed", "1.2.3-1"))
        );
        assert_eq!(parse_dpkg_record("unknown ok not-installed\n"), None);
    }

    fn prerequisite(replace: bool, digest: Option<&str>) -> ProviderPrerequisite {
        let mut data = serde_json::json!({
            "name": "yazi",
            "uri": "https://yazi-rs.github.io/builds/",
            "suite": "stable",
            "components": ["main"],
            "key": {
                "url": "https://yazi-rs.github.io/builds/yazi-keyring.gpg",
                "format": "gpg",
            },
            "replace": replace,
        });
        if let Some(digest) = digest {
            data["key"]["sha256"] = serde_json::Value::String(digest.to_string());
        }
        ProviderPrerequisite {
            provider: "apt".to_string(),
            identity: "source:yazi".to_string(),
            description: "Configure Apt source yazi".to_string(),
            when: CoreRung::DeployBefore.into(),
            elevated: true,
            data: serde_json::from_value(data).unwrap(),
        }
    }

    fn context<'a>(
        root: &Path,
        prerequisites: &'a [ProviderPrerequisite],
    ) -> RequirementContext<'a> {
        RequirementContext {
            id: "fixture",
            providers: &[],
            requirements: &[],
            prerequisites,
            preparations: &[],
            ladder: crate::execution::ladder::ExecutionLadder::default(),
            payload_root: root.join("payloads"),
            system_root: root.to_path_buf(),
            command_root: None,
        }
    }

    #[test]
    fn apt_source_reconciliation_is_rooted_canonical_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let prerequisite = prerequisite(true, None);
        let source = apt_source(&prerequisite).unwrap();
        let key_path = root.join(source.key_relative_path());
        fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        fs::write(&key_path, b"binary signing key").unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        let prerequisites = [prerequisite];
        let context = context(root, &prerequisites);

        assert_eq!(
            check_apt_source(&context, &prerequisites[0]).unwrap().0,
            CheckStatus::Missing
        );
        reconcile_apt_source(&context, &prerequisites[0], true).unwrap();
        assert_eq!(
            check_apt_source(&context, &prerequisites[0]).unwrap().0,
            CheckStatus::Satisfied
        );
        assert_eq!(fs::read(&key_path).unwrap(), b"binary signing key");
        let source_path = root.join(source.source_relative_path());
        assert_eq!(fs::read_to_string(&source_path).unwrap(), source.deb822());
        assert_eq!(
            fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(&source_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        reconcile_apt_source(&context, &prerequisites[0], true).unwrap();
        assert!(
            !root
                .join("etc/apt/sources.list.d")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("wombat-new"))
        );
    }

    #[test]
    fn unmanaged_source_requires_explicit_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let prerequisite = prerequisite(false, None);
        let source = apt_source(&prerequisite).unwrap();
        let source_path = temporary.path().join(source.source_relative_path());
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        fs::write(&source_path, "Types: deb\nURIs: https://example.invalid\n").unwrap();
        let prerequisites = [prerequisite];
        let context = context(temporary.path(), &prerequisites);
        let (status, detail) = check_apt_source(&context, &prerequisites[0]).unwrap();
        assert_eq!(status, CheckStatus::Unavailable);
        assert!(detail.contains("replace = true"), "{detail}");
    }

    #[test]
    fn apt_key_download_uses_bounded_protocol_arguments_and_verifies_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let script = temporary.path().join("curl-fixture");
        let log = temporary.path().join("arguments");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nout=''\nwhile [ \"$#\" -gt 0 ]; do\n  printf '%s\\n' \"$1\" >> '{}'\n  if [ \"$1\" = '--output' ]; then out=$2; shift 2; else shift; fi\ndone\nprintf 'binary signing key' > \"$out\"\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let digest = crate::storage::digest::hex_sha256(b"binary signing key");
        let prerequisite = prerequisite(true, Some(&digest));
        let mut source = apt_source(&prerequisite).unwrap();
        source.key_url = "http://example.invalid/key.gpg".to_string();
        let download = download_apt_key(&script, &source, true).unwrap();
        assert_eq!(fs::read(download.path()).unwrap(), b"binary signing key");
        let arguments = fs::read_to_string(&log).unwrap();
        assert!(arguments.contains("--max-filesize"));
        assert!(arguments.contains("4194304"));
        assert!(arguments.contains("=http,https"));

        source.key_sha256 = Some("0".repeat(64));
        let error = download_apt_key(&script, &source, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("downloaded an invalid gpg signing key"),
            "{error}"
        );
    }

    #[test]
    fn apt_key_format_distinguishes_armored_and_binary_material() {
        let prerequisite = prerequisite(true, None);
        let mut source = apt_source(&prerequisite).unwrap();
        assert!(apt_key_is_usable(&source, b"binary signing key"));
        assert!(!apt_key_is_usable(
            &source,
            b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n"
        ));
        source.key_format = "asc".to_string();
        assert!(!apt_key_is_usable(&source, b"binary signing key"));
        assert!(apt_key_is_usable(
            &source,
            b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n"
        ));
    }
}
