use std::fs;
use std::process::Command;

use wombat::{
    BuildOptions, InspectSection, PlanInspectSection, build, compare, explain, inspect,
    inspect_plan, plan,
};

struct Fixture {
    _temporary: tempfile::TempDir,
    source: std::path::PathBuf,
    build: std::path::PathBuf,
}

impl Fixture {
    fn new(contents: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let build_dir = temporary.path().join("product");
        fs::create_dir_all(source.join("modules")).unwrap();
        fs::create_dir_all(source.join("src/dot_config")).unwrap();
        fs::write(
            source.join("wombat.lua"),
            "local w = require(\"wombat\")\nw.use(\"app\")\n",
        )
        .unwrap();
        fs::write(
            source.join("modules/app.lua"),
            "local w = require(\"wombat\")\nw.module.from(\".config\")\nw.install(\"app.toml\")\n",
        )
        .unwrap();
        fs::write(source.join("src/dot_config/app.toml"), contents).unwrap();
        build(BuildOptions::new(&source, &build_dir)).unwrap();
        Self {
            _temporary: temporary,
            source,
            build: build_dir,
        }
    }
}

#[test]
fn every_product_section_reads_the_verified_manifest() {
    let fixture = Fixture::new("value = 1\n");
    let expectations = [
        (InspectSection::Overview, "Build sha256:"),
        (InspectSection::Inputs, "Inputs\n  none"),
        (InspectSection::Target, "Target"),
        (InspectSection::Modules, "module"),
        (InspectSection::Dependencies, "<root> -> app"),
        (InspectSection::Helpers, "Template helper packs\n  none"),
        (InspectSection::Artifacts, ".config/app.toml"),
        (InspectSection::Sources, "modules/app.lua"),
    ];
    for (section, expected) in expectations {
        let output = inspect(&fixture.build, section).unwrap();
        assert!(output.contains(expected), "{section:?}: {output}");
    }
}

#[test]
fn helper_registry_is_inspectable_explainable_and_comparable() {
    let left = Fixture::new("value = 1\n");
    fs::create_dir_all(left.source.join("lua")).unwrap();
    fs::write(
        left.source.join("wombat.lua"),
        "local w=require('wombat')\nw.template.helpers('format')\nw.use('app')\n",
    )
    .unwrap();
    fs::write(
        left.source.join("lua/format.lua"),
        "return {tag=function(value, options) return '<' .. value .. '>' end}\n",
    )
    .unwrap();
    fs::write(
        left.source.join("modules/app.lua"),
        "local w=require('wombat')\nw.module.from('.config')\nw.install('app.toml', {with={value='x'}})\n",
    )
    .unwrap();
    fs::write(
        left.source.join("src/dot_config/app.toml"),
        "{{tag value}}\n",
    )
    .unwrap();
    let planned = plan(BuildOptions::new(&left.source, &left.build)).unwrap();
    wombat::materialise(BuildOptions::new(&left.source, &left.build)).unwrap();

    let helpers = inspect(&left.build, InspectSection::Helpers).unwrap();
    assert!(helpers.contains("format"), "{helpers}");
    assert!(helpers.contains("tag (lua/format.lua:1)"), "{helpers}");
    let plan_helpers = inspect_plan(&planned.plan, PlanInspectSection::Helpers);
    assert!(plan_helpers.contains("format"), "{plan_helpers}");
    let explanation = explain(&left.build, ".config/app.toml", None, None).unwrap();
    assert!(
        explanation.contains("template helpers: format"),
        "{explanation}"
    );

    let right = Fixture::new("value = 1\n");
    let comparison = compare(&right.build, &left.build).unwrap();
    assert!(comparison.contains("Template helpers"), "{comparison}");
    assert!(comparison.contains("Add format:"), "{comparison}");
}

#[test]
fn explanation_accepts_target_source_and_absolute_aliases_with_freshness() {
    let fixture = Fixture::new("value = 1\n");
    for selector in [
        ".config/app.toml".to_string(),
        ".config/app.toml".to_string(),
        "src/dot_config/app.toml".to_string(),
        fixture
            ._temporary
            .path()
            .join("home/.config/app.toml")
            .to_string_lossy()
            .into_owned(),
    ] {
        let home = fixture._temporary.path().join("home");
        let output = explain(
            &fixture.build,
            &selector,
            Some(&fixture.source),
            Some(&home),
        )
        .unwrap();
        assert!(output.contains("owner: app"), "{selector}: {output}");
        assert!(output.contains("w.install(\"app.toml\")"), "{output}");
    }

    fs::write(
        fixture.source.join("modules/app.lua"),
        "-- changed\nlocal w = require(\"wombat\")\nw.install(\"app.toml\")\n",
    )
    .unwrap();
    let stale = explain(
        &fixture.build,
        ".config/app.toml",
        Some(&fixture.source),
        None,
    )
    .unwrap();
    assert!(stale.contains("current source differs from this build"));
}

#[test]
fn inspection_never_evaluates_changed_repository_lua() {
    let fixture = Fixture::new("value = 1\n");
    let sentinel = fixture._temporary.path().join("evaluated");
    fs::write(
        fixture.source.join("wombat.lua"),
        format!("local f = io.open({:?}, \"w\")\nf:write(\"bad\")\nf:close()\nerror(\"must not run\")\n", sentinel),
    )
    .unwrap();

    let output = inspect(&fixture.build, InspectSection::Overview).unwrap();
    assert!(output.contains("Build sha256:"));
    assert!(!sentinel.exists());
}

#[test]
fn explanation_rejects_ambiguous_source_selectors_with_candidates() {
    let fixture = Fixture::new("value = 1\n");
    fs::write(
        fixture.source.join("modules/app.lua"),
        concat!(
            "local w = require(\"wombat\")\n",
            "w.module.from(\".config\")\n",
            "w.install(\"app.toml\")\n",
            "w.install(\"app.toml\", { to = \".config/other.toml\" })\n",
        ),
    )
    .unwrap();
    build(BuildOptions::new(&fixture.source, &fixture.build)).unwrap();

    let error = explain(&fixture.build, "src/dot_config/app.toml", None, None).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("is ambiguous"), "{rendered}");
    assert!(rendered.contains(".config/app.toml"), "{rendered}");
    assert!(rendered.contains(".config/other.toml"), "{rendered}");
}

#[test]
fn relocated_products_inspect_without_source_and_corruption_is_rejected() {
    let fixture = Fixture::new("value = 1\n");
    fs::remove_dir_all(&fixture.source).unwrap();

    let output = explain(&fixture.build, ".config/app.toml", None, None).unwrap();
    assert!(output.contains("source repository is not available"));

    fs::write(fixture.build.join("tree/.config/app.toml"), "tampered\n").unwrap();
    let error = inspect(&fixture.build, InspectSection::Overview).unwrap_err();
    assert!(error.to_string().contains("content identity"), "{error}");
}

#[test]
fn semantic_comparison_reports_source_and_artifact_changes() {
    let left = Fixture::new("value = 1\n");
    let right = Fixture::new("value = 2\n");
    let output = compare(&left.build, &right.build).unwrap();
    assert!(output.contains("Product comparison"));
    assert!(output.contains("Artifacts"));
    assert!(output.contains("Change .config/app.toml"));
}

#[test]
fn cli_supports_sections_explanation_and_one_or_two_product_operands() {
    let left = Fixture::new("value = 1\n");
    let right = Fixture::new("value = 2\n");
    let inspect_output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "inspect", "artifacts", "-B"])
        .arg(&left.build)
        .output()
        .unwrap();
    assert!(inspect_output.status.success());
    assert!(
        String::from_utf8(inspect_output.stdout)
            .unwrap()
            .contains("Artifacts")
    );

    let explain_output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "explain", ".config/app.toml", "-B"])
        .arg(&left.build)
        .output()
        .unwrap();
    assert!(explain_output.status.success());

    let compare_output = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "compare"])
        .arg(&left.build)
        .arg(&right.build)
        .output()
        .unwrap();
    assert!(compare_output.status.success());
    assert!(
        String::from_utf8(compare_output.stdout)
            .unwrap()
            .contains("Product comparison")
    );

    build(BuildOptions::new(&left.source, "build")).unwrap();
    let one_operand = Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "-S"])
        .arg(&left.source)
        .arg("compare")
        .arg(&right.build)
        .output()
        .unwrap();
    assert!(one_operand.status.success());
    assert!(
        String::from_utf8(one_operand.stdout)
            .unwrap()
            .contains("Product comparison")
    );
}
