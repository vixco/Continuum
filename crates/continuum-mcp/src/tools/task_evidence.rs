//! Durable task plans and agent evidence records.

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TaskPlanWriteRequest {
    pub id: Option<String>,
    pub title: String,
    pub status: String,
    pub steps: Vec<TaskStep>,
    pub project: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TaskStep {
    pub title: String,
    pub status: String,
    pub evidence_ids: Vec<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RecordIdRequest {
    pub id: String,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RecordListRequest {
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EvidenceWriteRequest {
    pub task_id: Option<String>,
    pub kind: String,
    pub summary: String,
    pub content: Option<String>,
    pub source_reference: Option<String>,
    pub success: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TaskPlanRecord {
    pub id: String,
    pub title: String,
    pub status: String,
    pub steps: Vec<TaskStep>,
    pub project: Option<String>,
    pub updated_at: String,
}
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EvidenceRecord {
    pub id: String,
    pub task_id: Option<String>,
    pub kind: String,
    pub summary: String,
    pub content: Option<String>,
    pub source_reference: Option<String>,
    pub success: bool,
    pub created_at: String,
}

pub fn write_task(data: &Path, req: &TaskPlanWriteRequest) -> Result<TaskPlanRecord, RecordError> {
    validate_status(&req.status)?;
    if req.title.trim().is_empty() || req.title.len() > 500 || req.steps.len() > 100 {
        return Err(RecordError::Invalid);
    }
    for step in &req.steps {
        validate_status(&step.status)?;
        if step.title.is_empty() || step.title.len() > 500 || step.evidence_ids.len() > 100 {
            return Err(RecordError::Invalid);
        }
    }
    let id = req.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_id(&id)?;
    let record = TaskPlanRecord {
        id: id.clone(),
        title: req.title.clone(),
        status: req.status.clone(),
        steps: req.steps.clone(),
        project: req
            .project
            .as_deref()
            .map(|v| v.chars().take(500).collect()),
        updated_at: Utc::now().to_rfc3339(),
    };
    atomic_json(&data.join("task-plans").join(format!("{id}.json")), &record)?;
    Ok(record)
}
pub fn get_task(data: &Path, id: &str) -> Result<TaskPlanRecord, RecordError> {
    validate_id(id)?;
    read_json(&data.join("task-plans").join(format!("{id}.json")))
}
pub fn list_tasks(data: &Path, limit: Option<u32>) -> Result<Vec<TaskPlanRecord>, RecordError> {
    list_json(&data.join("task-plans"), limit)
}
pub fn write_evidence(
    data: &Path,
    req: &EvidenceWriteRequest,
) -> Result<EvidenceRecord, RecordError> {
    if req.kind.is_empty()
        || req.kind.len() > 100
        || req.summary.is_empty()
        || req.summary.len() > 2000
        || req.content.as_ref().is_some_and(|v| v.len() > 1024 * 1024)
    {
        return Err(RecordError::Invalid);
    }
    let id = Uuid::new_v4().to_string();
    let record = EvidenceRecord {
        id: id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        summary: req.summary.clone(),
        content: req.content.clone(),
        source_reference: req
            .source_reference
            .as_deref()
            .map(|v| v.chars().take(1000).collect()),
        success: req.success,
        created_at: Utc::now().to_rfc3339(),
    };
    atomic_json(
        &data
            .join("evidence")
            .join("agent")
            .join(format!("{id}.json")),
        &record,
    )?;
    Ok(record)
}
pub fn list_evidence(data: &Path, limit: Option<u32>) -> Result<Vec<EvidenceRecord>, RecordError> {
    list_json(&data.join("evidence").join("agent"), limit)
}
fn validate_status(v: &str) -> Result<(), RecordError> {
    if [
        "pending",
        "in_progress",
        "completed",
        "blocked",
        "cancelled",
    ]
    .contains(&v)
    {
        Ok(())
    } else {
        Err(RecordError::Invalid)
    }
}
fn validate_id(v: &str) -> Result<(), RecordError> {
    if !v.is_empty()
        && v.len() <= 100
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(RecordError::Invalid)
    }
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RecordError> {
    let parent = path.parent().ok_or(RecordError::Invalid)?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, RecordError> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(RecordError::Invalid);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn list_json<T: for<'de> Deserialize<'de>>(
    dir: &Path,
    limit: Option<u32>,
) -> Result<Vec<T>, RecordError> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut paths = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect::<Vec<PathBuf>>();
    paths.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));
    paths
        .into_iter()
        .take(limit.unwrap_or(50).clamp(1, 100) as usize)
        .map(|p| read_json(&p))
        .collect()
}
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("invalid or over-limit record")]
    Invalid,
    #[error("record storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("record JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_and_evidence_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let task = write_task(
            d.path(),
            &TaskPlanWriteRequest {
                id: None,
                title: "Ship".into(),
                status: "in_progress".into(),
                steps: vec![],
                project: None,
            },
        )
        .unwrap();
        assert_eq!(get_task(d.path(), &task.id).unwrap().title, "Ship");
        let evidence = write_evidence(
            d.path(),
            &EvidenceWriteRequest {
                task_id: Some(task.id),
                kind: "test".into(),
                summary: "passed".into(),
                content: None,
                source_reference: None,
                success: true,
            },
        )
        .unwrap();
        assert_eq!(list_evidence(d.path(), Some(5)).unwrap()[0].id, evidence.id);
    }
}
