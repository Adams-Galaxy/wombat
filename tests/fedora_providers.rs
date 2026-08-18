use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

fn fedora_host() -> wombat::HostContext {
    wombat::HostContext {
        platform: wombat::TargetPlatform {
            os: wombat::OperatingSystem {
                name: wombat::OperatingSystemName::Linux,
                family: "unix".into(),
                version: None,
                kernel: Some(wombat::Kernel {
                    name: "linux".into(),
                    release: "6.14.0".into(),
                }),
                distribution: Some(wombat::Distribution {
                    id: "fedora".into(),
                    id_like: Vec::new(),
                    version: Some(wombat::LooseVersion::parse("44")),
                    pretty_name: Some("Fedora Linux 44".into()),
                }),
            },
            arch: wombat::Architecture::X86_64,
        },
        hostname: Some("fedora-test".into()),
        username: Some("wombat".into()),
        home: Some(PathBuf::from("/home/wombat")),
        paths: wombat::HostPaths::conventional(Path::new("/home/wombat")),
        wsl: false,
    }
}

fn build(source: &str) -> (TempDir, wombat::BuildOutcome) {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("source");
    fs::create_dir(&repository).unwrap();
    fs::write(repository.join("wombat.lua"), source).unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&repository, temporary.path().join("build"))
            .with_host(fedora_host()),
    )
    .unwrap();
    (temporary, outcome)
}

fn build_error(source: &str) -> String {
    build_error_with_host(source, fedora_host())
}

fn build_error_with_host(source: &str, host: wombat::HostContext) -> String {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("source");
    fs::create_dir(&repository).unwrap();
    fs::write(repository.join("wombat.lua"), source).unwrap();
    wombat::build(
        wombat::BuildOptions::new(&repository, temporary.path().join("build")).with_host(host),
    )
    .unwrap_err()
    .to_string()
}

#[test]
fn dnf_resolves_commands_and_freezes_rpm_fusion_prerequisites() {
    let (_temporary, outcome) = build(
        r#"local w = require("wombat")
w.providers({ { name = "dnf", with = { aliases = { custom = "custom-package" } } } })
w.need.command("rg")
w.need.command("custom")
w.need.package("ffmpeg", { provider = "dnf", with = { rpmfusion = "free" } })
w.need.package("steam", { provider = "dnf", with = { rpmfusion = "nonfree" }, when = w.rungs.deploy.before })
"#,
    );

    assert_eq!(outcome.manifest.format_version, 21);
    assert_eq!(outcome.manifest.construction_version, 3);
    assert_eq!(outcome.manifest.providers[0].name, "dnf");
    assert_eq!(
        serde_json::to_value(&outcome.manifest.providers[0].origin).unwrap(),
        serde_json::json!({ "kind": "builtin", "contract_version": 1 })
    );
    let requirement = |name: &str| {
        outcome
            .manifest
            .requirements
            .iter()
            .find(|value| value.candidates[value.selected as usize].name() == name)
            .unwrap()
    };
    assert_eq!(
        requirement("rg").binding.package.as_deref(),
        Some("ripgrep")
    );
    assert_eq!(
        requirement("custom").binding.package.as_deref(),
        Some("custom-package")
    );
    assert!(
        outcome
            .manifest
            .requirements
            .iter()
            .all(|value| value.binding.elevated)
    );
    assert_eq!(outcome.manifest.prerequisites.len(), 2);
    assert_eq!(
        outcome.manifest.prerequisites[0].identity,
        "repository:rpmfusion-free"
    );
    assert_eq!(
        outcome.manifest.prerequisites[0].when.id(),
        "materialise.before"
    );
    assert_eq!(
        outcome.manifest.prerequisites[1].identity,
        "repository:rpmfusion-nonfree"
    );
    assert_eq!(outcome.manifest.prerequisites[1].when.id(), "deploy.before");
    let free = serde_json::to_value(&outcome.manifest.prerequisites[0].data).unwrap();
    assert_eq!(free["major"], 44);
    assert_eq!(
        free["url"],
        "https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-44.noarch.rpm"
    );
    assert_eq!(
        requirement("steam").binding.prerequisites,
        ["repository:rpmfusion-free", "repository:rpmfusion-nonfree",]
    );
    let providers = wombat::inspect(&outcome.build_dir, wombat::InspectSection::Providers).unwrap();
    let requirements =
        wombat::inspect(&outcome.build_dir, wombat::InspectSection::Requirements).unwrap();
    assert!(
        providers.contains("repository:rpmfusion-free"),
        "{providers}"
    );
    assert!(requirements.contains("elevated: true"), "{requirements}");
}

#[test]
fn flatpak_freezes_exact_scope_ref_and_one_flathub_prerequisite_per_scope() {
    let (_temporary, outcome) = build(
        r#"local w = require("wombat")
w.providers({ { name = "flatpak", with = { scope = "system" } } })
w.need.package("org.gnome.Calculator", { provider = "flatpak" })
w.need.package("org.freedesktop.Platform", {
  provider = "flatpak",
  with = { kind = "runtime", scope = "user", branch = "24.08" },
})
w.need.package("com.spotify.Client", { provider = "flatpak" })
"#,
    );

    assert_eq!(outcome.manifest.prerequisites.len(), 2);
    assert_eq!(
        outcome.manifest.prerequisites[0].identity,
        "remote:system:flathub"
    );
    assert!(outcome.manifest.prerequisites[0].elevated);
    assert_eq!(
        outcome.manifest.prerequisites[1].identity,
        "remote:user:flathub"
    );
    assert!(!outcome.manifest.prerequisites[1].elevated);
    let requirement = |name: &str| {
        outcome
            .manifest
            .requirements
            .iter()
            .find(|value| value.candidates[value.selected as usize].name() == name)
            .unwrap()
    };
    let application = &requirement("org.gnome.Calculator").binding;
    assert_eq!(
        application.identity,
        "ref:system:app:org.gnome.Calculator:x86_64:current"
    );
    assert!(application.elevated);
    assert_eq!(application.prerequisites, ["remote:system:flathub"]);
    let runtime = &requirement("org.freedesktop.Platform").binding;
    assert_eq!(
        runtime.identity,
        "ref:user:runtime:org.freedesktop.Platform:x86_64:24.08"
    );
    assert!(!runtime.elevated);
    assert_eq!(runtime.prerequisites, ["remote:user:flathub"]);
    let providers = wombat::inspect(&outcome.build_dir, wombat::InspectSection::Providers).unwrap();
    assert!(providers.contains("remote:system:flathub"), "{providers}");
    assert!(providers.contains("remote:user:flathub"), "{providers}");
}

#[test]
fn fedora_provider_schemas_reject_unsupported_policy() {
    let cases = [
        (
            "w.providers({'dnf'}); w.need.package('tool', { provider='dnf', with={ copr='owner/project' } })",
            "DNF package does not support `with.copr`",
        ),
        (
            "w.providers({{name='dnf', with={mirror='unsafe'}}}); w.need.package('tool', {provider='dnf'})",
            "DNF provider does not support `with.mirror`",
        ),
        (
            "w.providers({{name='dnf', with={aliases=true}}}); w.need.package('tool', {provider='dnf'})",
            "DNF provider `with.aliases` must be a table",
        ),
        (
            "w.providers({'dnf'}); w.need.package('tool', { provider='dnf', with={ rpmfusion='both' } })",
            "must be `free` or `nonfree`",
        ),
        (
            "w.providers({'dnf'}); w.need.package('tool', { provider='dnf', with={ name='--setopt=unsafe' } })",
            "must be a package token beginning with a letter or number",
        ),
        (
            "w.providers({'flatpak'}); w.need.command('org.gnome.Calculator')",
            "Flatpak resolves explicit package requirements only",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('org.gnome.Calculator', { provider='flatpak', minimum='1.0' })",
            "do not support minimum versions",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('org.gnome.Calculator', { provider='flatpak', with={ remote='company' } })",
            "only the `flathub` remote",
        ),
        (
            "w.providers({{name='flatpak', with={remote='company'}}}); w.need.package('org.gnome.Calculator', {provider='flatpak'})",
            "Flatpak provider does not support `with.remote`",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('org.gnome.Calculator', { provider='flatpak', with={ kind='document' } })",
            "must be `app` or `runtime`",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('org.gnome.Calculator', { provider='flatpak', with={ scope='session' } })",
            "must be `system` or `user`",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('unsafe', { provider='flatpak' })",
            "must be an application or runtime ID",
        ),
        (
            "w.providers({'flatpak'}); w.need.package('org.gnome.Calculator', { provider='flatpak', with={ branch='--unsafe' } })",
            "must be a safe branch token",
        ),
    ];
    for (body, expected) in cases {
        let error = build_error(&format!("local w=require('wombat')\n{body}\n"));
        assert!(
            error.contains(expected),
            "expected {expected:?} in {error:?}"
        );
    }
}

#[test]
fn dnf_refuses_non_fedora_linux_while_flatpak_remains_available() {
    let mut ubuntu = fedora_host();
    let distribution = ubuntu.platform.os.distribution.as_mut().unwrap();
    distribution.id = "ubuntu".to_string();
    distribution.id_like = vec!["debian".to_string()];
    distribution.version = Some(wombat::LooseVersion::parse("24.04"));
    distribution.pretty_name = Some("Ubuntu 24.04 LTS".to_string());

    let error = build_error_with_host(
        "local w=require('wombat')\nw.providers({'dnf'})\nw.need.package('ripgrep',{provider='dnf'})\n",
        ubuntu.clone(),
    );
    assert!(error.contains("requires a Fedora Linux target"), "{error}");

    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("source");
    fs::create_dir(&repository).unwrap();
    fs::write(
        repository.join("wombat.lua"),
        "local w=require('wombat')\nw.providers({'flatpak'})\nw.need.package('org.gnome.Calculator',{provider='flatpak'})\n",
    )
    .unwrap();
    let outcome = wombat::build(
        wombat::BuildOptions::new(&repository, temporary.path().join("build")).with_host(ubuntu),
    )
    .unwrap();
    assert_eq!(outcome.manifest.requirements[0].binding.provider, "flatpak");
}

#[test]
fn rpm_fusion_requires_a_rich_numeric_fedora_target() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("source");
    fs::create_dir(&repository).unwrap();
    fs::write(
        repository.join("wombat.lua"),
        "local w=require('wombat')\nw.providers({'dnf'})\nw.need.package('ffmpeg',{provider='dnf',with={rpmfusion='free'}})\n",
    )
    .unwrap();
    let mut host = fedora_host();
    host.platform.os.distribution.as_mut().unwrap().version =
        Some(wombat::LooseVersion::parse("rawhide"));
    let error = wombat::build(
        wombat::BuildOptions::new(&repository, temporary.path().join("build")).with_host(host),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("numeric Fedora major version"), "{error}");
}
