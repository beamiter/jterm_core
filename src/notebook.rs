//! Explicit local notebook snapshots and supervised Markdown cell runs.

use crate::notebook_text::{cell_argv, try_parse_segments, Segment};
use crate::review_input::{contains_noncontrol_visual_spoofing, is_visual_spoofing_character};
use jterm_runtime::pipe_job::{JobHandle, JobOutcome, JobSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

pub const MAX_LOCAL_NOTEBOOK_BYTES: usize = 1024 * 1024;
const MAX_DOCUMENT_SEGMENTS: usize = 512;
const MAX_CELL_SOURCE_BYTES: usize = 256 * 1024;
const MAX_CELL_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotebookView {
    pub document_id: Uuid,
    pub name: String,
    pub sha256: String,
    pub parts: Vec<NotebookPart>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum NotebookPart {
    Text {
        text: String,
    },
    Cell {
        index: usize,
        info: String,
        source: String,
        interpreter: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellRunId(Uuid);

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CellOutcome {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Stopped {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CellRunView {
    pub document_id: Uuid,
    pub run_id: CellRunId,
    pub cell: usize,
    pub revision: u64,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub outcome: CellOutcome,
}

struct WorkingDirectory(PathBuf);

impl WorkingDirectory {
    fn create() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("jterm-notebook-{}", Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Only the name and selected bytes are admitted. The OS source path is never
/// inferred, including from a terminal path or a browser file basename.
pub struct NotebookDocument {
    view: NotebookView,
    working_dir: Arc<WorkingDirectory>,
}

impl NotebookDocument {
    pub fn open_local_snapshot(name: String, source: &str) -> io::Result<Self> {
        if name.len() > 255
            || !name.to_ascii_lowercase().ends_with(".jtnb.md")
            || name.contains(['/', '\\'])
            || name
                .chars()
                .any(|ch| ch.is_control() || is_visual_spoofing_character(ch))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Select a local .jtnb.md notebook.",
            ));
        }
        if source.len() > MAX_LOCAL_NOTEBOOK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Notebook exceeds 1 MiB.",
            ));
        }
        let segments = try_parse_segments(source).map_err(io::Error::other)?;
        if segments.len() > MAX_DOCUMENT_SEGMENTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Notebook exceeds 512 sections.",
            ));
        }
        let parts = segments
            .into_iter()
            .enumerate()
            .map(|(index, part)| match part {
                Segment::Text(text) => NotebookPart::Text { text },
                Segment::Code { lang, src } => {
                    let interpreter = admitted_cell(&lang, &src).map(|argv| argv[0].clone());
                    NotebookPart::Cell {
                        index,
                        info: lang,
                        source: src,
                        interpreter,
                    }
                }
            })
            .collect();
        Ok(Self {
            view: NotebookView {
                document_id: Uuid::new_v4(),
                name,
                sha256: crate::digest_hex::encode(&Sha256::digest(source)),
                parts,
            },
            working_dir: Arc::new(WorkingDirectory::create()?),
        })
    }

    pub fn view(&self) -> &NotebookView {
        &self.view
    }

    pub fn start(&self, cell: usize) -> io::Result<CellRun> {
        let Some(NotebookPart::Cell { info, source, .. }) = self.view.parts.get(cell) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Notebook cell is unavailable.",
            ));
        };
        let argv = admitted_cell(info, source).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Notebook cell is read-only.")
        })?;
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command.current_dir(&self.working_dir.0);
        command.env_clear();
        command.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        command.env("LC_ALL", "C.UTF-8");
        command.env("HOME", &self.working_dir.0);
        let mut input = source.as_bytes().to_vec();
        input.push(b'\n');
        Ok(CellRun {
            job: JobHandle::start(command, input, MAX_CELL_OUTPUT_BYTES)?,
            _working_dir: self.working_dir.clone(),
            document_id: self.view.document_id,
            id: CellRunId(Uuid::new_v4()),
            cell,
        })
    }
}

fn admitted_cell(info: &str, source: &str) -> Option<Vec<String>> {
    if source.trim().is_empty()
        || source.len() > MAX_CELL_SOURCE_BYTES
        || source
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        || info.chars().any(|ch| ch.is_control() && ch != '\t')
        || contains_noncontrol_visual_spoofing(source)
        || contains_noncontrol_visual_spoofing(info)
    {
        return None;
    }
    cell_argv(info, &["/bin/sh".to_owned()])
}

pub struct CellRun {
    job: JobHandle,
    // Drop joins the job before the last directory owner can remove it.
    _working_dir: Arc<WorkingDirectory>,
    document_id: Uuid,
    id: CellRunId,
    cell: usize,
}

impl CellRun {
    pub fn id(&self) -> CellRunId {
        self.id
    }

    pub fn is_running(&self) -> bool {
        self.job.is_running()
    }

    pub fn stop(&self) {
        self.job.stop();
    }

    pub fn view(&self) -> CellRunView {
        self.project(self.job.snapshot())
    }

    pub fn view_after(&self, revision: u64) -> Option<CellRunView> {
        self.job
            .snapshot_if_newer(revision)
            .map(|snapshot| self.project(snapshot))
    }

    fn project(&self, snapshot: JobSnapshot) -> CellRunView {
        CellRunView {
            document_id: self.document_id,
            run_id: self.id,
            cell: self.cell,
            revision: snapshot.revision,
            stdout: String::from_utf8_lossy(&snapshot.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&snapshot.stderr).into_owned(),
            truncated: snapshot.truncated,
            outcome: match snapshot.outcome {
                JobOutcome::Running => CellOutcome::Running,
                JobOutcome::Exited { code, signal } => CellOutcome::Exited { code, signal },
                JobOutcome::Stopped { code, signal } => CellOutcome::Stopped { code, signal },
                JobOutcome::Failed(reason) => CellOutcome::Failed { reason },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_exact_source_with_a_fresh_document_identity_and_no_execution() {
        let input = "# first\n\n```shell extra\nprintf ok > started\n```\n```javascript\nalert('no')\n```\n";
        let first = NotebookDocument::open_local_snapshot("Read.JTNB.MD".into(), input).unwrap();
        let second = NotebookDocument::open_local_snapshot("Read.JTNB.MD".into(), input).unwrap();
        assert_ne!(first.view().document_id, second.view().document_id);
        assert_eq!(first.view().sha256, second.view().sha256);
        assert!(!first.working_dir.0.join("started").exists());
        assert!(!second.working_dir.0.join("started").exists());
        assert!(
            matches!(&first.view().parts[1], NotebookPart::Cell { interpreter: Some(value), .. } if value == "/bin/sh")
        );
        assert!(first.start(0).is_err());
        assert!(first.start(2).is_err());
    }

    #[test]
    fn rejects_unscoped_or_oversize_inputs_and_disables_unsafe_fences() {
        assert!(NotebookDocument::open_local_snapshot("../x.jtnb.md".into(), "test").is_err());
        assert!(NotebookDocument::open_local_snapshot("x.jtnb.md.bak".into(), "test").is_err());
        assert!(NotebookDocument::open_local_snapshot(
            "x.jtnb.md".into(),
            &"x".repeat(MAX_LOCAL_NOTEBOOK_BYTES + 1)
        )
        .is_err());
        let input = "```bash\nprintf x\u{202e}\n```\n```\n\n```\n```bash\u{202e}\necho x\n```\n";
        let doc = NotebookDocument::open_local_snapshot("x.jtnb.md".into(), input).unwrap();
        for index in 0..3 {
            assert!(doc.start(index).is_err());
        }
    }
}
