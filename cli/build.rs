//! Embeds every file under ../skills/pgbx-skill into the binary (see src/skill.rs).
use std::fs;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut es: Vec<_> = fs::read_dir(dir).expect("read skill dir").filter_map(|e| e.ok()).map(|e| e.path()).collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            walk(&p, out);
        } else if p.file_name().map(|n| n != ".DS_Store").unwrap_or(false) {
            out.push(p);
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../skills/pgbx-skill");
    let root = root.canonicalize().expect("skills/pgbx-skill must exist next to cli/");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = vec![];
    walk(&root, &mut files);
    let mut s = String::from("pub static SKILL_FILES: &[(&str, &[u8], bool)] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        #[cfg(unix)]
        let exec = { use std::os::unix::fs::PermissionsExt; fs::metadata(f).unwrap().permissions().mode() & 0o111 != 0 };
        #[cfg(not(unix))]
        let exec = false;
        let exec = exec || rel.starts_with("scripts/") || rel.starts_with("bin/") || rel.ends_with(".sh");
        s.push_str(&format!("    ({rel:?}, include_bytes!({:?}), {exec}),\n", f.display().to_string()));
    }
    s.push_str("];\n");
    fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("skill_files.rs"), s).unwrap();
}
