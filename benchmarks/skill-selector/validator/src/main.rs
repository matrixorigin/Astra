use std::path::PathBuf;

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: skill-validator <dir>");

    let mut ok = 0usize;
    let mut failures = Vec::new();

    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&root).expect("read curated root") {
        let entry = entry.expect("dir entry");
        if !entry.file_type().expect("file type").is_dir() {
            continue;
        }
        let path = entry.path().join("SKILL.md");
        if path.is_file() {
            paths.push(path);
        }
    }

    paths.sort();

    for path in paths {
        match astra_skills::loader::load_skill_from_path(&path) {
            Ok(_) => ok += 1,
            Err(err) => failures.push(serde_json::json!({
                "path": path.display().to_string(),
                "error": err.to_string(),
            })),
        }
    }

    println!(
        "{}",
        serde_json::json!({
            "root": root.display().to_string(),
            "ok": ok,
            "failed": failures.len(),
            "failures": failures,
        })
    );
}
