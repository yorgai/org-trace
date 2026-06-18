//! Importers for explicit external trace files.
//!
//! The crate intentionally accepts caller-provided files instead of probing
//! private Cursor, Codex, or Claude Code databases. Importers normalize JSONL,
//! transcript, and CI JSON inputs into regular Brick `TraceEvent` records that
//! callers append through the local store.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use brick_protocol::{
    ActorRef, ActorType, ArtifactCreatedPayload, ArtifactId, ArtifactKind, ConfidenceLevel,
    EvidenceAvailability, ExternalRefId, ExternalRefLinkedPayload, LogRefId, MissionId, SessionId,
    SessionLogFormat, SessionLogUploadedPayload, SessionSource, SessionStartedPayload, TraceEvent,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// External trace source supported by an importer implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    /// Cursor agent/editor transcript exports supplied as explicit files.
    Cursor,
    /// Codex transcript exports supplied as explicit files.
    Codex,
    /// Claude Code transcript exports supplied as explicit files.
    ClaudeCode,
    /// Continuous integration JSON summaries supplied as explicit files.
    CI,
}

impl ImportSource {
    /// Returns the stable application/provider identifier recorded on events.
    pub fn app_id(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::CI => "ci",
        }
    }

    /// Returns a default actor type for events created from the source.
    pub fn default_actor_type(self) -> ActorType {
        match self {
            Self::CI => ActorType::System,
            Self::Cursor | Self::Codex | Self::ClaudeCode => ActorType::Agent,
        }
    }
}

/// Typed request describing explicit external files to import.
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub source: ImportSource,
    pub paths: Vec<PathBuf>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub actor: Option<ActorRef>,
    pub mission_id: Option<MissionId>,
    pub session_id: Option<SessionId>,
}

impl ImportRequest {
    /// Creates a request for one or more explicit input paths.
    pub fn new(source: ImportSource, paths: Vec<PathBuf>) -> Self {
        Self {
            source,
            paths,
            app_session_id: None,
            app_session_name: None,
            actor: None,
            mission_id: None,
            session_id: None,
        }
    }
}

/// Result of normalizing external files into Brick events.
#[derive(Debug, Clone)]
pub struct ImportResult {
    pub events: Vec<TraceEvent>,
}

impl ImportResult {
    /// Returns the number of events ready to append.
    pub fn imported_event_count(&self) -> usize {
        self.events.len()
    }
}

/// Imports explicit source files as normal Brick events.
pub fn import_traces(request: ImportRequest) -> Result<ImportResult> {
    if request.paths.is_empty() {
        return Err(anyhow!("import requires at least one --path"));
    }

    let actor = request
        .actor
        .clone()
        .unwrap_or_else(|| default_actor(request.source));
    let session_id = request.session_id.clone().unwrap_or_default();
    let mut events = Vec::new();

    if request.source != ImportSource::CI {
        events.push(imported(
            TraceEvent::session_started(
                actor.clone(),
                session_id.clone(),
                request.mission_id.clone(),
                SessionStartedPayload {
                    session_name: request.app_session_name.clone(),
                    source: SessionSource {
                        app_id: Some(request.source.app_id().to_string()),
                        app_session_id: request.app_session_id.clone(),
                        app_session_name: request.app_session_name.clone(),
                        runtime_id: None,
                    },
                    repo_context_id: None,
                },
            )
            .context("failed to build imported session.started event")?,
        ));
    }

    for path in &request.paths {
        let mut path_events = import_path(path, &request, &actor, &session_id)?;
        events.append(&mut path_events);
    }

    Ok(ImportResult { events })
}

fn import_path(
    path: &Path,
    request: &ImportRequest,
    actor: &ActorRef,
    session_id: &SessionId,
) -> Result<Vec<TraceEvent>> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);

    match (request.source, extension.as_deref()) {
        (_, Some("jsonl")) => import_jsonl(path, request, actor, session_id),
        (ImportSource::CI, Some("json")) => import_ci_json(path, request, actor, session_id),
        (_, Some("txt" | "log" | "md" | "markdown")) => {
            import_transcript(path, request, actor, session_id)
        }
        (ImportSource::CI, _) => import_ci_json(path, request, actor, session_id),
        _ => import_transcript(path, request, actor, session_id),
    }
}

fn import_jsonl(
    path: &Path,
    request: &ImportRequest,
    actor: &ActorRef,
    session_id: &SessionId,
) -> Result<Vec<TraceEvent>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read JSONL import file {}", path.display()))?;
    let mut events = Vec::new();

    for (line_index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse JSONL import file {} at line {}",
                path.display(),
                line_index + 1
            )
        })?;
        if looks_like_trace_event(&value) {
            let event: TraceEvent = serde_json::from_value(value).with_context(|| {
                format!(
                    "failed to parse Brick TraceEvent in {} at line {}",
                    path.display(),
                    line_index + 1
                )
            })?;
            events.push(imported(event));
        } else {
            events.push(simple_record_event(
                value,
                path,
                request,
                actor.clone(),
                session_id.clone(),
                line_index + 1,
            )?);
        }
    }

    Ok(events)
}

fn import_transcript(
    path: &Path,
    request: &ImportRequest,
    actor: &ActorRef,
    session_id: &SessionId,
) -> Result<Vec<TraceEvent>> {
    let metadata = file_metadata(path)?;
    let event = TraceEvent::session_log_uploaded(
        actor.clone(),
        session_id.clone(),
        SessionLogUploadedPayload {
            log_ref_id: LogRefId::new(),
            original_path: path.display().to_string(),
            format: infer_session_log_format(path),
            source: request.source.app_id().to_string(),
            sha256: metadata.sha256,
            size_bytes: metadata.size_bytes,
            storage_uri: format!("file://{}", path.display()),
            local_path: String::new(),
            external_uri: Some(format!("file://{}", path.display())),
            availability: EvidenceAvailability::LocalPointer,
            repo_context_id: None,
        },
    )
    .context("failed to build imported session.log_uploaded event")?;
    Ok(vec![imported(event)])
}

fn import_ci_json(
    path: &Path,
    request: &ImportRequest,
    actor: &ActorRef,
    _session_id: &SessionId,
) -> Result<Vec<TraceEvent>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read CI JSON import file {}", path.display()))?;
    let value: Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse CI JSON import file {}", path.display()))?;
    let records = ci_records_from_value(value)?;
    let mut events = Vec::new();

    for record in records {
        let title = record
            .name
            .clone()
            .unwrap_or_else(|| "Imported CI job".to_string());
        let body = ci_body(&record);
        let artifact_id = ArtifactId::new();
        let artifact = TraceEvent::artifact_created(
            actor.clone(),
            artifact_id.clone(),
            request.mission_id.clone(),
            request.session_id.clone(),
            ArtifactCreatedPayload {
                artifact_kind: ArtifactKind::TestResult,
                title,
                body: Some(body),
                repo_context_id: None,
            },
        )
        .context("failed to build imported CI artifact event")?;
        events.push(imported(artifact));

        if let Some(url) = record.url.filter(|url| !url.trim().is_empty()) {
            let external_ref = TraceEvent::external_ref_linked(
                actor.clone(),
                request.mission_id.clone(),
                request.session_id.clone(),
                Some(artifact_id),
                ExternalRefLinkedPayload {
                    external_ref_id: ExternalRefId::new(),
                    provider: request.source.app_id().to_string(),
                    ref_type: "ci_job".to_string(),
                    target: url,
                    repo_context_id: None,
                },
            )
            .context("failed to build imported CI external ref event")?;
            events.push(imported(external_ref));
        }
    }

    Ok(events)
}

fn simple_record_event(
    value: Value,
    path: &Path,
    request: &ImportRequest,
    actor: ActorRef,
    session_id: SessionId,
    line_number: usize,
) -> Result<TraceEvent> {
    #[derive(Deserialize)]
    struct SimpleRecord {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        kind: Option<String>,
    }

    let simple: SimpleRecord = serde_json::from_value(value.clone()).unwrap_or(SimpleRecord {
        title: None,
        message: None,
        role: None,
        kind: None,
    });
    let title = simple.title.unwrap_or_else(|| {
        simple
            .kind
            .clone()
            .map(|kind| format!("Imported {kind} record"))
            .unwrap_or_else(|| format!("Imported record from line {line_number}"))
    });
    let body = simple
        .message
        .or(simple.role.map(|role| format!("role={role}")))
        .or_else(|| serde_json::to_string(&value).ok());

    let event = TraceEvent::artifact_created(
        actor,
        ArtifactId::new(),
        request.mission_id.clone(),
        Some(session_id),
        ArtifactCreatedPayload {
            artifact_kind: ArtifactKind::Note,
            title: format!("{}: {title}", request.source.app_id()),
            body: body.map(|body| format!("source_path={}\n{body}", path.display())),
            repo_context_id: None,
        },
    )
    .context("failed to build imported JSONL simple record event")?;
    Ok(imported(event))
}

fn ci_records_from_value(value: Value) -> Result<Vec<CiRecord>> {
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .cloned()
            .map(ci_record_from_value)
            .collect::<Result<Vec<_>>>();
    }
    if let Some(jobs) = value.get("jobs").and_then(Value::as_array) {
        return jobs
            .iter()
            .cloned()
            .map(ci_record_from_value)
            .collect::<Result<Vec<_>>>();
    }
    Ok(vec![ci_record_from_value(value)?])
}

fn ci_record_from_value(value: Value) -> Result<CiRecord> {
    #[derive(Deserialize)]
    struct RawCiRecord {
        #[serde(default, alias = "job_name")]
        name: Option<String>,
        #[serde(default, alias = "conclusion")]
        status: Option<String>,
        #[serde(default, alias = "html_url", alias = "web_url")]
        url: Option<String>,
        #[serde(default, alias = "sha", alias = "head_sha")]
        commit: Option<String>,
    }

    let raw: RawCiRecord = serde_json::from_value(value.clone())
        .context("failed to deserialize CI record with basic fields")?;
    Ok(CiRecord {
        name: raw.name,
        status: raw.status,
        url: raw.url,
        commit: raw.commit,
        raw_value: value,
    })
}

#[derive(Debug, Clone)]
struct CiRecord {
    name: Option<String>,
    status: Option<String>,
    url: Option<String>,
    commit: Option<String>,
    raw_value: Value,
}

fn ci_body(record: &CiRecord) -> String {
    let mut fields = Vec::new();
    if let Some(status) = &record.status {
        fields.push(format!("status={status}"));
    }
    if let Some(commit) = &record.commit {
        fields.push(format!("commit={commit}"));
    }
    if let Some(url) = &record.url {
        fields.push(format!("url={url}"));
    }
    fields.push(format!("raw={}", record.raw_value));
    fields.join("\n")
}

fn looks_like_trace_event(value: &Value) -> bool {
    value.get("event_id").is_some()
        && value.get("event_type").is_some()
        && value.get("payload").is_some()
}

fn imported(mut event: TraceEvent) -> TraceEvent {
    event.confidence = ConfidenceLevel::Imported;
    event
}

fn default_actor(source: ImportSource) -> ActorRef {
    ActorRef {
        actor_type: source.default_actor_type(),
        actor_id: source.app_id().to_string(),
        display_name: None,
    }
}

struct FileMetadata {
    sha256: String,
    size_bytes: u64,
}

fn file_metadata(path: &Path) -> Result<FileMetadata> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read transcript import file {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(FileMetadata {
        sha256: format!("{:x}", hasher.finalize()),
        size_bytes: bytes.len() as u64,
    })
}

fn infer_session_log_format(path: &Path) -> SessionLogFormat {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "log") => SessionLogFormat::Text,
        Some("jsonl") => SessionLogFormat::Jsonl,
        Some("md" | "markdown") => SessionLogFormat::Markdown,
        _ => SessionLogFormat::Unknown,
    }
}

impl FromStr for ImportSource {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cursor" => Ok(Self::Cursor),
            "codex" => Ok(Self::Codex),
            "claude-code" | "claude_code" => Ok(Self::ClaudeCode),
            "ci" => Ok(Self::CI),
            _ => Err(anyhow!("unsupported import source: {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brick_protocol::EventType;
    use std::io::Write;

    fn temp_file(name: &str, extension: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-importer-{name}-{}.{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            extension
        ));
        let mut file = fs::File::create(&path).expect("create temp import file");
        file.write_all(contents.as_bytes())
            .expect("write temp import file");
        path
    }

    #[test]
    fn imports_simple_jsonl_records_as_notes() {
        let path = temp_file(
            "simple-jsonl",
            "jsonl",
            "{\"title\":\"User asked for tests\",\"message\":\"please add coverage\"}\n",
        );
        let result = import_traces(ImportRequest::new(ImportSource::Cursor, vec![path]))
            .expect("import jsonl");

        assert_eq!(result.imported_event_count(), 2);
        assert_eq!(result.events[0].event_type, EventType::SessionStarted);
        assert_eq!(result.events[1].event_type, EventType::ArtifactCreated);
        assert_eq!(result.events[1].confidence, ConfidenceLevel::Imported);
    }

    #[test]
    fn imports_text_transcript_as_log_upload() {
        let path = temp_file("transcript", "md", "# Transcript\nhello");
        let result = import_traces(ImportRequest::new(ImportSource::ClaudeCode, vec![path]))
            .expect("import transcript");

        assert_eq!(result.imported_event_count(), 2);
        assert_eq!(result.events[1].event_type, EventType::SessionLogUploaded);
        assert_eq!(result.events[1].payload["format"], "markdown");
    }

    #[test]
    fn imports_ci_json_as_test_result_and_external_ref() {
        let path = temp_file(
            "ci-json",
            "json",
            "{\"job_name\":\"test\",\"status\":\"success\",\"url\":\"https://ci.example/job/1\",\"commit\":\"abc123\"}",
        );
        let result = import_traces(ImportRequest::new(ImportSource::CI, vec![path]))
            .expect("import ci json");

        assert_eq!(result.imported_event_count(), 2);
        assert_eq!(result.events[0].event_type, EventType::ArtifactCreated);
        assert_eq!(result.events[0].payload["artifact_kind"], "test_result");
        assert_eq!(result.events[1].event_type, EventType::ExternalRefLinked);
    }
}
