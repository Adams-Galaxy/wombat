use std::fs;
use std::process::Command;

use wombat::{BuildOptions, build};

fn repository(root: &std::path::Path, root_lua: &str) {
    fs::create_dir_all(root.join("modules/dot_config")).unwrap();
    fs::create_dir_all(root.join("dot_config")).unwrap();
    fs::write(root.join("wombat.lua"), root_lua).unwrap();
}

#[test]
fn runtime_failures_render_the_user_source_without_wrapper_frames() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    repository(&root, "local w = require(\"wombat\")\nw.use(\"broken\")\n");
    fs::write(
        root.join("modules/dot_config/broken.lua"),
        "local value = \"bad\"\nreturn value + 1\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "-S"])
        .arg(&root)
        .arg("build")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(
        stderr.contains("modules/dot_config/broken.lua:2"),
        "{stderr}"
    );
    assert!(stderr.contains("return value + 1"), "{stderr}");
    assert!(stderr.contains("module `broken` was selected at wombat.lua:2"));
    assert!(!stderr.contains("<wombat>/init.lua"), "{stderr}");
    assert!(!stderr.contains("stack traceback"), "{stderr}");
}

#[test]
fn trace_mode_exposes_filtered_user_frames_and_underlying_evidence() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    repository(
        &root,
        "local w = require(\"wombat\")\nlocal function choose() w.use(\"../bad\") end\nchoose()\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "--trace", "-S"])
        .arg(&root)
        .arg("build")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert!(!output.status.success());
    assert!(stderr.contains("user trace:"), "{stderr}");
    assert!(stderr.contains("underlying:"), "{stderr}");
    assert!(stderr.contains("wombat.lua:2"), "{stderr}");
}

#[test]
fn required_helpers_are_catalogued_and_preserve_declaration_callers() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    repository(
        &root,
        "local helper = require(\"helper\")\nhelper.select()\n",
    );
    fs::create_dir(root.join("lua")).unwrap();
    fs::write(
        root.join("lua/helper.lua"),
        "local w = require(\"wombat\")\nreturn { select = function() w.use(\"app\") end }\n",
    )
    .unwrap();
    fs::write(
        root.join("modules/dot_config/app.lua"),
        "local w = require(\"wombat\")\nw.install(\"app.toml\")\n",
    )
    .unwrap();
    fs::write(root.join("dot_config/app.toml"), "value = true\n").unwrap();

    let manifest = build(BuildOptions::new(&root, "build")).unwrap().manifest;
    assert!(
        manifest
            .sources
            .iter()
            .any(|source| source.path == "lua/helper.lua")
    );
    let selection = manifest
        .dependencies
        .iter()
        .find(|dependency| dependency.to == "app")
        .unwrap();
    assert_eq!(selection.declared_at.primary.source, "lua/helper.lua");
    assert!(
        selection
            .declared_at
            .callers
            .iter()
            .any(|location| location.source == "wombat.lua")
    );
}

#[test]
fn syntax_errors_and_template_errors_use_source_aware_diagnostics() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("syntax");
    repository(&root, "local w = require(\"wombat\")\nw.use(\"broken\")\n");
    fs::write(
        root.join("modules/dot_config/broken.lua"),
        "local value = )\n",
    )
    .unwrap();
    let syntax = build(BuildOptions::new(&root, "build"))
        .unwrap_err()
        .render(false);
    assert!(
        syntax.contains("modules/dot_config/broken.lua:1"),
        "{syntax}"
    );
    assert!(syntax.contains("local value = )"), "{syntax}");

    let template_root = temporary.path().join("template");
    repository(
        &template_root,
        "local w = require(\"wombat\")\nw.use(\"broken\")\n",
    );
    fs::write(
        template_root.join("modules/dot_config/broken.lua"),
        "local w = require(\"wombat\")\nw.install.template(\"broken.tmpl\", { with = {} })\n",
    )
    .unwrap();
    fs::write(
        template_root.join("dot_config/broken.tmpl"),
        "value = {{missing}}\n",
    )
    .unwrap();
    let template = build(BuildOptions::new(&template_root, "build"))
        .unwrap_err()
        .render(false);
    assert!(
        template.contains("--> dot_config/broken.tmpl:1:"),
        "{template}"
    );
    assert!(template.contains("value = {{missing}}"), "{template}");
    assert!(template.contains('^'), "{template}");
}

#[test]
fn tail_call_loss_is_reported_honestly() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    repository(&root, "local w = require(\"wombat\")\nw.use(\"tail\")\n");
    fs::write(
        root.join("modules/dot_config/tail.lua"),
        "local function fail() return \"bad\" + 1 end\nlocal function helper() return fail() end\nreturn helper()\n",
    )
    .unwrap();

    let rendered = build(BuildOptions::new(&root, "build"))
        .unwrap_err()
        .render(false);
    assert!(rendered.contains("tail call"), "{rendered}");
    assert!(rendered.contains("frames may be unavailable"), "{rendered}");
}
