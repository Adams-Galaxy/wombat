use std::fs;

use wombat::{BuildOptions, add, build, initialize};

#[test]
fn add_imports_a_complete_directory_as_one_generated_declaration() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let target = temp.path().join("target");
    fs::create_dir_all(target.join(".config/nvim/lua")).unwrap();
    fs::write(target.join(".config/nvim/init.lua"), "return true\n").unwrap();
    fs::write(target.join(".config/nvim/lua/plugin.lua"), "return {}\n").unwrap();
    initialize(&repo).unwrap();
    add(&repo, &target, &target.join(".config/nvim")).unwrap();
    build(BuildOptions::new(&repo, repo.join("build"))).unwrap();
    assert!(repo.join("src/dot_config/nvim/lua/plugin.lua").is_file());
    assert!(repo.join("build/tree/.config/nvim/init.lua").is_file());
}
