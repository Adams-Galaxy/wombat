use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use tempfile::tempdir;
use wombat::manifest::Production;
use wombat::{BuildOptions, PlanInspectSection, build, inspect_plan, materialise, plan};

fn repository() -> (tempfile::TempDir, std::path::PathBuf) {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("modules/dot_config")).unwrap();
    fs::create_dir_all(source.join("src/dot_config")).unwrap();
    fs::create_dir_all(source.join("tasks/helpers")).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.use('generated')\n",
    )
    .unwrap();
    fs::write(
        source.join("modules/dot_config/generated.lua"),
        r#"local w = require('wombat')
w.module.from(".config")
w.generate("lua.bin", { content = "lua\0bytes" })
w.build.task("generate.py", { greeting = "Hello", count = 2 })
"#,
    )
    .unwrap();
    fs::write(
        source.join("tasks/helpers/message.py"),
        "def message(value, count):\n    return f'{value} x {count}\\n'\n",
    )
    .unwrap();
    fs::write(
        source.join("tasks/generate.py"),
        r#"from wombat import params, output, work, cache
from helpers.message import message

assert work.name == "work"
cache.mkdir(exist_ok=True)
(cache / "seen").write_text("yes\n")
(output / "nested").mkdir(parents=True)
(output / "nested" / ".hidden").write_text(message(params["greeting"], params["count"]))
"#,
    )
    .unwrap();
    (temporary, source)
}

#[test]
fn plan_inspection_does_not_execute_tasks_and_build_publishes_generated_outputs() {
    let (_temporary, source) = repository();
    let options = BuildOptions::new(&source, "build");
    let planned = plan(options.clone()).unwrap();
    assert_eq!(planned.plan.format_version, 7);
    assert_eq!(planned.plan.tasks.len(), 1);
    assert!(planned.plan.requirements.is_empty());
    assert!(!planned.build_dir.join(".wombat/tasks").exists());
    let inspected = inspect_plan(&planned.plan, PlanInspectSection::Tasks);
    assert!(inspected.contains("tasks/generate.py"));
    assert!(inspected.contains("cache: true"));

    let built = build(options.clone()).unwrap();
    assert_eq!(built.manifest.format_version, 16);
    assert_eq!(built.manifest.plan_id, planned.plan.plan_id);
    assert_eq!(built.manifest.tasks[0].outputs.len(), 1);
    assert_eq!(
        fs::read(built.build_dir.join("tree/.config/lua.bin")).unwrap(),
        b"lua\0bytes"
    );
    assert_eq!(
        fs::read_to_string(built.build_dir.join("tree/.config/nested/.hidden")).unwrap(),
        "Hello x 2\n"
    );
    assert!(
        built
            .manifest
            .artifacts
            .iter()
            .any(|artifact| matches!(artifact.production, Production::GeneratedLua { .. }))
    );
    assert!(
        built
            .manifest
            .artifacts
            .iter()
            .any(|artifact| matches!(artifact.production, Production::Task { .. }))
    );

    let repeated = build(options).unwrap();
    assert_eq!(repeated.build_id, built.build_id);
    assert_eq!(repeated.manifest, built.manifest);
}

#[test]
fn stored_plan_materialises_without_reevaluating_lua() {
    let (temporary, source) = repository();
    let marker = temporary.path().join("constructed-once");
    fs::write(
        source.join("wombat.lua"),
        format!(
            "local f = assert(io.open({:?}, 'a'))\nf:write('x')\nf:close()\nlocal w = require('wombat')\nw.use('generated')\n",
            marker
        ),
    )
    .unwrap();
    let options = BuildOptions::new(&source, "stored-build");
    let constructed = plan(options.clone()).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "x");
    let materialised = materialise(options).unwrap();
    assert_eq!(constructed.plan.plan_id, materialised.manifest.plan_id);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "x");
}

#[test]
fn missing_task_interpreter_stops_before_execution() {
    let (_temporary, source) = repository();
    fs::write(
        source.join("modules/dot_config/generated.lua"),
        r#"local w = require('wombat')
w.module.from(".config")
w.build.task("generate.py", {}, { interpreter = "wombat-plan-0010-missing" })
"#,
    )
    .unwrap();
    let error = build(BuildOptions::new(&source, "build")).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("interpreter"), "{rendered}");
    assert!(!source.join("build/.wombat/tasks").exists());
}

#[test]
fn compile_only_check_still_requires_direct_task_runners() {
    let (_temporary, source) = repository();
    fs::write(
        source.join("modules/dot_config/generated.lua"),
        r#"local w = require('wombat')
w.module.from(".config")
w.build.task("generate.py", {}, { interpreter = "wombat-compile-only-missing" })
"#,
    )
    .unwrap();
    let planned = plan(BuildOptions::new(&source, "compile-check")).unwrap();
    let error = wombat::build::check_compile_only_plan(&source, &planned.build_dir, &planned.plan)
        .unwrap_err()
        .to_string();
    assert!(error.contains("wombat-compile-only-missing"), "{error}");
}

#[test]
fn task_cache_repairs_corruption_and_respects_revision_and_opt_out() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("modules/dot_config")).unwrap();
    fs::create_dir_all(source.join("src/dot_config")).unwrap();
    fs::create_dir_all(source.join("tasks")).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.use('generated')\n",
    )
    .unwrap();
    let module = source.join("modules/dot_config/generated.lua");
    fs::write(
        &module,
        "local w = require('wombat')\nw.module.from('.config')\nw.build.task('counter.py', { value = 'one' })\nw.build.task('gate.sh', {}, { cache = false })\n",
    )
    .unwrap();
    fs::write(
        source.join("tasks/counter.py"),
        r#"from wombat import cache, output, params
counter = cache / "runs"
runs = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(runs))
(output / "counter.txt").write_text(f"{params['value']}:{runs}\n")
"#,
    )
    .unwrap();
    fs::write(
        source.join("tasks/gate.sh"),
        "#!/bin/sh\nset -eu\nfor value in \"$@\"; do case \"$value\" in --cache-dir=*) cache=${value#*=};; esac; done\ncount=0\ntest ! -f \"$cache/runs\" || count=$(cat \"$cache/runs\")\nprintf '%s' $((count + 1)) > \"$cache/runs\"\n",
    )
    .unwrap();

    let options = BuildOptions::new(&source, "build");
    let first = build(options.clone()).unwrap();
    assert_eq!(
        fs::read_to_string(first.build_dir.join("tree/.config/counter.txt")).unwrap(),
        "one:1\n"
    );
    let second = build(options.clone()).unwrap();
    assert_eq!(second.build_id, first.build_id);

    let derivation = fs::read_dir(first.build_dir.join(".wombat/cache/derivations/tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| fs::read_to_string(path).unwrap().contains("counter.txt"))
        .unwrap();
    fs::write(&derivation, "not json").unwrap();
    let repaired = build(options.clone()).unwrap();
    assert_eq!(
        fs::read_to_string(repaired.build_dir.join("tree/.config/counter.txt")).unwrap(),
        "one:2\n"
    );

    fs::write(
        &module,
        "local w = require('wombat')\nw.module.from('.config')\nw.build.task('counter.py', { value = 'one' }, { cache = { revision = 'v2' } })\nw.build.task('gate.sh', {}, { cache = false })\n",
    )
    .unwrap();
    let revised = build(options).unwrap();
    assert_eq!(
        fs::read_to_string(revised.build_dir.join("tree/.config/counter.txt")).unwrap(),
        "one:3\n"
    );
    let task_root = revised.build_dir.join(".wombat/tasks");
    let gate_workspace = fs::read_dir(task_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("gate-sh")
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(gate_workspace.join("cache/runs")).unwrap(),
        "4"
    );
}

#[cfg(unix)]
#[test]
fn inferred_runners_share_the_fixed_protocol_and_publish_normal_artifacts() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("modules/dot_config")).unwrap();
    fs::create_dir_all(source.join("src/dot_config")).unwrap();
    fs::create_dir_all(source.join("tasks")).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.use('runners')\n",
    )
    .unwrap();
    fs::write(
        source.join("modules/dot_config/runners.lua"),
        "local w = require('wombat')\nw.module.from('.config')\nw.build.task('python.py')\nw.build.task('posix.sh')\nw.build.task('bash.bash')\nw.build.task('embedded.lua')\nw.build.task('direct')\n",
    )
    .unwrap();
    fs::write(
        source.join("tasks/python.py"),
        "from wombat import output\n(output / 'python').write_text('python\\n')\n",
    )
    .unwrap();
    for (name, value) in [("posix.sh", "posix"), ("bash.bash", "bash")] {
        fs::write(
            source.join("tasks").join(name),
            format!("for value in \"$@\"; do case \"$value\" in --output-dir=*) output=${{value#*=}};; esac; done\nprintf '{value}\\n' > \"$output/{value}\"\n"),
        )
        .unwrap();
    }
    fs::write(
        source.join("tasks/embedded.lua"),
        "local output\nfor i = 1, #arg do output = arg[i]:match('^%-%-output%-dir=(.*)$') or output end\nlocal f = assert(io.open(output .. '/lua', 'wb'))\nf:write('lua\\n')\nf:close()\n",
    )
    .unwrap();
    let direct = source.join("tasks/direct");
    fs::write(
        &direct,
        "#!/bin/sh\nfor value in \"$@\"; do case \"$value\" in --output-dir=*) output=${value#*=};; esac; done\nprintf 'direct\\n' > \"$output/direct\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&direct).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&direct, permissions).unwrap();

    let outcome = build(BuildOptions::new(&source, "build")).unwrap();
    for name in ["python", "posix", "bash", "lua", "direct"] {
        assert_eq!(
            fs::read_to_string(outcome.build_dir.join("tree/.config").join(name)).unwrap(),
            format!("{name}\n")
        );
    }
    assert_eq!(outcome.manifest.tasks.len(), 5);
}

#[test]
fn task_failure_preserves_the_previous_completed_product() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("modules/dot_config")).unwrap();
    fs::create_dir_all(source.join("src/dot_config")).unwrap();
    fs::create_dir_all(source.join("tasks")).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.use('app')\n",
    )
    .unwrap();
    let module = source.join("modules/dot_config/app.lua");
    fs::write(
        &module,
        "local w = require('wombat')\nw.module.from('.config')\nw.generate('stable', { content = 'old' })\n",
    )
    .unwrap();
    let first = build(BuildOptions::new(&source, "build")).unwrap();
    fs::write(
        source.join("tasks/fail.sh"),
        "printf 'expected failure\\n' >&2\nexit 23\n",
    )
    .unwrap();
    fs::write(
        &module,
        "local w = require('wombat')\nw.module.from('.config')\nw.generate('stable', { content = 'new' })\nw.build.task('fail.sh')\n",
    )
    .unwrap();
    let error = build(BuildOptions::new(&source, "build")).unwrap_err();
    assert!(error.to_string().contains("exit status 23"));
    let workspace = fs::read_dir(first.build_dir.join(".wombat/tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        fs::read_to_string(workspace.join("stderr.log"))
            .unwrap()
            .contains("expected failure")
    );
    assert!(workspace.join("stdout.log").is_file());
    let retained: wombat::Manifest =
        serde_json::from_slice(&fs::read(first.build_dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(retained.build_id, first.build_id);
    assert_eq!(
        fs::read(first.build_dir.join("tree/.config/stable")).unwrap(),
        b"old"
    );
}

#[test]
fn plan_normalizes_requirement_deadlines_for_cross_target_policy() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.target({ os = { name = 'linux', distribution = { id = 'ubuntu', id_like = { 'debian' } } }, arch = 'x86_64' })\nw.providers({ { name = 'apt', with = { update = true } } })\nw.need.command('git', { when = w.rungs.materialise.tasks })\n",
    )
    .unwrap();
    let host = wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
        wombat::OperatingSystemName::Macos,
        wombat::Architecture::Aarch64,
    ));
    let outcome = plan(BuildOptions::new(&source, "build").with_host(host)).unwrap();
    assert_eq!(outcome.plan.providers[0].name, "apt");
    assert_eq!(outcome.plan.requirements[0].candidates[0].name(), "git");
    assert_eq!(
        outcome.plan.requirements[0].when,
        wombat::ladder::CoreRung::MaterialiseTasks
    );
}

#[test]
fn task_paths_and_root_output_fail_before_publication() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("tasks")).unwrap();
    fs::write(source.join("outside.py"), "pass\n").unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.build.task('../outside.py')\n",
    )
    .unwrap();
    let traversal = plan(BuildOptions::new(&source, "build")).unwrap_err();
    assert!(traversal.to_string().contains("task entrypoint"));

    fs::write(
        source.join("tasks/output.py"),
        "from wombat import output\n(output / 'file').write_text('value')\n",
    )
    .unwrap();
    fs::write(
        source.join("wombat.lua"),
        "local w = require('wombat')\nw.build.task('output.py')\n",
    )
    .unwrap();
    build(BuildOptions::new(&source, "build")).unwrap();
    assert_eq!(
        fs::read_to_string(source.join("build/tree/file")).unwrap(),
        "value"
    );
}

#[test]
fn build_plan_round_trips_replaces_atomically_and_rejects_tampering() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    let entrypoint = source.join("wombat.lua");
    fs::write(
        &entrypoint,
        "local w = require('wombat')\nw.generate('value', { content = 'one', to = '.value' })\n",
    )
    .unwrap();
    let options = BuildOptions::new(&source, "build");
    let first = plan(options.clone()).unwrap();
    assert_eq!(wombat::plan::read(&first.build_dir).unwrap(), first.plan);

    fs::write(
        &entrypoint,
        "local w = require('wombat')\nw.generate('value', { content = 'two', to = '.value' })\n",
    )
    .unwrap();
    let second = plan(options).unwrap();
    assert_ne!(second.plan.plan_id, first.plan.plan_id);
    assert_eq!(wombat::plan::read(&second.build_dir).unwrap(), second.plan);
    assert!(!second.build_dir.join(".wombat/plan.previous").exists());

    let path = second.build_dir.join(".wombat/plan/plan.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["plan_id"] = serde_json::Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let tampered = wombat::plan::read(&second.build_dir).unwrap_err();
    assert!(tampered.to_string().contains("identity mismatch"));

    let mut legacy: serde_json::Value = serde_json::to_value(&second.plan).unwrap();
    legacy["format_version"] = serde_json::Value::from(1);
    fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
    let error = wombat::plan::read(&second.build_dir)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unsupported build plan format version 1") && error.contains("expected 7"),
        "{error}"
    );
}
