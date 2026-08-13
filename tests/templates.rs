use std::fs;
use std::path::PathBuf;

use wombat::manifest::Production;
use wombat::{
    Architecture, BuildOptions, HostContext, OperatingSystemName, TargetPlatform, build,
    verify_build,
};

fn fixture_host() -> HostContext {
    HostContext::fixture(TargetPlatform::minimal(
        OperatingSystemName::Macos,
        Architecture::Aarch64,
    ))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn manifest_json(manifest: &wombat::Manifest) -> String {
    serde_json::to_string_pretty(manifest).unwrap()
}

/// `project_identity` digests the repository root, so it is checked for shape
/// rather than pinned. Every other field, including the identities derived from
/// content, is compared exactly.
fn exact_manifest_json(value: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(value).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in ["project_identity"] {
        let Some(identity) = object.remove(key) else {
            continue;
        };
        let identity = identity.as_str().unwrap();
        assert!(
            identity.strip_prefix("sha256:").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
            }),
            "{key} must be a sha256 digest, got {identity}"
        );
    }
    value
}

struct Repository {
    root: PathBuf,
    _temporary: tempfile::TempDir,
}

impl Repository {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        fs::create_dir(&root).unwrap();
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

    fn build(&self) -> wombat::Result<wombat::BuildOutcome> {
        build(BuildOptions::new(&self.root, self.root.join("build")).with_host(fixture_host()))
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

fn basic_repository(module: &str, source_name: &str, source: &str) -> Repository {
    let repository = Repository::new();
    repository.write("wombat.lua", "local w = require('wombat')\nw.use('app')\n");
    repository.write("modules/dot_config/app.lua", module);
    repository.write(&format!("dot_config/{source_name}"), source);
    repository
}

#[test]
fn renders_realistic_starship_and_wezterm_templates_with_frozen_context() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w = require('wombat')\nw.use('theme')\nw.use('starship')\nw.use('wezterm')\n",
    );
    repository.write(
        "modules/theme.lua",
        "return { colors = { accent = '#7e9cd8', background = '#1f1f28' } }\n",
    );
    repository.write(
        "modules/dot_config/starship.lua",
        "local w = require('wombat')\nlocal theme = w.using('theme')\nlocal enabled = true\nlocal format = enabled and 'wombat' or nil\nw.install('starship.toml.tmpl', { with = { colors = theme.colors, format = format } })\n",
    );
    repository.write(
        "dot_config/starship.toml.tmpl",
        "{{#if format}}[palette]\nformat = '{{format}}'\n{{#each colors}}{{@key}} = '{{this}}'\n{{/each}}{{else}}disabled = true\n{{/if}}",
    );
    repository.write(
        "modules/dot_config/wezterm.lua",
        "local w = require('wombat')\nlocal theme = w.using('theme')\nw.install('wezterm.lua.tmpl', { with = { colors = theme.colors, literal = '<&>' } })\n",
    );
    repository.write(
        "dot_config/wezterm.lua.tmpl",
        "local config = {}\n{{{{raw}}}}-- {{ kept literally }}{{{{/raw}}}}\n{{#with colors}}config.background = '{{background}}'\n{{/with}}config.literal = '{{literal}}'\nreturn config\n",
    );

    let outcome = repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/starship.toml")).unwrap(),
        "[palette]\nformat = 'wombat'\naccent = '#7e9cd8'\nbackground = '#1f1f28'\n"
    );
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/wezterm.lua")).unwrap(),
        "local config = {}\n-- {{ kept literally }}\nconfig.background = '#1f1f28'\nconfig.literal = '<&>'\nreturn config\n"
    );
    assert!(outcome.manifest.artifacts.iter().all(|artifact| {
        matches!(
            &artifact.production,
            Production::Template {
                renderer,
                source_digest,
                context: wombat::frozen::FrozenValue::Map(_),
            } if renderer.name == "handlebars"
                && renderer.contract_version == 1
                && source_digest.len() == 71
        )
    }));
    verify_build(&repository.root.join("build")).unwrap();
}

#[test]
fn template_fixture_matches_exact_manifest_v17_and_rendered_tree() {
    let root = fixture("templates");
    let temporary = tempfile::tempdir().unwrap();
    let build_dir = temporary.path().join("build");
    let outcome = build(BuildOptions::new(&root, &build_dir).with_host(fixture_host())).unwrap();
    let expected = fs::read_to_string(root.join("expected-manifest.json")).unwrap();
    assert_eq!(
        exact_manifest_json(&manifest_json(&outcome.manifest)),
        exact_manifest_json(&expected)
    );
    let starship = fs::read_to_string(build_dir.join("tree/.config/starship.toml")).unwrap();
    assert!(starship.contains("format = \"wombat\""));
    assert!(starship.contains("palette_accent = \"#7e9cd8\""));
    let wezterm = fs::read_to_string(build_dir.join("tree/.config/wezterm.lua")).unwrap();
    assert!(wezterm.contains("-- {{ this remains literal }}"));
    assert!(wezterm.ends_with("return config\n"));
}

#[test]
fn callable_install_and_explicit_forms_disambiguate_template_names() {
    let repository = Repository::new();
    repository.write("wombat.lua", "local w = require('wombat')\nw.use('app')\n");
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install.file('literal.tmpl')\nw.install.template('dynamic', { to = '.config/rendered', with = { value = 'yes' } })\n",
    );
    repository.write("dot_config/literal.tmpl", "{{ untouched }}\n");
    repository.write("dot_config/dynamic", "value={{ value }}\n");

    let outcome = repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/literal.tmpl")).unwrap(),
        "{{ untouched }}\n"
    );
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/rendered")).unwrap(),
        "value=yes\n"
    );
    let literal = outcome
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.target.path == ".config/literal.tmpl")
        .unwrap();
    let rendered = outcome
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.target.path == ".config/rendered")
        .unwrap();
    assert!(matches!(literal.production, Production::Static));
    assert!(matches!(rendered.production, Production::Template { .. }));
}

#[test]
fn tmpl_suffix_renders_with_an_empty_context_and_strips_only_inferred_target() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('empty.tmpl')\nw.install('explicit.tmpl', { to = '.config/kept.tmpl' })\n",
        "empty.tmpl",
        "plain\n",
    );
    repository.write("dot_config/explicit.tmpl", "explicit\n");

    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/empty")).unwrap(),
        "plain\n"
    );
    assert!(
        repository
            .root
            .join("build/tree/.config/kept.tmpl")
            .is_file()
    );
}

#[test]
fn template_errors_identify_the_source_and_missing_value() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('broken.tmpl')\n",
        "broken.tmpl",
        "line one\n{{ absent.value }}\n",
    );

    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("dot_config/broken.tmpl"), "{error}");
    assert!(error.contains("absent"), "{error}");
    assert!(error.contains("line 2") || error.contains(":2"), "{error}");

    for block in ["if", "unless", "each", "with"] {
        let repository = basic_repository(
            "local w = require('wombat')\nw.install('broken.tmpl')\n",
            "broken.tmpl",
            &format!("{{{{#{block} absent}}}}unexpected{{{{/{block}}}}}\n"),
        );
        let error = repository.build().unwrap_err().to_string();
        assert!(error.contains("dot_config/broken.tmpl"), "{block}: {error}");
        assert!(error.contains("absent"), "{block}: {error}");
    }
}

#[test]
fn templates_require_utf8_sources_and_map_contexts() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install.template('binary', { with = {} })\n",
        "binary",
        "placeholder",
    );
    fs::write(repository.root.join("src/dot_config/binary"), [0xff, 0xfe]).unwrap();
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");

    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { 'array' } })\n",
        "value.tmpl",
        "plain",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("must be a string-keyed map"), "{error}");
}

#[test]
fn template_syntax_and_unsupported_context_fail_before_publication() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('broken.tmpl')\n",
        "broken.tmpl",
        "{{#if value}}\n",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("failed to compile template"), "{error}");
    assert!(error.contains("dot_config/broken.tmpl"), "{error}");
    assert!(!repository.root.join("build/manifest.json").exists());

    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { callback = function() end } })\n",
        "value.tmpl",
        "plain\n",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("unsupported Lua function value"), "{error}");
}

#[test]
fn handlebars_contract_rejects_policy_helpers_lookup_logging_and_partials() {
    for source in [
        "{{eq 1 1}}\n",
        "{{#if (eq 1 1)}}yes{{/if}}\n",
        "{{lookup values 0}}\n",
        "{{log value}}\n",
        "{{#each values}}value{{else}}empty{{/each}}\n",
        "{{#with value}}value{{else}}empty{{/with}}\n",
        "{{#*inline \"part\"}}content{{/inline}}{{> part}}\n",
    ] {
        let repository = basic_repository(
            "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'x', values = { 'x' } } })\n",
            "value.tmpl",
            source,
        );
        let error = repository.build().unwrap_err().to_string();
        assert!(
            error.contains("failed to render template")
                || error.contains("failed to compile template")
                || error.contains("uses unsupported Handlebars"),
            "source {source:?}: {error}"
        );
    }
}

#[test]
fn explicit_template_and_file_forms_reject_directories() {
    for call in ["w.install.template('tree')", "w.install.file('tree')"] {
        let repository = basic_repository(
            &format!("local w = require('wombat')\n{call}\n"),
            "tree/file",
            "plain",
        );
        let error = repository.build().unwrap_err().to_string();
        assert!(error.contains("cannot select a directory"), "{error}");
    }
}

#[test]
fn explicit_template_and_file_forms_accept_globs() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install.file('literal-*.tmpl', { to = '.config/literal' })\nw.install.template('render-*', { to = '.config/rendered', with = { value = 'ready' } })\n",
        "literal-one.tmpl",
        "{{ value }} remains literal\n",
    );
    repository.write("src/dot_config/render-one", "value={{ value }}\n");

    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(
            repository
                .root
                .join("build/tree/.config/literal/literal-one.tmpl")
        )
        .unwrap(),
        "{{ value }} remains literal\n"
    );
    assert_eq!(
        fs::read_to_string(
            repository
                .root
                .join("build/tree/.config/rendered/render-one")
        )
        .unwrap(),
        "value=ready\n"
    );
}

#[test]
fn template_context_is_frozen_when_declared() {
    let repository = basic_repository(
        "local w = require('wombat')\nlocal context = { value = 'before' }\nw.install('value.tmpl', { with = context })\ncontext.value = 'after'\n",
        "value.tmpl",
        "{{ value }}\n",
    );

    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/value")).unwrap(),
        "before\n"
    );
}

#[test]
fn template_source_and_context_changes_affect_build_identity() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'one' } })\n",
        "value.tmpl",
        "{{ value }}\n",
    );
    let first = repository.build().unwrap();
    repository.write(
        "modules/dot_config/app.lua",
        "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'two' } })\n",
    );
    let context = repository.build().unwrap();
    assert_ne!(first.build_id, context.build_id);
    repository.write("dot_config/value.tmpl", "value={{ value }}\n");
    let source = repository.build().unwrap();
    assert_ne!(context.build_id, source.build_id);
}

#[test]
fn template_cache_corruption_is_recomputed_without_changing_the_product() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'one' } })\n",
        "value.tmpl",
        "{{ value }}\n",
    );
    let first = repository.build().unwrap();
    let derivation = fs::read_dir(
        repository
            .root
            .join("build/.wombat/cache/derivations/templates"),
    )
    .unwrap()
    .next()
    .unwrap()
    .unwrap()
    .path();
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(&derivation).unwrap()).unwrap();
    let digest = record["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let blob = repository
        .root
        .join("build/.wombat/cache/blobs/sha256")
        .join(digest);
    fs::write(&blob, "corrupt").unwrap();

    let repaired = repository.build().unwrap();
    assert_eq!(repaired.build_id, first.build_id);
    assert_eq!(fs::read(&blob).unwrap(), b"one\n");
    fs::write(&derivation, "not json").unwrap();
    let descriptor = repository.build().unwrap();
    assert_eq!(descriptor.build_id, first.build_id);
    assert!(serde_json::from_slice::<serde_json::Value>(&fs::read(derivation).unwrap()).is_ok());
}

#[test]
fn verifier_rejects_unknown_template_renderer_contracts() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'one' } })\n",
        "value.tmpl",
        "{{ value }}\n",
    );
    repository.build().unwrap();
    let manifest_path = repository.root.join("build/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["artifacts"][0]["production"]["renderer"]["contract_version"] = serde_json::json!(2);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let error = verify_build(&repository.root.join("build"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unsupported template renderer contract"),
        "{error}"
    );
}

#[test]
fn unknown_template_options_are_rejected() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { renderer = 'other' })\n",
        "value.tmpl",
        "plain",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(
        error.contains("does not support option `renderer`"),
        "{error}"
    );
}
