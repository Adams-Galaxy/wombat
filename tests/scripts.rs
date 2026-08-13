use std::fs;
use std::path::Path;

use tempfile::tempdir;
use wombat::{BuildOptions, build, plan};

fn repository(lua: &str, script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(root.join("wombat.lua"), lua).unwrap();
    fs::write(root.join("scripts/mark.sh"), script).unwrap();
    (temporary, root)
}

fn marker_contents(root: &Path) -> String {
    fn find(path: &Path, output: &mut String) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "marker") {
                output.push_str(&fs::read_to_string(&path).unwrap());
            }
            if path.is_dir() {
                find(&path, output);
            }
        }
    }
    let mut output = String::new();
    find(root, &mut output);
    output
}

#[test]
fn custom_nested_ladder_freezes_and_executes_scripts_in_order() {
    let lua = r#"
local w=require('wombat')
local first=w.rung('first')
local nested=w.rung('group', { w.rung('second') })
w.ladder('test', {
  w.rungs.materialise.before,
  first,
  nested,
  w.rungs.materialise.tasks,
  w.rungs.materialise.artifacts,
  w.rungs.materialise.publish,
  w.rungs.materialise.after,
  w.rungs.deploy.before,
  w.rungs.deploy.apply,
  w.rungs.deploy.after,
})
w.script('mark.sh', { value='a' }, { name='a', at=first, schedule='always' })
w.script('mark.sh', { value='b' }, { name='b', at='group.second', schedule='always' })
"#;
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; --params=*) params=${arg#*=};; esac; done
printf '%s\n' "$params" >> "$cache/marker"
"#;
    let (temporary, root) = repository(lua, shell);
    let state = temporary.path().join("state");
    let planned = plan(BuildOptions::new(&root, "build")).unwrap();
    assert_eq!(planned.plan.format_version, 9);
    assert!(
        planned
            .plan
            .ladder
            .contains(&wombat::ladder::RungId::new("group.second").unwrap())
    );
    assert_eq!(planned.plan.scripts.len(), 2);
    let built = build(BuildOptions::new(&root, "build").with_script_state_root(&state)).unwrap();
    assert_eq!(built.manifest.format_version, 18);
    let journal = wombat::ladder::read(&built.build_dir).unwrap();
    assert_eq!(
        journal
            .actions
            .iter()
            .filter(|action| action.identity.contains("mark.sh"))
            .count(),
        2
    );
    assert_eq!(marker_contents(&state).lines().count(), 2);
}

#[test]
fn schedules_survive_clean_and_rerun_override_forces_execution() {
    let lua = "local w=require('wombat')\nw.script('mark.sh', {}, { schedule='once' })\n";
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; esac; done
printf x >> "$cache/marker"
"#;
    let (temporary, root) = repository(lua, shell);
    let state = temporary.path().join("state");
    let options = BuildOptions::new(&root, "build")
        .with_script_state_root(&state)
        .with_provider_reconciliation(true);
    build(options.clone()).unwrap();
    build(options.clone()).unwrap();
    assert_eq!(marker_contents(&state), "x");
    build(options.clone().with_clean(true)).unwrap();
    assert_eq!(marker_contents(&state), "x");
    build(options.with_rerun_scripts(true)).unwrap();
    assert_eq!(marker_contents(&state), "xx");
}

#[test]
fn compile_only_scope_policy_skips_target_and_requires_host_authorization() {
    let lua = "local w=require('wombat')\nw.target('linux/x86_64')\nw.script('mark.sh')\n";
    let (temporary, root) = repository(lua, "exit 0\n");
    let host = wombat::HostContext::fixture(wombat::TargetPlatform::minimal(
        wombat::OperatingSystemName::Macos,
        wombat::Architecture::Aarch64,
    ));
    let built = build(
        BuildOptions::new(&root, "build")
            .with_host(host.clone())
            .with_compile_only(true)
            .with_script_state_root(temporary.path().join("state")),
    )
    .unwrap();
    let journal = wombat::ladder::read(&built.build_dir).unwrap();
    assert!(journal.actions.iter().any(|action| {
        action.identity.contains("mark.sh")
            && action.status == wombat::ladder::ExecutionStatus::Skipped
            && action.reason.contains("compile-only")
    }));

    fs::write(
        root.join("wombat.lua"),
        "local w=require('wombat')\nw.target('linux/x86_64')\nw.script('mark.sh', {}, { scope='host' })\n",
    )
    .unwrap();
    let options = BuildOptions::new(&root, "build-host")
        .with_host(host)
        .with_compile_only(true)
        .with_script_state_root(temporary.path().join("state-host"));
    assert!(
        build(options.clone())
            .unwrap_err()
            .to_string()
            .contains("--allow-host-scripts")
    );
    build(options.with_allow_host_scripts(true)).unwrap();
}

#[test]
fn invalid_ladder_shapes_and_task_placement_fail_during_construction() {
    let (temporary, root) = repository(
        "local w=require('wombat')\nw.ladder('bad', { w.rungs.materialise.before })\n",
        "exit 0\n",
    );
    assert!(
        plan(BuildOptions::new(&root, "build"))
            .unwrap_err()
            .to_string()
            .contains("must contain core rung")
    );
    fs::write(
        root.join("wombat.lua"),
        "local w=require('wombat')\nw.build.task('mark.sh', {}, { at=w.rungs.materialise.publish })\n",
    )
    .unwrap();
    fs::create_dir(root.join("tasks")).unwrap();
    fs::copy(root.join("scripts/mark.sh"), root.join("tasks/mark.sh")).unwrap();
    let error = plan(BuildOptions::new(
        &root,
        temporary.path().join("other-build"),
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("materialise.artifacts"), "{error}");
}

#[test]
fn portable_post_deploy_scripts_follow_schedules_and_rerun_policy() {
    let lua = "local w=require('wombat')\nw.script('mark.sh', {}, { at=w.rungs.deploy.after, schedule='once' })\n";
    let shell = r#"for arg in "$@"; do case "$arg" in --target-root=*) target=${arg#*=};; esac; done
printf x >> "$target/deploy-script-marker"
"#;
    let (temporary, root) = repository(lua, shell);
    let built = build(
        BuildOptions::new(&root, "build")
            .with_provider_reconciliation(true)
            .with_script_state_root(temporary.path().join("material-state")),
    )
    .unwrap();
    let target = temporary.path().join("target");
    fs::create_dir(&target).unwrap();
    let state = temporary.path().join("target-state");
    let options = wombat::DeploymentOptions::new(&built.build_dir, &target)
        .with_state_root(&state)
        .with_provider_reconciliation(true);
    wombat::apply(&options, wombat::ConflictPolicy::Fail).unwrap();
    wombat::apply(&options, wombat::ConflictPolicy::Fail).unwrap();
    assert_eq!(
        fs::read_to_string(target.join("deploy-script-marker")).unwrap(),
        "x"
    );
    wombat::apply(
        &options.clone().with_rerun_scripts(true),
        wombat::ConflictPolicy::Fail,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(target.join("deploy-script-marker")).unwrap(),
        "xx"
    );

    let relocated = temporary.path().join("relocated");
    copy_product(&built.build_dir, &relocated);
    let other_target = temporary.path().join("other-target");
    fs::create_dir(&other_target).unwrap();
    wombat::apply(
        &wombat::DeploymentOptions::new(&relocated, &other_target)
            .with_state_root(temporary.path().join("other-state"))
            .with_provider_reconciliation(true),
        wombat::ConflictPolicy::Fail,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(other_target.join("deploy-script-marker")).unwrap(),
        "x"
    );
}

#[test]
fn python_helper_companions_and_embedded_lua_follow_the_script_contract() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("source");
    fs::create_dir_all(root.join("scripts/lib")).unwrap();
    fs::write(
        root.join("wombat.lua"),
        r#"local w=require('wombat')
w.script('generate.py', { greeting='hello' }, { files={ 'lib/**' }, schedule='always' })
w.script('embedded.lua', {}, { name='lua', schedule='always', env={ WOMBAT_TEST_VALUE='lua-env' } })
"#,
    )
    .unwrap();
    fs::write(root.join("scripts/lib/helper.py"), "VALUE = 'companion'\n").unwrap();
    fs::write(
        root.join("scripts/generate.py"),
        r#"from wombat import params, work, cache, source, scope, target_root
from lib.helper import VALUE
assert params == {"greeting": "hello"}
assert work.name == "work" and cache.name == "cache"
assert source.name == "payload" and scope == "target" and target_root is None
(cache / "marker").write_text(VALUE)
"#,
    )
    .unwrap();
    fs::write(
        root.join("scripts/embedded.lua"),
        "assert(os.getenv('WOMBAT_TEST_VALUE') == 'lua-env')\nlocal f=assert(io.open('marker','w')); f:write('lua'); f:close(); print('embedded')\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let built = build(BuildOptions::new(&root, "build").with_script_state_root(&state)).unwrap();
    let journal = wombat::ladder::read(&built.build_dir).unwrap();
    assert_eq!(
        journal
            .actions
            .iter()
            .filter(|action| {
                action.identity.contains("generate.py") || action.identity.contains("embedded.lua")
            })
            .count(),
        2
    );
    let contents = marker_contents(&state);
    assert!(contents.contains("companion"));
    assert!(contents.contains("lua"));
}

#[test]
fn onchange_tracks_params_payload_options_and_schedule_without_changing_identity() {
    let lua = "local w=require('wombat')\nw.script('mark.sh', { value='one' }, { schedule='onchange', files={ 'data.txt' }, revision='1' })\n";
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; esac; done
printf x >> "$cache/marker"
"#;
    let (temporary, root) = repository(lua, shell);
    fs::write(root.join("scripts/data.txt"), "one\n").unwrap();
    let state = temporary.path().join("state");
    let options = BuildOptions::new(&root, "build").with_script_state_root(&state);
    let first = build(options.clone()).unwrap();
    let identity = first.manifest.scripts[0].identity.clone();
    build(options.clone()).unwrap();
    assert_eq!(marker_contents(&state), "x");

    fs::write(root.join("scripts/data.txt"), "two\n").unwrap();
    let changed = build(options.clone()).unwrap();
    assert_eq!(changed.manifest.scripts[0].identity, identity);
    assert_eq!(marker_contents(&state), "xx");

    fs::write(
        root.join("wombat.lua"),
        "local w=require('wombat')\nw.script('mark.sh', { value='two' }, { schedule='once', files={ 'data.txt' }, revision='2' })\n",
    )
    .unwrap();
    let scheduled = build(options).unwrap();
    assert_eq!(scheduled.manifest.scripts[0].identity, identity);
    assert_eq!(marker_contents(&state), "xx");
}

#[test]
fn failures_retry_and_embedded_lua_timeouts_stop_the_ladder() {
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; esac; done
if test ! -e "$cache/tried"; then touch "$cache/tried"; exit 7; fi
printf x > "$cache/marker"
"#;
    let (temporary, root) = repository(
        "local w=require('wombat')\nw.script('mark.sh', {}, { schedule='once' })\n",
        shell,
    );
    let state = temporary.path().join("state");
    let options = BuildOptions::new(&root, "build").with_script_state_root(&state);
    assert!(
        build(options.clone())
            .unwrap_err()
            .to_string()
            .contains("exit status: 7")
    );
    build(options).unwrap();
    assert_eq!(marker_contents(&state), "x");

    fs::write(root.join("scripts/spin.lua"), "while true do end\n").unwrap();
    fs::write(
        root.join("wombat.lua"),
        "local w=require('wombat')\nw.script('spin.lua', {}, { timeout=1 })\n",
    )
    .unwrap();
    let error = build(
        BuildOptions::new(&root, temporary.path().join("timeout-build"))
            .with_script_state_root(temporary.path().join("timeout-state")),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("script timeout"), "{error}");
}

#[test]
fn mixed_actions_preserve_declaration_order_and_journal_it() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("source");
    let marker = temporary.path().join("order");
    fs::create_dir_all(root.join("tasks")).unwrap();
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(
        root.join("tasks/first.sh"),
        format!("printf t >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::write(
        root.join("scripts/second.sh"),
        format!("printf s >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::write(
        root.join("wombat.lua"),
        r#"local w=require('wombat')
local mixed=w.rung('mixed')
w.ladder('mixed-actions', {
  w.rungs.materialise.before, mixed, w.rungs.materialise.tasks,
  w.rungs.materialise.artifacts, w.rungs.materialise.publish,
  w.rungs.materialise.after, w.rungs.deploy.before,
  w.rungs.deploy.apply, w.rungs.deploy.after,
})
w.build.task('first.sh', {}, { at=mixed, cache=false })
w.script('second.sh', {}, { at=mixed })
"#,
    )
    .unwrap();
    build(BuildOptions::new(&root, "build").with_script_state_root(temporary.path().join("state")))
        .unwrap();
    assert_eq!(fs::read_to_string(marker).unwrap(), "ts");
    let journal: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("build/.wombat/execution-journal.json")).unwrap(),
    )
    .unwrap();
    let actions = journal["actions"].as_array().unwrap();
    let mixed = actions
        .iter()
        .filter(|action| action["rung"] == "mixed")
        .collect::<Vec<_>>();
    assert_eq!(mixed.len(), 2);
    assert!(mixed[0]["identity"].as_str().unwrap().contains("first.sh"));
    assert!(mixed[1]["identity"].as_str().unwrap().contains("second.sh"));
}

#[test]
fn once_state_is_locked_concurrently_and_source_relocation_is_fresh() {
    let lua = "local w=require('wombat')\nw.script('mark.sh', {}, { schedule='once' })\n";
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; esac; done
sleep 1
printf x > "$cache/marker"
"#;
    let (temporary, root) = repository(lua, shell);
    let state = temporary.path().join("state");
    let first_root = root.clone();
    let first_state = state.clone();
    let first = std::thread::spawn(move || {
        build(BuildOptions::new(&first_root, "build-a").with_script_state_root(&first_state))
            .unwrap();
    });
    let second_root = root.clone();
    let second_state = state.clone();
    let second = std::thread::spawn(move || {
        build(BuildOptions::new(&second_root, "build-b").with_script_state_root(&second_state))
            .unwrap();
    });
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(marker_contents(&state), "x");

    let relocated = temporary.path().join("relocated-source");
    copy_directory(&root, &relocated);
    build(BuildOptions::new(&relocated, "build-relocated").with_script_state_root(&state)).unwrap();
    assert_eq!(marker_contents(&state), "xx");
}

#[test]
fn a_declared_project_keeps_once_state_across_relocation() {
    let lua = "local w=require('wombat')\nw.script('mark.sh', {}, { schedule='once' })\n";
    let shell = r#"for arg in "$@"; do case "$arg" in --cache-dir=*) cache=${arg#*=};; esac; done
printf x >> "$cache/marker"
"#;
    let (temporary, root) = repository(lua, shell);
    fs::write(
        root.join("wombat.toml"),
        "format_version = 3\nproject = \"proving\"\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    build(BuildOptions::new(&root, "build").with_script_state_root(&state)).unwrap();
    assert_eq!(marker_contents(&state), "x");

    let relocated = temporary.path().join("relocated-source");
    copy_directory(&root, &relocated);
    build(BuildOptions::new(&relocated, "build-relocated").with_script_state_root(&state)).unwrap();
    assert_eq!(
        marker_contents(&state),
        "x",
        "a declared project must not restart once state when the checkout moves"
    );
}

#[test]
fn an_unusable_project_name_is_refused() {
    let (_temporary, root) = repository("local w=require('wombat')\n", "exit 0\n");
    for name in ["../escape", "with space", ""] {
        fs::write(
            root.join("wombat.toml"),
            format!("format_version = 3\nproject = \"{name}\"\n"),
        )
        .unwrap();
        let error = plan(BuildOptions::new(&root, "build"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("project must be 1 to 64 characters"),
            "{error}"
        );
    }
}

#[test]
fn embedded_lua_execution_serializes_process_global_working_directories() {
    fn embedded_repository(root: &Path) {
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(
            root.join("wombat.lua"),
            "local w=require('wombat')\nw.script('mark.lua', {}, { schedule='always' })\n",
        )
        .unwrap();
        fs::write(
            root.join("scripts/mark.lua"),
            "local f=assert(io.open('marker','w')); f:write('local'); f:close()\n",
        )
        .unwrap();
    }

    let temporary = tempdir().unwrap();
    let first_root = temporary.path().join("first");
    let second_root = temporary.path().join("second");
    embedded_repository(&first_root);
    embedded_repository(&second_root);
    let first_state = temporary.path().join("first-state");
    let second_state = temporary.path().join("second-state");
    let first = std::thread::spawn(move || {
        build(BuildOptions::new(&first_root, "build").with_script_state_root(&first_state))
            .unwrap();
        first_state
    });
    let second = std::thread::spawn(move || {
        build(BuildOptions::new(&second_root, "build").with_script_state_root(&second_state))
            .unwrap();
        second_state
    });
    assert_eq!(marker_contents(&first.join().unwrap()), "local");
    assert_eq!(marker_contents(&second.join().unwrap()), "local");
}

#[cfg(unix)]
#[test]
fn bash_direct_and_configured_runners_execute_and_timeouts_reap_children() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempdir().unwrap();
    let root = temporary.path().join("source");
    let marker = temporary.path().join("runners");
    fs::create_dir_all(root.join("scripts")).unwrap();
    for (name, value) in [("one.bash", "b"), ("two", "d"), ("three.custom", "c")] {
        // `two` is run as a direct executable rather than through an
        // interpreter, so it needs a shebang. Darwin's execve falls back to a
        // shell for shebang-less text files; Linux returns ENOEXEC.
        let shebang = if name == "two" { "#!/bin/sh\n" } else { "" };
        fs::write(
            root.join("scripts").join(name),
            format!("{shebang}printf {value} >> '{}'\n", marker.display()),
        )
        .unwrap();
    }
    fs::set_permissions(root.join("scripts/two"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        root.join("wombat.lua"),
        r#"local w=require('wombat')
w.script('one.bash')
w.script('two')
w.script('three.custom', {}, { interpreter={ command='/bin/sh' } })
"#,
    )
    .unwrap();
    build(BuildOptions::new(&root, "build").with_script_state_root(temporary.path().join("state")))
        .unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "bdc");

    let child_marker = temporary.path().join("escaped-child");
    fs::write(
        root.join("scripts/timeout.sh"),
        format!("(sleep 2; touch '{}') &\nwait\n", child_marker.display()),
    )
    .unwrap();
    fs::write(
        root.join("wombat.lua"),
        "local w=require('wombat')\nw.script('timeout.sh', {}, { timeout=1 })\n",
    )
    .unwrap();
    let error = build(
        BuildOptions::new(&root, "timeout-build")
            .with_script_state_root(temporary.path().join("timeout-state")),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("timeout"), "{error}");
    std::thread::sleep(std::time::Duration::from_millis(2200));
    assert!(
        !child_marker.exists(),
        "timed-out child escaped its process group"
    );
}

#[test]
fn cli_forwards_script_output_attributed_to_its_producer() {
    let lua = r#"
local w=require('wombat')
w.script('mark.sh', {}, { name='shell', schedule='always' })
w.script('mark.lua', {}, { name='embedded', schedule='always' })
"#;
    let shell = "printf '%s\\n' 'subprocess script spoke'\n";
    let (temporary, root) = repository(lua, shell);
    fs::write(
        root.join("scripts/mark.lua"),
        "print('embedded script spoke')\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wombat"))
        .args(["--color", "never", "-S"])
        .arg(&root)
        .args(["build", "-B"])
        .arg(temporary.path().join("build"))
        .arg("--yes")
        .env("HOME", temporary.path())
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let attributed: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("script spoke"))
        .collect();
    assert_eq!(attributed.len(), 2, "{stderr}");
    assert!(
        attributed
            .iter()
            .all(|line| line.starts_with('[') && line.contains("] ")),
        "output must carry its producer's identity: {attributed:?}"
    );
    assert!(
        attributed.iter().any(|line| line.contains("mark.sh"))
            && attributed.iter().any(|line| line.contains("mark.lua")),
        "both runners must attribute output: {attributed:?}"
    );
}

fn copy_product(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for name in ["manifest.json", "tree", "providers", "scripts"] {
        let source = source.join(name);
        if !source.exists() {
            continue;
        }
        let target = destination.join(name);
        if source.is_dir() {
            copy_directory(&source, &target);
        } else {
            fs::copy(source, target).unwrap();
        }
    }
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}
