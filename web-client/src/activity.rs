//! Shared work stays visible without interrupting the local editor or camera.
use super::*;

fn signature(entry: &Value) -> String {
    json!([entry["state"], entry["job"]]).to_string()
}

pub(super) fn apply_activity(app: &mut App, revision: u64, entries: &[Value]) {
    if app.activity_loaded && revision < app.activity_revision {
        // Revisions start over with a new server process.
        app.activity_seen.clear();
        app.latest_activity.clear();
    }
    let mut changed = Vec::new();
    for entry in entries {
        let id = number(entry, "id");
        let state = signature(entry);
        let previous = app.activity_seen.insert(id, state.clone());
        if previous.as_deref() != Some(&state) && !is_own(entry, &app.actor_id) {
            changed.push(entry);
        }
    }
    changed.sort_by_key(|entry| {
        (
            number(entry, "finished_at_ms")
                .max(number(entry, "started_at_ms"))
                .max(number(entry, "submitted_at_ms")),
            number(entry, "id"),
        )
    });
    if let Some(entry) = changed.last() {
        app.latest_activity = announcement(entry);
    }
    app.activity_seen
        .retain(|id, _| entries.iter().any(|entry| number(entry, "id") == *id));
    app.activity = entries.to_vec();
    app.activity_revision = revision;
    app.activity_loaded = true;
}

fn is_own(entry: &Value, actor_id: &str) -> bool {
    !actor_id.is_empty() && text(&entry["actor"], "id") == actor_id
}

fn actor_label(entry: &Value) -> String {
    let actor = &entry["actor"];
    let name = text(actor, "name");
    if !name.is_empty() {
        let id = text(actor, "id");
        let short_id = if id.len() > 16 {
            id.get(id.len() - 8..).unwrap_or(id)
        } else {
            id
        };
        format!(
            "{} · {}{}",
            name,
            if text(actor, "kind") == "agent" {
                "AI agent"
            } else {
                "Human"
            },
            if short_id.is_empty() {
                String::new()
            } else {
                format!(" · {short_id}")
            }
        )
    } else if text(entry, "origin") == "gui" {
        "Desktop user".into()
    } else {
        "CLI / API participant".into()
    }
}

fn state(entry: &Value) -> &str {
    let job = text(&entry["job"], "status");
    if !job.is_empty() {
        job
    } else {
        text(entry, "state")
    }
}

fn state_label(entry: &Value) -> &str {
    match state(entry) {
        "queued" => "Queued",
        "pending" => "Scheduled",
        "running" => "Running",
        "succeeded" | "complete" | "completed" => "Done",
        "cancelled" => "Stopped",
        "failed" => "Failed",
        _ => "Updated",
    }
}

fn active(entry: &Value) -> bool {
    matches!(state(entry), "queued" | "pending" | "running")
}

fn announcement(entry: &Value) -> String {
    let target = text(entry, "camera");
    let mut result = format!(
        "{}{} · {}: {}",
        actor_label(entry),
        if target.is_empty() {
            String::new()
        } else {
            format!(" · {target}")
        },
        state_label(entry),
        text(entry, "label")
    );
    if entry["job"].is_object() {
        result.push_str(&format!(
            " · {} / {} frames saved",
            number(&entry["job"], "captured"),
            number(&entry["job"], "count")
        ));
    }
    if state(entry) == "failed" {
        let error = if !text(&entry["job"], "error").is_empty() {
            text(&entry["job"], "error")
        } else {
            text(entry, "detail")
        };
        if !error.is_empty() {
            result.push_str(&format!(
                " · {}",
                error.chars().take(200).collect::<String>()
            ));
        }
    }
    result
}

/// Show every active action and the latest twenty finished actions. An older
/// queued command must never disappear behind newer work on another camera.
fn activity_html(entries: &[Value], actor_id: &str) -> String {
    let mut rows: Vec<_> = entries.iter().filter(|entry| active(entry)).rev().collect();
    rows.extend(entries.iter().rev().filter(|entry| !active(entry)).take(20));
    if rows.is_empty() {
        return "<p class=\"activity-empty muted\">No actions yet. Actions from every person and AI agent appear here.</p>".into();
    }
    rows.into_iter().map(|entry| {
        let own = is_own(entry, actor_id);
        let actor = format!("{}{}", actor_label(entry), if own { " · You" } else { "" });
        let target = text(entry, "camera");
        let target = if target.is_empty() { "Session" } else { target };
        let status = state_label(entry);
        let style = if state(entry) == "failed" { "failed" } else if active(entry) { "active" } else { "done" };
        let job = &entry["job"];
        let job_detail = if job.is_object() {
            format!("<p class=\"activity-detail\">Job #{} · {} / {} frames saved{}</p>",
                number(job, "id"), number(job, "captured"), number(job, "count"),
                if text(job, "last_file").is_empty() { String::new() } else { format!(" · {}", escape(text(job, "last_file"))) })
        } else { String::new() };
        let detail = if !text(job, "error").is_empty() { text(job, "error") } else { text(entry, "detail") };
        let detail = if detail.is_empty() { String::new() } else { format!("<p class=\"activity-detail\">{}</p>", escape(detail)) };
        format!("<article class=\"activity-entry {style}{}\"><div class=\"activity-heading\"><strong>{}</strong><span class=\"activity-state\">{status}</span></div><p class=\"activity-target\">{} · Action #{}</p><p class=\"activity-action\">{}</p>{job_detail}{detail}</article>",
            if own { " own" } else { "" }, escape(&actor), escape(target), number(entry, "id"), escape(text(entry, "label")))
    }).collect()
}

pub(super) fn render_activity(app: &App) {
    let active = app.activity.iter().filter(|entry| active(entry)).count();
    let status = if !app.connected {
        "Reconnecting"
    } else if app.activity_live {
        "Live"
    } else if app.activity_loaded {
        "Refreshing"
    } else {
        "Connecting"
    };
    set_text("activity-status", &format!("{status} · {active} active"));
    element("activity-status").set_class_name(if app.activity_live && app.connected {
        "analytics-status live"
    } else {
        "analytics-status"
    });
    set_text("latest-activity", &app.latest_activity);
    let html = activity_html(&app.activity, &app.actor_id);
    // Avoid removing keyboard focus or resetting the scroll when no action
    // changed during a camera or preview refresh.
    let container = element("activity-list");
    if container.inner_html() != html {
        container.set_inner_html(&html);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, actor: &str, state: &str) -> Value {
        json!({"id": id, "actor": {"id": actor, "name": actor, "kind": "agent"},
            "camera": "sim:0", "label": "Capture 1 PNG frame", "state": state,
            "submitted_at_ms": id, "detail": null})
    }

    #[test]
    fn other_participants_are_obvious_at_every_lifecycle_transition() {
        let mut app = App {
            actor_id: "human".into(),
            ..Default::default()
        };
        for (revision, phase) in [(1, "queued"), (2, "running"), (3, "failed")] {
            let mut action = entry(1, "Alice", phase);
            action["detail"] = json!("Camera timed out");
            apply_activity(&mut app, revision, &[action]);
            assert!(
                app.latest_activity
                    .contains("Alice · AI agent · Alice · sim:0")
            );
            assert!(app.latest_activity.contains(match phase {
                "queued" => "Queued",
                "running" => "Running",
                _ => "Failed",
            }));
        }
        assert!(app.latest_activity.contains("Camera timed out"));
        apply_activity(
            &mut app,
            4,
            &[entry(1, "Alice", "failed"), entry(2, "human", "succeeded")],
        );
        assert!(app.latest_activity.contains("Alice"));
    }

    #[test]
    fn feed_preserves_older_active_work_and_escapes_every_server_string() {
        let mut entries = vec![entry(1, "<Alice>", "queued")];
        entries.extend((2..40).map(|id| entry(id, "Bob", "succeeded")));
        entries[0]["label"] = json!("<script>alert('x')</script>");
        let html = activity_html(&entries, "Bob");
        assert!(html.contains("Action #1"));
        assert!(html.contains("&lt;Alice&gt;"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("Bob · AI agent · Bob · You"));
        assert!(!html.contains("Action #2</p>"));
    }

    #[test]
    fn scheduled_job_progress_and_failure_remain_attributed_to_the_submitter() {
        let mut app = App::default();
        let mut action = entry(1, "Alice", "succeeded");
        action["job"] = json!({"id": 2, "status": "running", "captured": 1, "count": 3});
        apply_activity(&mut app, 2, &[action.clone()]);
        assert!(app.latest_activity.contains("Running"));
        assert!(app.latest_activity.contains("1 / 3 frames saved"));
        assert!(active(&action));
        action["job"]["captured"] = json!(2);
        apply_activity(&mut app, 3, &[action.clone()]);
        assert!(app.latest_activity.contains("2 / 3 frames saved"));
        action["job"]["status"] = json!("failed");
        action["job"]["error"] = json!("Storage is full");
        apply_activity(&mut app, 4, &[action]);
        assert!(app.latest_activity.contains("Alice"));
        assert!(app.latest_activity.contains("Failed"));
        assert!(app.latest_activity.contains("Storage is full"));
        let mut complete = entry(2, "Bob", "succeeded");
        complete["job"] = json!({"id": 3, "status": "complete", "captured": 3, "count": 3});
        apply_activity(&mut app, 5, &[complete.clone()]);
        assert!(app.latest_activity.contains("Done"));
        assert!(app.latest_activity.contains("3 / 3 frames saved"));
        assert!(!active(&complete));
    }

    #[test]
    fn server_restart_does_not_hide_actions_with_reused_ids() {
        let mut app = App::default();
        apply_activity(&mut app, 20, &[entry(1, "Alice", "running")]);
        apply_activity(&mut app, 1, &[]);
        apply_activity(&mut app, 2, &[entry(1, "Bob", "running")]);
        assert!(app.latest_activity.contains("Bob"));
    }

    #[test]
    fn participants_with_the_same_display_name_are_distinguishable() {
        let mut first = entry(1, "browser-a12b34cd", "running");
        let mut second = entry(2, "browser-f98e76dc", "running");
        first["actor"]["name"] = json!("Alice");
        second["actor"]["name"] = json!("Alice");
        assert_ne!(actor_label(&first), actor_label(&second));
    }
}
