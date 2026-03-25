use super::*;

impl ToolExecutor {
    pub(crate) fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root.join(p)
        }
    }

    pub(crate) fn read_file(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => self.resolve(p),
            None => return "Error: missing 'path'".to_string(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: {e}"),
        };
        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        if start.is_none() && end.is_none() {
            if content.len() > 50_000 {
                let mut out = content[..50_000].to_string();
                out.push_str("\n[truncated]");
                return out;
            }
            return content;
        }
        let lines: Vec<&str> = content.lines().collect();
        let s = start.unwrap_or(1).saturating_sub(1);
        let e = end.unwrap_or(lines.len()).min(lines.len());
        lines[s..e].join("\n")
    }

    pub(crate) fn write_file(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => self.resolve(p),
            None => return "Error: missing 'path'".to_string(),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return "Error: missing 'content'".to_string(),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::write(&path, content) {
            Ok(_) => format!("Written {} bytes to {}", content.len(), path.display()),
            Err(e) => format!("Error: {e}"),
        }
    }

    pub(crate) fn str_replace(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => self.resolve(p),
            None => return "Error: missing 'path'".to_string(),
        };
        let old_str = match args.get("old_str").and_then(Value::as_str) {
            Some(s) => s,
            None => return "Error: missing 'old_str'".to_string(),
        };
        let new_str = match args.get("new_str").and_then(Value::as_str) {
            Some(s) => s,
            None => return "Error: missing 'new_str'".to_string(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };
        let count = content.matches(old_str).count();
        if count == 0 {
            return "Error: old_str not found in file".to_string();
        }
        if count > 1 {
            return format!("Error: old_str found {count} times — must be unique");
        }
        let new_content = content.replacen(old_str, new_str, 1);
        match fs::write(&path, &new_content) {
            Ok(_) => "Replaced successfully".to_string(),
            Err(e) => format!("Error writing file: {e}"),
        }
    }

    pub(crate) fn list_dir(&self, args: &Value) -> String {
        let dir = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());
        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
        let mut out = String::new();
        self.list_dir_recursive(&dir, &dir, depth, 0, &mut out);
        if out.is_empty() {
            "(empty)".to_string()
        } else {
            out
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    pub(crate) fn list_dir_recursive(
        &self,
        base: &Path,
        dir: &Path,
        max_depth: usize,
        cur: usize,
        out: &mut String,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let indent = "  ".repeat(cur);
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip hidden and common noise dirs
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
            {
                continue;
            }
            let ft = entry.file_type().ok();
            let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
            out.push_str(&format!(
                "{indent}{}{}\n",
                name,
                if is_dir { "/" } else { "" }
            ));
            if is_dir && cur < max_depth.saturating_sub(1) {
                self.list_dir_recursive(base, &entry.path(), max_depth, cur + 1, out);
            }
        }
    }
}
