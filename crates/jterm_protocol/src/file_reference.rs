//! Source-bound references to a bounded read-only file snapshot.

use crate::terminal::Attachment;
use serde::{Deserialize, Serialize};

pub const MAX_TEXT_PREVIEW_BYTES: usize = 256 * 1024;
pub const MAX_FILE_REFERENCE_PATH_BYTES: usize = 4096;
pub const MAX_FILE_TRANSFER_BYTES: usize = 1024 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 256;

/// A file browser connects to an explicitly chosen project independently of any PTY.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshProject {
    pub host: String,
    pub root: String,
    pub view: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileVersion {
    pub bytes: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileReference {
    /// Browser-authorized bytes. The directory picker exposes only its basename.
    LocalSnapshot {
        directory: Option<String>,
        path: String,
        version: FileVersion,
    },
    SshHost {
        host: String,
        owner: Attachment,
        path: String,
        version: FileVersion,
    },
    SshProject {
        project: SshProject,
        path: String,
        version: FileVersion,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilePreview {
    pub reference: FileReference,
    pub text: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryPage {
    pub path: String,
    pub entries: Vec<DirectoryEntry>,
    pub next: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileCommand {
    Open { root: String },
    List { path: String },
    More { path: String, cursor: String },
    Read { path: String, preview: bool },
    Upload { path: String, base64: String },
    CreateDirectory { path: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileReply {
    Directory {
        page: DirectoryPage,
    },
    Contents {
        path: String,
        version: FileVersion,
        base64: String,
    },
    Uploaded {
        path: String,
        version: FileVersion,
    },
    CreatedDirectory {
        path: String,
    },
}
