use super::*;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommandUsageStore {
    #[serde(default)]
    counts: HashMap<String, u32>,
}

#[derive(Debug, Default)]
struct CommandUsageCache {
    stores: HashMap<PathBuf, CommandUsageStore>,
}

static COMMAND_USAGE_CACHE: OnceLock<Mutex<CommandUsageCache>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_COMMAND_USAGE_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn command_usage_cache() -> &'static Mutex<CommandUsageCache> {
    COMMAND_USAGE_CACHE.get_or_init(|| Mutex::new(CommandUsageCache::default()))
}

fn command_usage_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_COMMAND_USAGE_DIR.with(|dir| dir.borrow().clone()) {
        return path.join("command-usage.json");
    }
    if let Ok(dir) = std::env::var("ASTRA_COMMAND_USAGE_DIR") {
        return PathBuf::from(dir).join("command-usage.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("command-usage.json")
}

fn load_store(path: &Path) -> CommandUsageStore {
    let Ok(content) = fs::read_to_string(path) else {
        return CommandUsageStore::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_store(path: &Path, store: &CommandUsageStore) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    fs::write(path, body).map_err(|e| e.to_string())
}

fn with_store<R>(f: impl FnOnce(&mut CommandUsageStore, &Path) -> R) -> R {
    let path = command_usage_path();
    let mut guard = command_usage_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let store = guard
        .stores
        .entry(path.clone())
        .or_insert_with(|| load_store(&path));
    f(store, &path)
}

pub(crate) fn usage_count(command: &str) -> u32 {
    with_store(|store, _| store.counts.get(command).copied().unwrap_or(0))
}

pub(crate) fn usage_boost(command: &str) -> usize {
    usage_count(command).min(20) as usize * 25
}

pub(crate) fn record_command_use(command: &str) -> Result<u32, String> {
    if command.trim().is_empty() {
        return Ok(0);
    }
    with_store(|store, path| {
        let entry = store.counts.entry(command.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        let count = *entry;
        save_store(path, store)?;
        Ok(count)
    })
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut guard = command_usage_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    guard.stores.clear();
}

#[cfg(test)]
pub(crate) fn set_test_dir(path: &Path) {
    TEST_COMMAND_USAGE_DIR.with(|dir| {
        *dir.borrow_mut() = Some(path.to_path_buf());
    });
}

#[cfg(test)]
pub(crate) fn clear_test_dir() {
    TEST_COMMAND_USAGE_DIR.with(|dir| {
        *dir.borrow_mut() = None;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn record_command_use_persists_counts() {
        let dir = tempfile::tempdir().unwrap();
        set_test_dir(dir.path());
        reset_for_tests();

        assert_eq!(usage_count("/help"), 0);
        assert_eq!(record_command_use("/help").unwrap(), 1);
        assert_eq!(record_command_use("/help").unwrap(), 2);
        assert_eq!(usage_count("/help"), 2);

        reset_for_tests();
        assert_eq!(usage_count("/help"), 2);
        let persisted = fs::read_to_string(dir.path().join("command-usage.json")).unwrap();
        assert!(persisted.contains("/help"));

        clear_test_dir();
        reset_for_tests();
    }
}
