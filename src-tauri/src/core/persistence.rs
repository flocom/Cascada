use crate::core::model::{Account, CopyRule};
use anyhow::Result;
use dashmap::DashMap;
use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Deserialize, Serialize, Serializer};
use std::io::ErrorKind;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub rules: Vec<CopyRule>,
}

/// Borrowed view used only for serialization — lets `save` stream accounts
/// straight out of the DashMap without cloning every entry.
pub struct SnapshotRef<'a> {
    pub accounts: &'a DashMap<String, Account>,
    pub rules: &'a [CopyRule],
}

struct AccountsView<'a>(&'a DashMap<String, Account>);

impl Serialize for AccountsView<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for kv in self.0.iter() {
            seq.serialize_element(kv.value())?;
        }
        seq.end()
    }
}

impl Serialize for SnapshotRef<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Snapshot", 2)?;
        st.serialize_field("accounts", &AccountsView(self.accounts))?;
        st.serialize_field("rules", self.rules)?;
        st.end()
    }
}

pub fn data_file() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "cascada", "Cascada")
        .ok_or_else(|| anyhow::anyhow!("no project dir"))?;
    let dir = dirs.data_dir();
    std::fs::create_dir_all(dir)?;
    Ok(dir.join("state.json"))
}

pub async fn load() -> Result<Snapshot> {
    let path = data_file()?;
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
            Ok(snap) => Ok(snap),
            Err(e) => {
                tracing::warn!("state.json parse error: {e}; starting fresh (old file kept)");
                let _ = tokio::fs::rename(&path, path.with_extension("json.bak")).await;
                Ok(Snapshot::default())
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(e) => Err(e.into()),
    }
}

/// Atomic write: temp file + rename avoids truncating state.json on crash.
/// Takes pre-serialized bytes so the caller can drop the snapshot lock
/// before the await point.
pub async fn save_bytes(bytes: Vec<u8>) -> Result<()> {
    let path = data_file()?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}
