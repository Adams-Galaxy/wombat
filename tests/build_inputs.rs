use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wombat::manifest::{BuildInputOrigin, ObservationSubject};
use wombat::{
    Architecture, BuildOptions, Distribution, HostContext, Kernel, LooseVersion, OperatingSystem,
    OperatingSystemName, TargetOrigin, TargetPlatform, build, project_help,
};

struct Repository {
    root: PathBuf,
    _temporary: tempfile::TempDir,
}

impl Repository {
    fn new(root_lua: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("wombat.lua"), root_lua).unwrap();
        Self {
            root,
            _temporary: temporary,
        }
    }

    fn write(&self, relative: &str, contents: &str) {
        let (relative, from) = fixture_path(relative);
        let contents = fixture_module(contents, from);
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn build(
        &self,
        directory: &str,
        arguments: &[&str],
        host: HostContext,
    ) -> wombat::Result<wombat::BuildOutcome> {
        build(
            BuildOptions::new(&self.root, self.root.join(directory))
                .with_project_arguments(arguments.iter().copied())
                .with_host(host),
        )
    }
}

fn fixture_path(relative: &str) -> (String, Option<&'static str>) {
    for (prefix, replacement, from) in [
        ("modules/dot_config/", "modules/", Some(".config")),
        ("modules/dot_local/", "modules/", Some(".local")),
        ("modules/home/", "modules/", Some(".")),
        ("dot_config/", "src/dot_config/", None),
        ("dot_local/", "src/dot_local/", None),
    ] {
        if let Some(rest) = relative.strip_prefix(prefix) {
            return (format!("{replacement}{rest}"), from);
        }
    }
    (relative.to_string(), None)
}

fn fixture_module(contents: &str, from: Option<&str>) -> String {
    let Some(from) = from else {
        return contents.to_string();
    };
    if !contents.contains("install")
        && !contents.contains("generate")
        && !contents.contains("build.task")
    {
        return contents.to_string();
    };
    let at = contents
        .find('\n')
        .map_or(contents.len(), |index| index + 1);
    format!(
        "{}w.module.from({from:?})\n{}",
        &contents[..at],
        &contents[at..]
    )
}

fn mac_host(hostname: &str) -> HostContext {
    HostContext {
        platform: TargetPlatform {
            os: OperatingSystem {
                name: OperatingSystemName::Macos,
                family: "unix".into(),
                version: Some(LooseVersion::parse("15.4.1")),
                kernel: Some(Kernel {
                    name: "darwin".into(),
                    release: "24.5.0".into(),
                }),
                distribution: None,
            },
            arch: Architecture::Aarch64,
        },
        hostname: Some(hostname.into()),
        username: Some("unused-user".into()),
        home: Some(PathBuf::from("/unused/home")),
    }
}

fn linux_host() -> HostContext {
    HostContext {
        platform: TargetPlatform {
            os: OperatingSystem {
                name: OperatingSystemName::Linux,
                family: "unix".into(),
                version: None,
                kernel: Some(Kernel {
                    name: "linux".into(),
                    release: "6.14.0".into(),
                }),
                distribution: Some(Distribution {
                    id: "fedora".into(),
                    id_like: vec!["rhel".into()],
                    version: Some(LooseVersion::parse("42")),
                    pretty_name: Some("Fedora Linux 42".into()),
                }),
            },
            arch: Architecture::X86_64,
        },
        hostname: Some("fedora-box".into()),
        username: Some("unused-user".into()),
        home: Some(PathBuf::from("/unused/home")),
    }
}

fn parameterised_repository() -> Repository {
    let repository = Repository::new(
        r#"local w = require("wombat")
local input = w.inputs({
    target = w.input.target({ help = "Target OS and architecture" }),
    theme = w.input.choice({ values = { "dark", "light" }, default = "dark", short = "t" }),
    yazi = w.input.flag({ default = true, help = "Install Yazi" }),
    label = w.input.string({ default = w.host.hostname }),
    columns = w.input.integer({ default = 120, min = 40, max = 400 }),
})
local target = w.target(input.target)
w.use("app", {
    theme = input.theme,
    label = input.label,
    columns = input.columns,
    os = target.os.name,
    arch = target.arch,
    modern = target.os.version ~= nil and target.os.version.major ~= nil and target.os.version.major >= 15,
    distro = target.os.distribution ~= nil and target.os.distribution.id or "none",
})
if input.yazi then w.use("yazi") end
"#,
    );
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    repository.write(
        "dot_config/app.tmpl",
        "theme={{theme}}\nlabel={{label}}\ncolumns={{columns}}\nos={{os}}\narch={{arch}}\nmodern={{modern}}\ndistro={{distro}}\n",
    );
    repository.write(
        "modules/dot_config/yazi.lua",
        "local w = require('wombat')\nw.install('yazi.toml')\n",
    );
    repository.write("dot_config/yazi.toml", "enabled = true\n");
    repository
}

#[test]
fn defaults_are_contextual_frozen_and_manifested_without_unused_host_facts() {
    let repository = parameterised_repository();
    let outcome = repository
        .build("build/default", &[], mac_host("wombat-mac"))
        .unwrap();

    assert_eq!(outcome.manifest.format_version, 19);
    assert_eq!(outcome.manifest.target.origin, TargetOrigin::RootOverride);
    assert_eq!(
        outcome
            .manifest
            .target
            .declared_at
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("wombat.lua:9")
    );
    assert_eq!(
        outcome.manifest.target.platform.os.name,
        OperatingSystemName::Macos
    );
    assert_eq!(outcome.manifest.target.platform.arch, Architecture::Aarch64);
    assert!(outcome.manifest.target.platform.os.version.is_none());
    assert_eq!(
        outcome
            .manifest
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect::<Vec<_>>(),
        ["columns", "label", "target", "theme", "yazi"]
    );
    assert!(
        outcome
            .manifest
            .inputs
            .iter()
            .all(|input| input.origin == BuildInputOrigin::Default)
    );
    let observations = outcome
        .manifest
        .observations
        .iter()
        .map(|observation| (observation.subject, observation.path.as_str()))
        .collect::<Vec<_>>();
    assert!(observations.contains(&(ObservationSubject::Host, "hostname")));
    assert!(observations.contains(&(ObservationSubject::Target, "os.version.major")));
    assert!(observations.contains(&(ObservationSubject::Target, "os.distribution")));
    assert!(
        !observations
            .iter()
            .any(|(_, path)| *path == "username" || *path == "home")
    );
    assert_eq!(
        fs::read_to_string(repository.root.join("build/default/tree/.config/app")).unwrap(),
        "theme=dark\nlabel=wombat-mac\ncolumns=120\nos=macos\narch=aarch64\nmodern=true\ndistro=none\n"
    );
    assert!(
        repository
            .root
            .join("build/default/tree/.config/yazi.toml")
            .is_file()
    );
}

#[test]
fn cli_values_select_a_cross_target_variant_and_disable_an_artifact() {
    let repository = parameterised_repository();
    let outcome = build(
        BuildOptions::new(&repository.root, repository.root.join("build/linux"))
            .with_project_arguments([
                "--target",
                "linux/x86_64",
                "-t",
                "light",
                "--no-yazi",
                "--columns=80",
                "--label",
                "server",
            ])
            .with_host(mac_host("wombat-mac"))
            .with_compile_only(true),
    )
    .unwrap();

    assert_eq!(
        outcome.manifest.target.platform.os.name,
        OperatingSystemName::Linux
    );
    assert_eq!(outcome.manifest.target.platform.arch, Architecture::X86_64);
    assert!(
        outcome
            .manifest
            .inputs
            .iter()
            .all(|input| input.origin == BuildInputOrigin::CommandLine)
    );
    assert_eq!(
        fs::read_to_string(repository.root.join("build/linux/tree/.config/app")).unwrap(),
        "theme=light\nlabel=server\ncolumns=80\nos=linux\narch=x86_64\nmodern=false\ndistro=none\n"
    );
    assert!(
        !repository
            .root
            .join("build/linux/tree/.config/yazi.toml")
            .exists()
    );
}

#[test]
fn rich_linux_distribution_and_kernel_context_are_available_and_tracked() {
    let repository = Repository::new(
        "local w = require('wombat')\nlocal d = w.target.os.distribution\nw.use('app', { distro = d.id, version = d.version.raw, kernel = w.target.os.kernel.release })\n",
    );
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    repository.write("dot_config/app.tmpl", "{{distro}} {{version}} {{kernel}}\n");
    let outcome = repository.build("build", &[], linux_host()).unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/app")).unwrap(),
        "fedora 42 6.14.0\n"
    );
    for path in [
        "os.distribution.id",
        "os.distribution.version.raw",
        "os.kernel.release",
    ] {
        assert!(outcome.manifest.observations.iter().any(|observation| {
            observation.subject == ObservationSubject::Target && observation.path == path
        }));
    }
}

#[test]
fn unconsulted_host_changes_do_not_change_identity_but_consulted_defaults_do() {
    let plain = Repository::new("local w = require('wombat')\nw.use('app')\n");
    plain.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    plain.write("dot_config/app", "same\n");
    let first = plain.build("build/a", &[], mac_host("first")).unwrap();
    let second = plain.build("build/b", &[], mac_host("second")).unwrap();
    assert_eq!(first.build_id, second.build_id);

    let contextual = parameterised_repository();
    let first = contextual.build("build/a", &[], mac_host("first")).unwrap();
    let second = contextual
        .build("build/b", &[], mac_host("second"))
        .unwrap();
    assert_ne!(first.build_id, second.build_id);
}

#[test]
fn repository_help_is_generated_without_materialising() {
    let repository = parameterised_repository();
    let help = project_help(&repository.root, Some(mac_host("help-host"))).unwrap();
    assert!(help.contains("Repository build inputs"), "{help}");
    assert!(help.contains("--yazi, --no-yazi"), "{help}");
    assert!(help.contains("-t, --theme <CHOICE>"), "{help}");
    assert!(help.contains("[values: dark, light]"), "{help}");
    assert!(help.contains("[default: help-host]"), "{help}");
    assert!(!repository.root.join("build").exists());
}

#[test]
fn repository_help_stops_immediately_after_the_schema() {
    let repository = Repository::new(
        "local w = require('wombat')\nw.inputs({ theme = w.input.choice({ values = { 'dark' }, default = 'dark' }) })\nerror('root policy must not run for help')\n",
    );
    let help = project_help(&repository.root, Some(mac_host("help-host"))).unwrap();
    assert!(help.contains("--theme <CHOICE>"), "{help}");
}

#[test]
fn schema_constructors_aliases_defaults_and_collisions_are_validated() {
    let valid = Repository::new(
        r#"local w = require('wombat')
local input = w.inputs({
    feature_flag = w.input.flag({ default = false, long = 'feature', short = 'f' }),
    enabled = w.input.flag({ default = true }),
    choice = w.input.choice({ values = { 'a', 'b' }, default = 'a' }),
    text = w.input.string(),
    count = w.input.integer({ default = 2, min = 1, max = 3 }),
    target = w.input.target(),
})
if input.feature_flag and not input.enabled and input.text == 'hello' then w.use('app') end
"#,
    );
    valid.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    valid.write("dot_config/app", "ok\n");
    valid
        .build(
            "build",
            &["-f", "--no-enabled", "--text=hello"],
            mac_host("host"),
        )
        .unwrap();

    for (declaration, expected) in [
        (
            "w.input.flag({ default = 'yes' })",
            "default must be boolean",
        ),
        (
            "w.input.choice({ values = { 'a' }, default = 1 })",
            "default must be a string",
        ),
        (
            "w.input.string({ default = false })",
            "default must be a string",
        ),
        (
            "w.input.integer({ default = '1' })",
            "default must be an integer",
        ),
        (
            "w.input.target({ default = 'macos/aarch64' })",
            "does not support default",
        ),
        (
            "w.input.flag({ mystery = true })",
            "does not support option `mystery`",
        ),
    ] {
        let repository = Repository::new(&format!(
            "local w = require('wombat')\nw.inputs({{ value = {declaration} }})\n"
        ));
        let error = repository
            .build("build", &[], mac_host("host"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{declaration}: {error}");
    }

    for (schema, expected) in [
        (
            "a = w.input.flag({ long = 'same' }), b = w.input.flag({ long = 'same' })",
            "duplicate project option `--same`",
        ),
        (
            "a = w.input.flag({ short = 'x' }), b = w.input.flag({ short = 'x' })",
            "duplicate project short option `-x`",
        ),
        ("a = descriptor, b = descriptor", "descriptor `1` is reused"),
    ] {
        let prefix = if schema.contains("descriptor") {
            "local descriptor = w.input.flag()\n"
        } else {
            ""
        };
        let repository = Repository::new(&format!(
            "local w = require('wombat')\n{prefix}w.inputs({{ {schema} }})\n"
        ));
        let error = repository
            .build("build", &[], mac_host("host"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{schema}: {error}");
    }
}

#[test]
fn rich_target_overrides_are_normalized_and_context_arrays_are_immutable() {
    let repository = Repository::new(
        r#"local w = require('wombat')
local target = w.target({
    os = {
        name = 'linux',
        version = { raw = 'rolling' },
        kernel = { name = 'linux', release = '6.15.0' },
        distribution = {
            id = 'Arch',
            id_like = { 'linux', 'arch', 'linux' },
            version = { raw = 'rolling' },
            pretty_name = 'Arch Linux',
        },
    },
    arch = 'amd64',
})
w.use('app', { distro = target.os.distribution.id, arch = target.arch })
"#,
    );
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    repository.write("dot_config/app.tmpl", "{{distro}}/{{arch}}\n");
    let outcome = build(
        BuildOptions::new(&repository.root, repository.root.join("build"))
            .with_host(mac_host("host"))
            .with_compile_only(true),
    )
    .unwrap();
    assert_eq!(outcome.manifest.target.platform.arch, Architecture::X86_64);
    let distribution = outcome
        .manifest
        .target
        .platform
        .os
        .distribution
        .as_ref()
        .unwrap();
    assert_eq!(distribution.id, "arch");
    assert_eq!(distribution.id_like, ["arch", "linux"]);
    assert_eq!(distribution.version.as_ref().unwrap().raw, "rolling");

    let immutable = Repository::new(
        "local w = require('wombat')\nw.target.os.distribution.id_like[1] = 'changed'\n",
    );
    let error = immutable
        .build("build", &[], linux_host())
        .unwrap_err()
        .to_string();
    assert!(error.contains("immutable"), "{error}");
}

#[test]
fn semantic_cli_aliases_match_while_source_only_input_edits_change_exact_identity() {
    let repository = parameterised_repository();
    let alias = repository
        .build(
            "build/alias",
            &["--target", "macos/arm64"],
            mac_host("host"),
        )
        .unwrap();
    let canonical = repository
        .build(
            "build/canonical",
            &["--target", "macos/aarch64"],
            mac_host("host"),
        )
        .unwrap();
    assert_eq!(alias.build_id, canonical.build_id);

    let first = Repository::new(
        "local w = require('wombat')\nlocal i = w.inputs({ value = w.input.flag({ short = 'a', help = 'first wording' }) })\nif i.value then w.use('app') end\n",
    );
    first.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    first.write("dot_config/app", "same\n");
    let second = Repository::new(
        "local w = require('wombat')\nlocal i = w.inputs({ value = w.input.flag({ short = 'b', help = 'second wording' }) })\nif i.value then w.use('app') end\n",
    );
    second.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    second.write("dot_config/app", "same\n");
    let first = first.build("build", &["-a"], mac_host("host")).unwrap();
    let second = second.build("build", &["-b"], mac_host("host")).unwrap();
    assert_ne!(first.build_id, second.build_id);
    assert_eq!(first.manifest.inputs, second.manifest.inputs);
    assert_eq!(first.manifest.artifacts, second.manifest.artifacts);
}

#[test]
fn semantic_origins_affect_identity_and_repeated_observations_deduplicate() {
    let inputs = Repository::new(
        "local w = require('wombat')\nlocal i = w.inputs({ enabled = w.input.flag() })\nw.use('app', { enabled = i.enabled })\n",
    );
    inputs.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    inputs.write("dot_config/app.tmpl", "{{enabled}}\n");
    let default = inputs
        .build("build/default", &[], mac_host("host"))
        .unwrap();
    let explicit = inputs
        .build("build/explicit", &["--no-enabled"], mac_host("host"))
        .unwrap();
    assert_ne!(default.build_id, explicit.build_id);
    assert_eq!(
        default.manifest.inputs[0].value,
        explicit.manifest.inputs[0].value
    );
    assert_ne!(
        default.manifest.inputs[0].origin,
        explicit.manifest.inputs[0].origin
    );

    let observed = Repository::new(
        "local w = require('wombat')\nlocal first = w.host.hostname\nlocal second = w.host.hostname\nw.use('app', { value = first .. second })\n",
    );
    observed.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    observed.write("dot_config/app.tmpl", "{{value}}\n");
    let outcome = observed.build("build", &[], mac_host("host")).unwrap();
    assert_eq!(
        outcome
            .manifest
            .observations
            .iter()
            .filter(|observation| {
                observation.subject == ObservationSubject::Host && observation.path == "hostname"
            })
            .count(),
        1
    );

    let host_default = Repository::new("local w = require('wombat')\nw.use('app')\n");
    host_default.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    host_default.write("dot_config/app", "same\n");
    let root_override =
        Repository::new("local w = require('wombat')\nw.target('macos/aarch64')\nw.use('app')\n");
    root_override.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('app')\n",
    );
    root_override.write("dot_config/app", "same\n");
    assert_ne!(
        host_default
            .build("build", &[], mac_host("host"))
            .unwrap()
            .build_id,
        root_override
            .build("build", &[], mac_host("host"))
            .unwrap()
            .build_id
    );
}

#[test]
fn wombat_modules_cannot_declare_global_inputs() {
    let repository = Repository::new("local w = require('wombat')\nw.use('app')\n");
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.inputs({ enabled = w.input.flag() })\n",
    );
    let error = repository
        .build("build", &[], mac_host("host"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Wombat modules cannot declare project build inputs"),
        "{error}"
    );
}

#[test]
fn input_and_target_lifecycle_errors_are_precise() {
    let cases = [
        (
            "local w = require('wombat')\nw.use('app')\nw.inputs({ value = w.input.flag() })\n",
            "input constructors must run before",
        ),
        (
            "local w = require('wombat')\nlocal i = w.inputs({ target = w.input.target() })\nlocal _ = w.target.os.name\nw.target(i.target)\n",
            "after it was read",
        ),
        (
            "local w = require('wombat')\nlocal i = w.inputs({ target = w.input.target() })\nw.target(i.target)\nw.target(i.target)\n",
            "only once",
        ),
        (
            "local w = require('wombat')\nlocal i = w.inputs({ value = w.input.flag() })\ni.value = true\n",
            "immutable",
        ),
        (
            "local w = require('wombat')\nw.host.arch = 'x86_64'\n",
            "immutable",
        ),
    ];
    for (source, expected) in cases {
        let repository = Repository::new(source);
        let error = repository
            .build("build", &[], mac_host("host"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
}

#[test]
fn invalid_project_invocations_fail_before_module_evaluation() {
    let repository = parameterised_repository();
    for (case, (arguments, expected)) in [
        (vec!["--unknown"], "unknown project option"),
        (vec!["positional"], "unexpected positional"),
        (vec!["--theme", "blue"], "invalid choice"),
        (vec!["--columns", "20"], "outside its declared bounds"),
        (vec!["--theme", "dark", "--theme", "dark"], "more than once"),
        (vec!["-ty"], "combined or attached short"),
        (
            vec!["--target", "windows/x86_64"],
            "unsupported target operating system",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let error = repository
            .build(&format!("build/error-{case}"), &arguments, mac_host("host"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{arguments:?}: {error}");
        assert!(
            !repository
                .root
                .join(format!("build/error-{case}/tree"))
                .exists()
        );
    }

    let no_schema = Repository::new("local w = require('wombat')\n");
    let error = no_schema
        .build("build", &["--anything"], mac_host("host"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not declare w.inputs"), "{error}");
}

fn run_wombat(args: &[&str], current_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(args)
        .current_dir(current_dir)
        .output()
        .unwrap()
}

#[test]
fn cli_separates_core_and_repository_help_and_forwards_build_arguments() {
    let repository = parameterised_repository();
    let project_help = run_wombat(
        &[
            "--color",
            "never",
            "--source",
            repository.root.to_str().unwrap(),
            "build",
            "--",
            "--help",
        ],
        &repository.root,
    );
    assert!(
        project_help.status.success(),
        "{}",
        String::from_utf8_lossy(&project_help.stderr)
    );
    let project_help = String::from_utf8(project_help.stdout).unwrap();
    assert!(
        project_help.contains("Repository build inputs"),
        "{project_help}"
    );
    assert!(!project_help.contains("--build-dir"), "{project_help}");

    let colored_help = run_wombat(
        &[
            "--color",
            "always",
            "--source",
            repository.root.to_str().unwrap(),
            "build",
            "--",
            "--help",
        ],
        &repository.root,
    );
    assert!(colored_help.status.success());
    assert!(
        colored_help
            .stdout
            .windows(2)
            .any(|window| window == b"\x1b[")
    );

    let core_help = run_wombat(&["--color", "never", "build", "--help"], &repository.root);
    assert!(core_help.status.success());
    let core_help = String::from_utf8(core_help.stdout).unwrap();
    assert!(core_help.contains("--build-dir"), "{core_help}");

    let build_dir = repository.root.join("build/cli");
    let output = run_wombat(
        &[
            "--color",
            "never",
            "--source",
            repository.root.to_str().unwrap(),
            "build",
            "-B",
            build_dir.to_str().unwrap(),
            "--",
            "--theme",
            "light",
            "--no-yazi",
        ],
        &repository.root,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: wombat::Manifest =
        serde_json::from_slice(&fs::read(build_dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest
            .inputs
            .iter()
            .find(|input| input.name == "theme")
            .unwrap()
            .value,
        wombat::frozen::FrozenValue::String("light".into())
    );
    assert_eq!(
        manifest
            .inputs
            .iter()
            .find(|input| input.name == "yazi")
            .unwrap()
            .value,
        wombat::frozen::FrozenValue::Boolean(false)
    );
}

#[test]
fn cli_namespace_boundary_keeps_project_options_out_of_wombat_parsing() {
    let repository = Repository::new(
        "local w = require('wombat')\nlocal i = w.inputs({ color = w.input.string({ default = 'blue' }) })\nw.use('app', { color = i.color })\n",
    );
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nlocal c = w.module.config()\nw.install('app.tmpl', { with = c })\n",
    );
    repository.write("dot_config/app.tmpl", "{{color}}\n");
    let build_dir = repository.root.join("build/color-input");
    let output = run_wombat(
        &[
            "--source",
            repository.root.to_str().unwrap(),
            "build",
            "-B",
            build_dir.to_str().unwrap(),
            "--",
            "--color",
            "always",
        ],
        &repository.root,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.windows(2).any(|window| window == b"\x1b["));
    assert_eq!(
        fs::read_to_string(build_dir.join("tree/.config/app")).unwrap(),
        "always\n"
    );

    for command in ["add", "diff", "apply"] {
        let output = run_wombat(
            &[
                "--source",
                repository.root.to_str().unwrap(),
                command,
                "--",
                "--theme",
                "light",
            ],
            &repository.root,
        );
        assert!(
            !output.status.success(),
            "{command} unexpectedly accepted project inputs"
        );
    }
}

#[test]
fn cli_apply_forwards_project_arguments_to_the_exact_applied_build() {
    let repository = parameterised_repository();
    let target = repository._temporary.path().join("target-root");
    let state = repository._temporary.path().join("state");
    fs::create_dir(&target).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args([
            "--color",
            "never",
            "--source",
            repository.root.to_str().unwrap(),
            "apply",
            "-B",
            "build/deploy",
            "--target-root",
            target.to_str().unwrap(),
            "--conflict",
            "fail",
            "--",
            "--theme",
            "light",
            "--no-yazi",
        ])
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let app = fs::read_to_string(target.join(".config/app")).unwrap();
    assert!(app.contains("theme=light"), "{app}");
    assert!(!target.join(".config/yazi.toml").exists());
    let manifest: wombat::Manifest = serde_json::from_slice(
        &fs::read(repository.root.join("build/deploy/manifest.json")).unwrap(),
    )
    .unwrap();
    assert!(manifest.inputs.iter().any(|input| {
        input.name == "theme" && input.value == wombat::frozen::FrozenValue::String("light".into())
    }));
}

#[cfg(unix)]
#[test]
fn non_utf8_project_argument_is_rejected() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let repository = parameterised_repository();
    let error = build(
        BuildOptions::new(&repository.root, repository.root.join("build/non-utf8"))
            .with_project_arguments([OsString::from_vec(vec![b'-', b'-', 0xff])])
            .with_host(mac_host("host")),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("valid UTF-8"), "{error}");
}
