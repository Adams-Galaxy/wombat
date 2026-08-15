use std::fs;
use std::path::{Path, PathBuf};

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
fn lua_helper_packs_render_inline_subexpressions_hashes_and_dependencies() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w = require('wombat')\nw.template.helpers('theme.colors')\nw.use('app')\n",
    );
    repository.write(
        "lua/theme/base.lua",
        "return { join = function(left, right) return left .. right end }\n",
    );
    repository.write(
        "lua/theme/colors.lua",
        "local base = require('theme.base')\nreturn {\n  alpha = function(color, amount, options) return base.join(color, ':' .. amount .. options.suffix) end,\n  is_dark = function(color, options) return color == '#101010' end,\n}\n",
    );
    repository.write(
        "modules/app.lua",
        "local w = require('wombat')\nw.module.from('.')\nw.install('theme.tmpl', { with = { color = '#101010' } })\n",
    );
    repository.write(
        "src/theme.tmpl",
        "value={{alpha color 0.6 suffix='!'}}\n{{#if (is_dark color)}}dark{{else}}light{{/if}}\n",
    );

    let outcome = repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/theme")).unwrap(),
        "value=#101010:0.6!\ndark\n"
    );
    assert_eq!(outcome.manifest.template_helpers.len(), 1);
    let pack = &outcome.manifest.template_helpers[0];
    assert_eq!(pack.module, "theme.colors");
    assert_eq!(
        pack.sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        ["lua/theme/base.lua", "lua/theme/colors.lua"]
    );
    assert_eq!(
        pack.exports
            .iter()
            .map(|export| export.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "is_dark"]
    );
    verify_build(&repository.root.join("build")).unwrap();
}

#[test]
fn helper_registration_deduplicates_and_applies_exact_prefixes_from_modules() {
    let repository = Repository::new();
    repository.write("wombat.lua", "local w=require('wombat')\nw.use('app')\n");
    repository.write(
        "lua/format.lua",
        "return { tag = function(value, options) return '<' .. value .. '>' end }\n",
    );
    repository.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.template.helpers('format', {prefix='fmt_'})\nw.template.helpers('format')\nw.template.helpers('format')\nw.module.from('.')\nw.install('value.tmpl', {with={value='x'}})\n",
    );
    repository.write("src/value.tmpl", "{{tag value}} {{fmt_tag value}}\n");

    let outcome = repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/value")).unwrap(),
        "<x> <x>\n"
    );
    assert_eq!(outcome.manifest.template_helpers.len(), 2);
    assert_eq!(outcome.manifest.template_helpers[0].prefix, "");
    assert_eq!(outcome.manifest.template_helpers[1].prefix, "fmt_");
}

#[test]
fn helper_results_preserve_frozen_shapes_and_explicit_null() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('values')\nw.use('app')\n",
    );
    repository.write(
        "lua/values.lua",
        "local w=require('wombat')\nreturn {\n empty_array=function(options) return w.array() end,\n explicit_null=function(options) return w.null end,\n mapped=function(options) return {answer=42} end,\n empty_string=function(options) return '' end,\n deliberate_false=function(options) return false end,\n}\n",
    );
    repository.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('values.tmpl')\n",
    );
    repository.write(
        "src/values.tmpl",
        "array={{len (empty_array)}}\nmap={{lookup (mapped) 'answer'}}\nnull={{#if (explicit_null)}}bad{{else}}ok{{/if}}\nempty=[{{empty_string}}]\nfalse={{#if (deliberate_false)}}bad{{else}}ok{{/if}}\n",
    );

    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/values")).unwrap(),
        "array=0\nmap=42\nnull=ok\nempty=[]\nfalse=ok\n"
    );
}

#[test]
fn helper_registration_rejects_invalid_exports_and_collisions() {
    for (source, expected) in [
        ("return {}\n", "must export at least one function"),
        (
            "return setmetatable({ok=function() return true end}, {})\n",
            "plain table",
        ),
        ("return {value=1}\n", "must be a function"),
        (
            "return {['if']=function() return true end}\n",
            "cannot replace a built-in",
        ),
        (
            "return {['bad.name']=function() return true end}\n",
            "invalid template helper name",
        ),
    ] {
        let repository = Repository::new();
        repository.write(
            "wombat.lua",
            "local w=require('wombat')\nw.template.helpers('bad')\n",
        );
        repository.write("lua/bad.lua", source);
        let error = repository.build().unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }

    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('one')\nw.template.helpers('two')\n",
    );
    repository.write("lua/one.lua", "return {same=function() return 1 end}\n");
    repository.write("lua/two.lua", "return {same=function() return 2 end}\n");
    let error = repository.build().unwrap_err().to_string();
    assert!(
        error.contains("exported by both `one` and `two`"),
        "{error}"
    );
}

#[test]
fn helper_registration_rejects_unknown_options_and_unsafe_names() {
    for (declaration, expected) in [
        (
            "w.template.helpers('format', {unknown=true})",
            "does not support option `unknown`",
        ),
        (
            "w.template.helpers('../format')",
            "invalid repository Lua module name",
        ),
        (
            "w.template.helpers('format', {prefix='bad.'})",
            "unsupported characters",
        ),
    ] {
        let repository = Repository::new();
        repository.write(
            "wombat.lua",
            &format!("local w=require('wombat')\n{declaration}\n"),
        );
        let error = repository.build().unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn helper_calls_reject_invalid_returns_blocks_and_dynamic_dependencies() {
    for (function, template, expected) in [
        ("function(options) end", "{{bad}}", "returned 0"),
        (
            "function(options) return nil end",
            "{{bad}}",
            "return w.null",
        ),
        ("function(options) return 1, 2 end", "{{bad}}", "returned 2"),
        (
            "function(options) return function() end end",
            "{{bad}}",
            "invalid value",
        ),
        (
            "function(options) local value={} value.self=value return value end",
            "{{bad}}",
            "cyclic",
        ),
        (
            "function(options) return {[2]='sparse'} end",
            "{{bad}}",
            "sparse Lua arrays",
        ),
        (
            "function(options) return {[1]='array', key='map'} end",
            "{{bad}}",
            "contiguous arrays or string-keyed maps",
        ),
        (
            "function(options) return math.huge end",
            "{{bad}}",
            "finite",
        ),
        (
            "function(options) return true end",
            "{{#bad}}x{{/bad}}",
            "value-only",
        ),
        (
            "function(options) return require('late').value end",
            "{{bad}}",
            "was not captured during construction",
        ),
    ] {
        let repository = Repository::new();
        repository.write(
            "wombat.lua",
            "local w=require('wombat')\nw.template.helpers('bad')\nw.use('app')\n",
        );
        repository.write("lua/bad.lua", &format!("return {{bad={function}}}\n"));
        repository.write("lua/late.lua", "return {value='late'}\n");
        repository.write(
            "modules/app.lua",
            "local w=require('wombat')\nw.module.from('.')\nw.install('bad.tmpl')\n",
        );
        repository.write("src/bad.tmpl", template);
        let error = repository.build().unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }

    let missing = Repository::new();
    missing.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('bad')\nw.use('app')\n",
    );
    missing.write(
        "lua/bad.lua",
        "return {bad=function(value, options) error('helper executed') end}\n",
    );
    missing.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('bad.tmpl')\n",
    );
    missing.write("src/bad.tmpl", "{{bad missing}}\n");
    let error = missing.build().unwrap_err().to_string();
    assert!(error.contains("missing"), "{error}");
    assert!(!error.contains("helper executed"), "{error}");
}

#[test]
fn helper_sandbox_and_instruction_limit_fail_with_source_context() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('ambient')\n",
    );
    repository.write(
        "lua/ambient.lua",
        "local value=os.getenv('HOME')\nreturn {value=function(options) return value end}\n",
    );
    let error = repository.build().unwrap_err().render(false);
    assert!(
        error.contains("failed to load template helper pack `ambient`"),
        "{error}"
    );
    assert!(error.contains("lua/ambient.lua:1"), "{error}");

    let dependency = Repository::new();
    dependency.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('parent')\n",
    );
    dependency.write(
        "lua/parent.lua",
        "local child=require('child')\nreturn {value=function(options) return child end}\n",
    );
    dependency.write("lua/child.lua", "error('dependency failed')\n");
    let error = dependency.build().unwrap_err().render(false);
    assert!(error.contains("lua/child.lua:1"), "{error}");

    let looping = Repository::new();
    looping.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('loop')\nw.use('app')\n",
    );
    looping.write(
        "lua/loop.lua",
        "return {spin=function(options) while true do end end}\n",
    );
    looping.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('loop.tmpl')\n",
    );
    looping.write("src/loop.tmpl", "{{spin}}\n");
    let error = looping.build().unwrap_err().render(false);
    assert!(error.contains("instruction limit"), "{error}");
    assert!(error.contains("src/loop.tmpl:1"), "{error}");
    assert!(
        error.contains("helper defined at lua/loop.lua:1"),
        "{error}"
    );

    let allocating = Repository::new();
    allocating.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('allocate')\nw.use('app')\n",
    );
    allocating.write(
        "lua/allocate.lua",
        "return {grow=function(options) local values={} while true do values[#values + 1]=string.rep('x', 1024 * 1024) end end}\n",
    );
    allocating.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('allocate.tmpl')\n",
    );
    allocating.write("src/allocate.tmpl", "{{grow}}\n");
    let error = allocating.build().unwrap_err().render(false);
    assert!(error.contains("memory"), "{error}");
    assert!(error.contains("src/allocate.tmpl:1"), "{error}");
    assert!(
        error.contains("helper defined at lua/allocate.lua:1"),
        "{error}"
    );
}

#[test]
fn frozen_helper_payloads_are_verified_before_materialisation() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('simple')\nw.use('app')\n",
    );
    repository.write(
        "lua/simple.lua",
        "return {value=function(options) return 'ok' end}\n",
    );
    repository.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('simple.tmpl')\n",
    );
    repository.write("src/simple.tmpl", "{{value}}\n");

    let options = wombat::BuildOptions::new(&repository.root, "build").with_host(fixture_host());
    wombat::plan(options.clone()).unwrap();
    fs::write(
        repository
            .root
            .join("build/.wombat/plan/payloads/helpers/lua/simple.lua"),
        "tampered",
    )
    .unwrap();
    let error = wombat::materialise(options.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("failed verification"), "{error}");

    wombat::plan(options.clone().with_clean(true)).unwrap();
    fs::remove_file(
        repository
            .root
            .join("build/.wombat/plan/payloads/helpers/lua/simple.lua"),
    )
    .unwrap();
    let error = wombat::materialise(options.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("is missing"), "{error}");

    wombat::plan(options.clone().with_clean(true)).unwrap();
    repository.write(
        "lua/simple.lua",
        "return {value=function(options) return 'changed' end}\n",
    );
    let error = wombat::materialise(options.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("stored plan is stale"), "{error}");

    #[cfg(unix)]
    {
        repository.write(
            "lua/simple.lua",
            "return {value=function(options) return 'ok' end}\n",
        );
        wombat::plan(options.clone().with_clean(true)).unwrap();
        let payload = repository
            .root
            .join("build/.wombat/plan/payloads/helpers/lua/simple.lua");
        fs::remove_file(&payload).unwrap();
        std::os::unix::fs::symlink(repository.root.join("lua/simple.lua"), &payload).unwrap();
        let error = wombat::materialise(options).unwrap_err().to_string();
        assert!(error.contains("must not be a symbolic link"), "{error}");
    }
}

#[test]
fn helper_registry_changes_invalidate_plan_product_and_template_cache_identity() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('changing')\nw.use('app')\n",
    );
    repository.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('changing.tmpl')\n",
    );
    repository.write("src/changing.tmpl", "{{value}}\n");
    repository.write(
        "lua/changing.lua",
        "return {value=function(options) return 'one' end}\n",
    );
    let first = repository.build().unwrap();
    let cache = repository
        .root
        .join("build/.wombat/cache/derivations/templates");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);

    repository.write(
        "lua/changing.lua",
        "return {value=function(options) return 'two' end}\n",
    );
    let second = repository.build().unwrap();
    assert_ne!(first.manifest.plan_id, second.manifest.plan_id);
    assert_ne!(first.build_id, second.build_id);
    assert_eq!(fs::read_dir(cache).unwrap().count(), 2);
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/changing")).unwrap(),
        "two\n"
    );
}

#[test]
fn helper_payload_verifier_rejects_unexpected_files() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('simple')\n",
    );
    repository.write(
        "lua/simple.lua",
        "return {value=function(options) return 'ok' end}\n",
    );
    let options = wombat::BuildOptions::new(&repository.root, "build").with_host(fixture_host());
    wombat::plan(options.clone()).unwrap();
    fs::write(
        repository
            .root
            .join("build/.wombat/plan/payloads/helpers/unexpected.lua"),
        "unexpected",
    )
    .unwrap();
    let error = wombat::materialise(options).unwrap_err().to_string();
    assert!(
        error.contains("unexpected template helper payload"),
        "{error}"
    );

    fs::remove_file(
        repository
            .root
            .join("build/.wombat/plan/payloads/helpers/unexpected.lua"),
    )
    .unwrap();
    fs::create_dir(
        repository
            .root
            .join("build/.wombat/plan/payloads/helpers/unexpected"),
    )
    .unwrap();
    let error = wombat::materialise(
        wombat::BuildOptions::new(&repository.root, "build").with_host(fixture_host()),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("unexpected template helper payload directory"),
        "{error}"
    );
}

#[test]
fn frozen_helper_payloads_relocate_with_the_stored_plan() {
    let repository = Repository::new();
    repository.write(
        "wombat.lua",
        "local w=require('wombat')\nw.template.helpers('simple')\nw.use('app')\n",
    );
    repository.write(
        "lua/simple.lua",
        "return {value=function(options) return 'relocated' end}\n",
    );
    repository.write(
        "modules/app.lua",
        "local w=require('wombat')\nw.module.from('.')\nw.install('simple.tmpl')\n",
    );
    repository.write("src/simple.tmpl", "{{value}}\n");

    let original = repository.root.join("build");
    wombat::plan(wombat::BuildOptions::new(&repository.root, &original).with_host(fixture_host()))
        .unwrap();
    let relocated = repository.root.join("relocated-build");
    copy_tree(&original, &relocated);

    wombat::materialise(
        wombat::BuildOptions::new(&repository.root, &relocated).with_host(fixture_host()),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(relocated.join("tree/simple")).unwrap(),
        "relocated\n"
    );
}

fn copy_tree(source: &Path, destination: &Path) {
    let metadata = fs::symlink_metadata(source).unwrap();
    if metadata.is_dir() {
        fs::create_dir_all(destination).unwrap();
        let mut entries = fs::read_dir(source)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        }
    } else {
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, metadata.permissions()).unwrap();
    }
}

#[test]
fn template_fixture_matches_exact_manifest_and_rendered_tree() {
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
fn handlebars_supports_policy_helpers_lookup_logging_and_partials() {
    for (source, expected) in [
        ("{{eq 1 1}}\n", "true\n"),
        ("{{#if (eq 1 1)}}yes{{/if}}\n", "yes\n"),
        ("{{lookup values 0}}\n", "x\n"),
        ("{{log value}}\n", "\n"),
        ("{{#each values}}value{{else}}empty{{/each}}\n", "value\n"),
        ("{{#with value}}value{{else}}empty{{/with}}\n", "value\n"),
        (
            "{{#*inline \"part\"}}content{{/inline}}{{> part}}\n",
            "content\n",
        ),
    ] {
        let repository = basic_repository(
            "local w = require('wombat')\nw.install('value.tmpl', { with = { value = 'x', values = { 'x' } } })\n",
            "value.tmpl",
            source,
        );
        repository.build().unwrap();
        assert_eq!(
            fs::read_to_string(repository.root.join("build/tree/.config/value")).unwrap(),
            expected,
            "source {source:?}"
        );
    }
}

#[test]
fn coalesce_skips_missing_and_null_but_preserves_deliberate_falsy_values() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { nothing = w.null, no = false, zero = 0, empty = '', values = { 'a' }, object = { key = 'value' }, generic = { border = '#928374' } } })\n",
        "value.tmpl",
        "{{coalesce missing.path nothing generic.border}}|{{coalesce missing.path no generic.border}}|{{coalesce missing.path zero generic.border}}|{{coalesce missing.path empty generic.border}}|{{coalesce missing.path values generic.border}}|{{coalesce missing.path object generic.border}}\n",
    );
    repository.build().unwrap();
    assert_eq!(
        fs::read_to_string(repository.root.join("build/tree/.config/value")).unwrap(),
        "#928374|false|0||[a]|[object]\n"
    );
}

#[test]
fn coalesce_rejects_no_params_and_an_exhausted_fallback_chain() {
    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl', { with = { nothing = w.null } })\n",
        "value.tmpl",
        "{{coalesce missing.path nothing also.missing}}\n",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("also"), "{error}");

    let repository = basic_repository(
        "local w = require('wombat')\nw.install('value.tmpl')\n",
        "value.tmpl",
        "{{coalesce}}\n",
    );
    let error = repository.build().unwrap_err().to_string();
    assert!(error.contains("coalesce"), "{error}");
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
