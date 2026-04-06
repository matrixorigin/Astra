//! Skill packaging — `.astra-skill` bundle format.
//!
//! A `.astra-skill` file is a gzip-compressed tar archive containing:
//! - `manifest.json` — extracted frontmatter as JSON (for quick inspection)
//! - `SKILL.md` — the full skill markdown (frontmatter + body)
//! - `templates/` — optional template files
//! - `scripts/` — optional script files
//!
//! The filename convention is `<name>-<version>.astra-skill`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::loader;
use super::manifest::SkillManifest;
use super::traits::SkillError;

/// Metadata embedded in the bundle for quick inspection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BundleManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    /// SHA-256 hash of the SKILL.md content.
    pub skill_md_sha256: String,
}

impl BundleManifest {
    pub fn from_skill_manifest(manifest: &SkillManifest, skill_md_hash: &str) -> Self {
        Self {
            name: manifest.name.clone(),
            version: manifest.version.to_string(),
            description: manifest.description.clone(),
            author: manifest.author.clone(),
            category: manifest.category.clone(),
            tags: manifest.tags.clone(),
            skill_md_sha256: skill_md_hash.to_string(),
        }
    }
}

/// Pack a skill directory into a `.astra-skill` bundle.
///
/// Returns `(output_path, BundleManifest)` on success.
pub fn pack_skill(
    skill_dir: &Path,
    output_dir: &Path,
) -> Result<(PathBuf, BundleManifest), SkillError> {
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        return Err(SkillError::NotFound(format!(
            "SKILL.md not found in {}",
            skill_dir.display()
        )));
    }

    // Parse SKILL.md to extract manifest
    let content = std::fs::read_to_string(&skill_md_path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read SKILL.md: {e}")))?;

    let (manifest, _body) = loader::parse_skill_md(&content)?;

    // Compute SHA-256 of SKILL.md
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    let bundle_manifest = BundleManifest::from_skill_manifest(&manifest, &hash);
    let manifest_json = serde_json::to_string_pretty(&bundle_manifest)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to serialize manifest: {e}")))?;

    // Build tar.gz
    let bundle_name = format!("{}-{}.astra-skill", manifest.name, manifest.version);
    let output_path = output_dir.join(&bundle_name);

    let file = std::fs::File::create(&output_path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to create bundle file: {e}")))?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);

    // Add manifest.json
    add_bytes_to_tar(&mut tar, "manifest.json", manifest_json.as_bytes())?;

    // Add SKILL.md
    add_bytes_to_tar(&mut tar, "SKILL.md", content.as_bytes())?;

    // Add templates/ if present
    let templates_dir = skill_dir.join("templates");
    if templates_dir.is_dir() {
        add_dir_to_tar(&mut tar, &templates_dir, "templates")?;
    }

    // Add scripts/ if present
    let scripts_dir = skill_dir.join("scripts");
    if scripts_dir.is_dir() {
        add_dir_to_tar(&mut tar, &scripts_dir, "scripts")?;
    }

    tar.finish()
        .map_err(|e| SkillError::LoadFailed(format!("Failed to finalize tar: {e}")))?;

    Ok((output_path, bundle_manifest))
}

/// Unpack a `.astra-skill` bundle into a target directory.
///
/// Creates `<target_dir>/<skill_name>/` and extracts all contents.
/// Returns `(install_dir, BundleManifest)`.
pub fn unpack_skill(
    bundle_path: &Path,
    target_dir: &Path,
) -> Result<(PathBuf, BundleManifest), SkillError> {
    let file = std::fs::File::open(bundle_path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to open bundle: {e}")))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    // First pass: read manifest.json to get skill name
    let mut manifest_json = String::new();
    let mut entries_data: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in archive
        .entries()
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read tar entries: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| SkillError::LoadFailed(format!("Failed to read tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| SkillError::LoadFailed(format!("Invalid path in tar: {e}")))?
            .to_string_lossy()
            .to_string();

        // Security: reject paths that escape the archive
        if path.contains("..") || path.starts_with('/') {
            return Err(SkillError::LoadFailed(format!(
                "Suspicious path in bundle: {path}"
            )));
        }

        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| SkillError::LoadFailed(format!("Failed to read entry {path}: {e}")))?;

        if path == "manifest.json" {
            manifest_json = String::from_utf8(data.clone()).map_err(|e| {
                SkillError::LoadFailed(format!("manifest.json is not valid UTF-8: {e}"))
            })?;
        }

        entries_data.push((path, data));
    }

    if manifest_json.is_empty() {
        return Err(SkillError::LoadFailed(
            "Bundle missing manifest.json".to_string(),
        ));
    }

    let bundle_manifest: BundleManifest = serde_json::from_str(&manifest_json)
        .map_err(|e| SkillError::LoadFailed(format!("Invalid manifest.json: {e}")))?;

    // Verify SKILL.md hash
    if let Some((_, skill_md_data)) = entries_data.iter().find(|(p, _)| p == "SKILL.md") {
        let mut hasher = Sha256::new();
        hasher.update(skill_md_data);
        let actual_hash = format!("{:x}", hasher.finalize());
        if actual_hash != bundle_manifest.skill_md_sha256 {
            return Err(SkillError::LoadFailed(format!(
                "SKILL.md integrity check failed: expected {}, got {actual_hash}",
                bundle_manifest.skill_md_sha256
            )));
        }
    } else {
        return Err(SkillError::LoadFailed(
            "Bundle missing SKILL.md".to_string(),
        ));
    }

    // Extract to target_dir/<name>/
    let install_dir = target_dir.join(&bundle_manifest.name);
    std::fs::create_dir_all(&install_dir)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to create install dir: {e}")))?;

    for (path, data) in &entries_data {
        if path == "manifest.json" {
            // Write manifest.json to install dir too (for inspection)
            std::fs::write(install_dir.join("manifest.json"), data).map_err(|e| {
                SkillError::LoadFailed(format!("Failed to write manifest.json: {e}"))
            })?;
            continue;
        }

        let dest = install_dir.join(path);

        // Create parent directories for nested files (templates/foo.yaml, scripts/bar.sh)
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SkillError::LoadFailed(format!("Failed to create dir: {e}")))?;
        }

        std::fs::write(&dest, data)
            .map_err(|e| SkillError::LoadFailed(format!("Failed to write {path}: {e}")))?;
    }

    Ok((install_dir, bundle_manifest))
}

/// Read bundle manifest without fully extracting.
pub fn inspect_bundle(bundle_path: &Path) -> Result<BundleManifest, SkillError> {
    let file = std::fs::File::open(bundle_path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to open bundle: {e}")))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    for entry in archive
        .entries()
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read tar entries: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| SkillError::LoadFailed(format!("Failed to read tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| SkillError::LoadFailed(format!("Invalid path: {e}")))?
            .to_string_lossy()
            .to_string();

        if path == "manifest.json" {
            let mut data = String::new();
            entry.read_to_string(&mut data).map_err(|e| {
                SkillError::LoadFailed(format!("Failed to read manifest.json: {e}"))
            })?;
            return serde_json::from_str(&data)
                .map_err(|e| SkillError::LoadFailed(format!("Invalid manifest.json: {e}")));
        }
    }

    Err(SkillError::LoadFailed(
        "Bundle missing manifest.json".to_string(),
    ))
}

/// Pack a skill to in-memory bytes (for upload).
pub fn pack_skill_to_bytes(skill_dir: &Path) -> Result<(Vec<u8>, BundleManifest), SkillError> {
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        return Err(SkillError::NotFound(format!(
            "SKILL.md not found in {}",
            skill_dir.display()
        )));
    }

    let content = std::fs::read_to_string(&skill_md_path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read SKILL.md: {e}")))?;

    let (manifest, _body) = loader::parse_skill_md(&content)?;

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    let bundle_manifest = BundleManifest::from_skill_manifest(&manifest, &hash);
    let manifest_json = serde_json::to_string_pretty(&bundle_manifest)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to serialize manifest: {e}")))?;

    let mut buf = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);

        add_bytes_to_tar(&mut tar, "manifest.json", manifest_json.as_bytes())?;
        add_bytes_to_tar(&mut tar, "SKILL.md", content.as_bytes())?;

        let templates_dir = skill_dir.join("templates");
        if templates_dir.is_dir() {
            add_dir_to_tar(&mut tar, &templates_dir, "templates")?;
        }
        let scripts_dir = skill_dir.join("scripts");
        if scripts_dir.is_dir() {
            add_dir_to_tar(&mut tar, &scripts_dir, "scripts")?;
        }

        tar.finish()
            .map_err(|e| SkillError::LoadFailed(format!("Failed to finalize tar: {e}")))?;
    }

    Ok((buf, bundle_manifest))
}

/// Unpack from in-memory bytes (for download).
pub fn unpack_skill_from_bytes(
    data: &[u8],
    target_dir: &Path,
) -> Result<(PathBuf, BundleManifest), SkillError> {
    // Write to temp file, then unpack
    let tmp = target_dir.join(".tmp-bundle.astra-skill");
    std::fs::create_dir_all(target_dir)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to create target dir: {e}")))?;
    std::fs::write(&tmp, data)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to write temp bundle: {e}")))?;
    let result = unpack_skill(&tmp, target_dir);
    let _ = std::fs::remove_file(&tmp);
    result
}

// ── Internal helpers ────────────────────────────────────────────────────

fn add_bytes_to_tar<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> Result<(), SkillError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, path, data)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to add {path} to archive: {e}")))
}

fn add_dir_to_tar<W: Write>(
    tar: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &str,
) -> Result<(), SkillError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read {}: {e}", dir.display())))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| SkillError::LoadFailed(format!("Failed to read dir entry: {e}")))?;
        let path = entry.path();

        // Only include regular files, skip subdirectories and symlinks for safety
        if !path.is_file() {
            continue;
        }

        // Verify the canonical path doesn't escape the skill directory (symlink safety)
        if let Ok(canonical) = path.canonicalize() {
            if let Ok(dir_canonical) = dir.canonicalize() {
                if !canonical.starts_with(&dir_canonical) {
                    continue; // symlink escape — skip
                }
            }
        }

        let file_name = entry.file_name();
        let archive_path = format!("{}/{}", prefix, file_name.to_string_lossy());
        let data = std::fs::read(&path).map_err(|e| {
            SkillError::LoadFailed(format!("Failed to read {}: {e}", path.display()))
        })?;

        add_bytes_to_tar(tar, &archive_path, &data)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_skill(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: test-skill\nversion: \"1.2.3\"\ndescription: A test skill\nauthor: tester\ntags:\n  - test\n  - demo\ncategory: testing\n---\n\nThis is the skill body.\n",
        )
        .unwrap();

        // Add templates
        std::fs::create_dir_all(dir.join("templates")).unwrap();
        std::fs::write(dir.join("templates/config.yaml"), "key: value\n").unwrap();

        // Add scripts
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/setup.sh"), "#!/bin/bash\necho ok\n").unwrap();
    }

    #[test]
    fn pack_creates_bundle_file() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        create_test_skill(&skill_dir);

        let output_dir = tmp.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();

        let (path, manifest) = pack_skill(&skill_dir, &output_dir).unwrap();

        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with(".astra-skill"));
        assert_eq!(manifest.name, "test-skill");
        assert_eq!(manifest.version, "1.2.3");
        assert!(!manifest.skill_md_sha256.is_empty());
    }

    #[test]
    fn pack_and_unpack_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        create_test_skill(&skill_dir);

        let output_dir = tmp.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();

        let (bundle_path, orig_manifest) = pack_skill(&skill_dir, &output_dir).unwrap();

        // Unpack to a different directory
        let install_dir = tmp.path().join("installed");
        std::fs::create_dir_all(&install_dir).unwrap();

        let (installed, unpacked_manifest) = unpack_skill(&bundle_path, &install_dir).unwrap();

        assert_eq!(unpacked_manifest.name, orig_manifest.name);
        assert_eq!(unpacked_manifest.version, orig_manifest.version);
        assert_eq!(
            unpacked_manifest.skill_md_sha256,
            orig_manifest.skill_md_sha256
        );

        // Verify files exist
        assert!(installed.join("SKILL.md").exists());
        assert!(installed.join("manifest.json").exists());
        assert!(installed.join("templates/config.yaml").exists());
        assert!(installed.join("scripts/setup.sh").exists());

        // Verify content matches
        let orig_md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        let installed_md = std::fs::read_to_string(installed.join("SKILL.md")).unwrap();
        assert_eq!(orig_md, installed_md);
    }

    #[test]
    fn inspect_reads_manifest_without_full_extract() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        create_test_skill(&skill_dir);

        let output_dir = tmp.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();

        let (bundle_path, _) = pack_skill(&skill_dir, &output_dir).unwrap();
        let manifest = inspect_bundle(&bundle_path).unwrap();

        assert_eq!(manifest.name, "test-skill");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.author, Some("tester".to_string()));
        assert_eq!(manifest.tags, vec!["test", "demo"]);
    }

    #[test]
    fn pack_to_bytes_and_unpack_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        create_test_skill(&skill_dir);

        let (bytes, manifest) = pack_skill_to_bytes(&skill_dir).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(manifest.name, "test-skill");

        let install_dir = tmp.path().join("installed");
        let (installed, _) = unpack_skill_from_bytes(&bytes, &install_dir).unwrap();
        assert!(installed.join("SKILL.md").exists());
        assert!(installed.join("templates/config.yaml").exists());
    }

    #[test]
    fn unpack_rejects_corrupted_hash() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        create_test_skill(&skill_dir);

        let output_dir = tmp.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();

        // Pack normally
        let (bundle_path, _) = pack_skill(&skill_dir, &output_dir).unwrap();

        // Tamper: create a new bundle with wrong hash in manifest
        let file = std::fs::File::open(&bundle_path).unwrap();
        let gz = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            if path == "manifest.json" {
                // Tamper with the hash
                let mut m: BundleManifest = serde_json::from_slice(&data).unwrap();
                m.skill_md_sha256 =
                    "0000000000000000000000000000000000000000000000000000000000000000".to_string();
                data = serde_json::to_vec_pretty(&m).unwrap();
            }
            entries.push((path, data));
        }

        // Rebuild tampered bundle
        let tampered_path = output_dir.join("tampered.astra-skill");
        let file = std::fs::File::create(&tampered_path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar_builder = tar::Builder::new(gz);
        for (path, data) in &entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar_builder
                .append_data(&mut header, path, data.as_slice())
                .unwrap();
        }
        let gz = tar_builder.into_inner().unwrap();
        gz.finish().unwrap();

        // Unpack should fail
        let install_dir = tmp.path().join("bad-install");
        let result = unpack_skill(&tampered_path, &install_dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("integrity check failed"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn pack_fails_without_skill_md() {
        let tmp = TempDir::new().unwrap();
        let empty_dir = tmp.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let result = pack_skill(&empty_dir, tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn unpack_rejects_path_traversal() {
        // The `tar` crate itself rejects `..` paths during append, so we build
        // a raw tar with a traversal path using low-level header manipulation.
        let tmp = TempDir::new().unwrap();
        let malicious_path = tmp.path().join("malicious.astra-skill");

        let mut buf = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut tar_builder = tar::Builder::new(gz);

            let manifest = BundleManifest {
                name: "evil".to_string(),
                version: "1.0.0".to_string(),
                description: "evil".to_string(),
                author: None,
                category: None,
                tags: vec![],
                skill_md_sha256: "abc".to_string(),
            };
            let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
            add_bytes_to_tar(&mut tar_builder, "manifest.json", &manifest_bytes).unwrap();

            // Manually craft a header with `..` in the path
            let payload = b"pwned";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            // Use as_gnu_mut to directly set the name bytes (bypasses validation)
            let gnu = header.as_gnu_mut().unwrap();
            let name = b"../escape.txt\0";
            gnu.name[..name.len()].copy_from_slice(name);
            header.set_cksum();
            tar_builder.append(&header, &payload[..]).unwrap();

            let gz = tar_builder.into_inner().unwrap();
            gz.finish().unwrap();
        }
        std::fs::write(&malicious_path, &buf).unwrap();

        let install_dir = tmp.path().join("install");
        let result = unpack_skill(&malicious_path, &install_dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Suspicious path"),
            "unexpected error: {err_msg}"
        );
    }
}
