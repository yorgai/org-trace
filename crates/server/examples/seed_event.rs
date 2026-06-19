//! Dev-only helper: prints one serialized TraceEvent (mission.created) tagged
//! with a repo_id and org_id, for seeding a server events.jsonl in tests.
//! Usage: cargo run -p brick-server --example seed_event -- <repo_id> <org_id> [actor_id]
//! When [actor_id] is given it sets the event's `actor.actor_id` (default "seed").

use brick_protocol::{
    ActorRef, ActorType, MissionCreatedPayload, MissionId, MissionStatus, OrgId, ProjectId,
    TraceEvent,
};

fn main() {
    let repo = std::env::args().nth(1).expect("repo arg");
    let org = std::env::args().nth(2).expect("org arg");
    let actor_id = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "seed".to_string());
    let mut event = TraceEvent::mission_created(
        ActorRef {
            actor_type: ActorType::Human,
            actor_id,
            display_name: None,
        },
        MissionId::new(),
        MissionCreatedPayload {
            project_id: ProjectId::new(),
            title: "seed".into(),
            description: None,
            status: MissionStatus::Planned,
            repo_context_id: None,
        },
    )
    .expect("build event");
    event.repo_id = Some(repo);
    event.org_id = Some(org.parse::<OrgId>().expect("org id"));
    println!("{}", serde_json::to_string(&event).expect("serialize"));
}
