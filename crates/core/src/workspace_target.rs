//! Client workspace coordinates. Remote display paths are never native paths.
use serde::{Deserialize, Serialize};

/// An authenticated endpoint identity, independent of its mutable label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HostId {
    #[default]
    Local,
    Remote([u8; 32]),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectKey {
    pub host: HostId,
    pub project_id: String,
}

/// Primary workspaces resolve through the durable project row; they are not
/// worktree rows and cannot be deleted through worktree lifecycle operations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkspaceId {
    Primary,
    Worktree(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceKey {
    pub project: ProjectKey,
    pub workspace: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentKey {
    pub workspace: WorkspaceKey,
    /// Host-relative spelling, interpreted and validated by the host.
    pub path: String,
}

/// Immutable presentation metadata, with no filesystem or execution handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDescriptor {
    pub key: WorkspaceKey,
    pub project_name: String,
    pub workspace_name: String,
    pub display_path: String,
    pub host_label: String,
}

impl ProjectKey {
    /// Explicit guard at legacy local storage/process boundaries.
    pub fn local_project_id(&self) -> Option<&str> {
        matches!(self.host, HostId::Local).then_some(self.project_id.as_str())
    }

    pub fn local(project_id: impl Into<String>) -> Self {
        Self { host: HostId::Local, project_id: project_id.into() }
    }
}

impl WorkspaceKey {
    /// Explicit conversion from existing local primary/worktree identities.
    /// The storage ID itself remains unchanged.
    pub fn local(project_id: impl Into<String>, workspace_id: &str) -> Self {
        let project = ProjectKey::local(project_id);
        let workspace = if workspace_id == format!("primary:{}", project.project_id) {
            WorkspaceId::Primary
        } else {
            WorkspaceId::Worktree(workspace_id.into())
        };
        Self { project, workspace }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn colliding_project_workspace_and_document_names_are_host_qualified() {
        let documents: HashSet<_> = [HostId::Local, HostId::Remote([1; 32]), HostId::Remote([2; 32])]
            .into_iter().map(|host| DocumentKey {
                workspace: WorkspaceKey {
                    project: ProjectKey { host, project_id: "project".into() },
                    workspace: WorkspaceId::Worktree("workspace".into()),
                },
                path: "src/main.rs".into(),
            }).collect();
        assert_eq!(documents.len(), 3);
        assert_eq!(ProjectKey::local("project").local_project_id(), Some("project"));
        assert_eq!(ProjectKey { host: HostId::Remote([1; 32]), project_id: "project".into() }.local_project_id(), None);
        for document in documents {
            let json = serde_json::to_string(&document).unwrap();
            assert_eq!(serde_json::from_str::<DocumentKey>(&json).unwrap(), document);
        }
    }

    #[test]
    fn local_conversion_preserves_ids_and_distinguishes_primary_from_worktrees() {
        let primary = WorkspaceKey::local("project", "primary:project");
        assert_eq!(primary.workspace, WorkspaceId::Primary);
        let worktree = WorkspaceKey::local("project", "primary:other");
        assert_eq!(worktree.workspace, WorkspaceId::Worktree("primary:other".into()));
        assert_eq!(primary.project, worktree.project);
    }
}
