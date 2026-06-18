# CI Example

Import CI job summaries from explicit JSON files. The MVP importer recognizes basic fields such as `job_name` or `name`, `status` or `conclusion`, `url`, and `commit` or `sha`.

```bash
cargo run -p brick -- import ci --path ./exports/ci-job.json --mission <mission-id> --session <session-id>
cargo run -p brick -- import ci --path ./exports/github-jobs.json --mission <mission-id>
```

A single object, an array of job objects, or an object with a `jobs` array is accepted. Each job becomes an imported `artifact.created` test-result event; jobs with URLs also get an `external_ref.linked` event.

Example fixture:

```json
{
  "job_name": "cargo test",
  "status": "success",
  "url": "https://ci.example/jobs/123",
  "commit": "abc123"
}
```

For a complete temporary end-to-end run that includes CI import plus server push/pull, see `../../scripts/smoke_mvp.sh`.
