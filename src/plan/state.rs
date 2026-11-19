use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<Registration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_location() -> Self {
        let home = std::env::var_os("GRAY_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
            .unwrap_or_else(|| PathBuf::from(".gray"));
        Self::at(home.join("chatgpt").join("state.json"))
    }

    pub fn load(&self) -> State {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state: &State) -> std::io::Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        tmp.write_all(&serde_json::to_vec_pretty(state)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }

    pub fn host_id(&self) -> std::io::Result<String> {
        let mut state = self.load();
        if let Some(id) = state.host_id.clone() {
            return Ok(id);
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4().hyphenated());
        state.host_id = Some(id.clone());
        self.save(&state)?;
        Ok(id)
    }

    pub fn registration(&self) -> Option<Registration> {
        self.load().registration
    }

    pub fn set_registration(&self, registration: Registration) -> std::io::Result<()> {
        let mut state = self.load();
        state.registration = Some(registration);
        self.save(&state)
    }

    pub fn forget_registration(&self) -> std::io::Result<()> {
        let mut state = self.load();
        state.registration = None;
        self.save(&state)
    }
}
