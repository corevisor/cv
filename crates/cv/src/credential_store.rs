use anyhow::Result;
use crate::types::CredentialEntry;

/// Local credential storage using a JSON file at `~/.corevisor/credentials.json`.
pub struct CredentialStore {
    path: std::path::PathBuf,
}

impl CredentialStore {
    pub fn new() -> Result<Self> {
        let dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?
            .join(".corevisor");
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            path: dir.join("credentials.json"),
        })
    }

    pub fn get(&self, profile_id: &str, domain: &str) -> Result<Option<CredentialEntry>> {
        let entries = read_all(&self.path)?;
        Ok(entries
            .into_iter()
            .find(|e| e.profile_id == profile_id && e.domain == domain))
    }

    pub fn set(&self, entry: CredentialEntry) -> Result<()> {
        let mut entries = read_all(&self.path)?;
        entries.retain(|e| !(e.profile_id == entry.profile_id && e.domain == entry.domain));
        entries.push(entry);
        write_all(&self.path, &entries)
    }

    pub fn delete(&self, profile_id: &str, domain: &str) -> Result<()> {
        let mut entries = read_all(&self.path)?;
        entries.retain(|e| !(e.profile_id == profile_id && e.domain == domain));
        write_all(&self.path, &entries)
    }

    pub fn list(&self, profile_id: &str) -> Result<Vec<CredentialEntry>> {
        let entries = read_all(&self.path)?;
        Ok(entries
            .into_iter()
            .filter(|e| e.profile_id == profile_id)
            .collect())
    }
}

fn read_all(path: &std::path::Path) -> Result<Vec<CredentialEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = std::fs::read_to_string(path)?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    let entries: Vec<CredentialEntry> = serde_json::from_str(&contents)?;
    Ok(entries)
}

fn write_all(path: &std::path::Path, entries: &[CredentialEntry]) -> Result<()> {
    let contents = serde_json::to_string_pretty(entries)?;
    std::fs::write(path, contents)?;
    Ok(())
}
