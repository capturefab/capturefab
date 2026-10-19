//! Extra plan-loop regressions with the same offline credentials, clock,
//! context and scripted transport as the core plan tests.
use crate::assistant::{
    agent::{
        PlanRequest, plan_turn,
        tests::{deps, end_reply, tool_reply},
    },
    conversation::Turn,
    http::{Http, HttpRequest, HttpResponse, ProviderError, Transport},
    provider::ProviderId,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn photo_args() -> Value {
    json!({"camera":"sim:0","count":1,"format":null,"file_name":null,"destination":null,"tags":null,"replace_step":null})
}

#[test]
fn revoked_data_consent_withholds_retained_plan_arguments() {
    use crate::{
        assistant::http::Headers,
        tools::tests::{FakeHost, fixture_snapshot},
    };

    const PRIVATE: &str = "sensor-private-plan-marker";
    for next_provider in [ProviderId::Anthropic, ProviderId::DeepSeek] {
        let provider = ProviderId::Anthropic;
        let mut args = photo_args();
        args["tags"] = json!([{"key":"sample","value":PRIVATE}]);
        let (mut deps, script, _) = deps(
            provider,
            vec![
                tool_reply(provider, "overview", "get_overview", json!({})),
                tool_reply(provider, "photo", "capture_photos", args),
                end_reply(provider),
            ],
        );
        let mut snapshot = fixture_snapshot();
        snapshot.dataset = Some(crate::dataset::DatasetStatus {
            slot_id: Some("a".repeat(32)),
            last_error: Some(PRIVATE.into()),
            ..Default::default()
        });
        deps.host = Arc::new(FakeHost::new(snapshot));
        let mut request = PlanRequest::new("Tag a photo with the current sensor value.");
        request.allow_data = true;
        let first = plan_turn(request, &deps, &mut |_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(first.plan.steps.len(), 1);
        assert!(first.plan.steps[0].checked.as_ref().unwrap().ready());
        assert!(first.conversation.data_permission);
        assert!(
            script
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .to_string()
                .contains(PRIVATE),
            "The fixture must first disclose the data-derived plan with permission."
        );

        script.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: Headers::new(),
            body: serde_json::to_vec(&end_reply(next_provider)).unwrap(),
        });
        let mut request = PlanRequest::new("Keep the local plan and discuss a camera photo.");
        request.provider = Some(next_provider);
        request.plan = first.plan;
        request.conversation = Some(first.conversation);
        let second = plan_turn(request, &deps, &mut |_| {}, &AtomicBool::new(false)).unwrap();
        assert!(!second.conversation.data_permission);
        assert_eq!(second.plan.steps.len(), 1);
        assert_eq!(second.plan.steps[0].call.args["tags"][0]["value"], PRIVATE);
        assert!(second.plan.value().to_string().contains(PRIVATE));
        let outgoing = script
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .to_string();
        assert!(
            !outgoing.contains(PRIVATE),
            "Revoking data permission must withhold retained data-derived plan arguments from {next_provider:?}."
        );
        assert!(outgoing.contains("Plan arguments and checked details are withheld"));

        script.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: Headers::new(),
            body: serde_json::to_vec(&end_reply(next_provider)).unwrap(),
        });
        let mut request = PlanRequest::new("Discuss the full plan with data permission restored.");
        request.provider = Some(next_provider);
        request.allow_data = true;
        request.plan = second.plan;
        request.conversation = Some(second.conversation);
        let third = plan_turn(request, &deps, &mut |_| {}, &AtomicBool::new(false)).unwrap();
        assert!(third.conversation.data_permission);
        assert!(
            script
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .to_string()
                .contains(PRIVATE),
            "Restoring permission may send the retained local plan again."
        );
        std::fs::remove_dir_all(deps.log_directory).unwrap();
    }
}

struct CancelSecond {
    script: Arc<crate::assistant::agent::tests::Script>,
    cancel: Arc<AtomicBool>,
    sends: AtomicUsize,
}
impl Transport for CancelSecond {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
        let response = self.script.send(request);
        if self.sends.fetch_add(1, Ordering::Relaxed) == 1 {
            self.cancel.store(true, Ordering::Relaxed);
        }
        response
    }
}

#[test]
fn cancelled_late_reply_restores_pre_turn_plan_and_resets_wire_history() {
    let provider = ProviderId::DeepSeek;
    let (mut deps, script, _) = deps(
        provider,
        vec![
            tool_reply(provider, "call_1", "capture_photos", photo_args()),
            end_reply(provider),
            tool_reply(provider, "call_3", "capture_photos", {
                let mut args = photo_args();
                args["count"] = json!(2);
                args
            }),
            end_reply(provider),
        ],
    );
    let cancel = Arc::new(AtomicBool::new(false));
    deps.http = Arc::new(Http::with_transport(Arc::new(CancelSecond {
        script: script.clone(),
        cancel: cancel.clone(),
        sends: AtomicUsize::new(0),
    })));
    let outcome = plan_turn(
        PlanRequest::new("Capture one photo"),
        &deps,
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert!(outcome.cancelled);
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert!(outcome.plan.steps.is_empty());
    assert!(
        outcome.plan.revision > 1,
        "Restoration advances the proposal revision."
    );
    assert!(
        outcome.conversation.turns.is_empty(),
        "Discarded proposals must not survive in model tool-result history."
    );
    outcome.conversation.check_invariant().unwrap();
    cancel.store(false, Ordering::Relaxed);
    let mut request = PlanRequest::new("Capture two photos");
    request.plan = outcome.plan;
    request.conversation = Some(outcome.conversation);
    let follow_up = plan_turn(request, &deps, &mut |_| {}, &cancel).unwrap();
    assert!(!follow_up.cancelled);
    assert!(follow_up.error.is_none());
    assert_eq!(follow_up.plan.steps.len(), 1);
    assert!(
        follow_up.plan.steps[0].id.0 > 1,
        "A discarded proposal's ID is never reused."
    );
    assert_eq!(follow_up.plan.steps[0].call.args["count"], 2);
    follow_up.conversation.check_invariant().unwrap();
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn tenth_round_answers_calls_without_adding_a_change_or_breaking_replay() {
    let provider = ProviderId::Anthropic;
    let replies = (1..=10)
        .map(|round| {
            if round == 10 {
                tool_reply(provider, "call_10", "capture_photos", photo_args())
            } else {
                tool_reply(
                    provider,
                    &format!("call_{round}"),
                    "get_overview",
                    json!({}),
                )
            }
        })
        .collect();
    let (deps, script, _) = deps(provider, replies);
    let outcome = plan_turn(
        PlanRequest::new("Check the session"),
        &deps,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 10);
    assert!(outcome.plan.steps.is_empty());
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap()
            .contains("more steps than allowed")
    );
    let Turn::ToolResults { results, .. } = outcome.conversation.turns.last().unwrap() else {
        panic!("Calls need terminal tool results")
    };
    assert_eq!(
        results[0].content,
        json!({"error":"Tool round limit reached."})
    );
    assert!(results[0].is_error);
    outcome.conversation.check_invariant().unwrap();
    let records = crate::assistant::log::read_log_in(&deps.log_directory, &outcome.run_id).unwrap();
    assert_eq!(records.last().unwrap()["kind"], "end");
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn two_malformed_replies_stop_without_appending_an_unanswered_assistant_turn() {
    let provider = ProviderId::Anthropic;
    let bad = json!({"content":[],"stop_reason":"end_turn"});
    let (deps, script, _) = deps(provider, vec![bad.clone(), bad]);
    let outcome = plan_turn(
        PlanRequest::new("Check the session"),
        &deps,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    assert!(outcome.plan.steps.is_empty());
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap()
            .contains("invalid tool call twice")
    );
    assert_eq!(outcome.conversation.turns.len(), 1);
    outcome.conversation.check_invariant().unwrap();
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn unknown_tools_receive_an_error_result_and_do_not_make_plan_steps() {
    let provider = ProviderId::DeepSeek;
    let (deps, script, _) = deps(
        provider,
        vec![
            tool_reply(provider, "call_1", "invented_camera_tool", json!({})),
            end_reply(provider),
        ],
    );
    let outcome = plan_turn(
        PlanRequest::new("Check the session"),
        &deps,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(outcome.error.is_none());
    assert!(outcome.plan.steps.is_empty());
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    let result = outcome
        .conversation
        .turns
        .iter()
        .find_map(|turn| match turn {
            Turn::ToolResults { results, .. } => results.first(),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content["error"]
            .as_str()
            .unwrap()
            .starts_with("Unknown tool.")
    );
    outcome.conversation.check_invariant().unwrap();
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn rejected_image_rebuilds_only_the_pending_tool_results_before_they_are_committed() {
    use crate::{
        assistant::{
            agent::Event,
            protocol::{self, test_image},
            vision::VisionMode,
        },
        tools::tests::{FakeHost, fixture_snapshot},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let provider = ProviderId::DeepSeek;
    let args =
        json!({"camera":"sim:0","fresh":0,"max_dimension":256,"region":null,"include_image":true});
    let (mut deps, script, _) = deps(
        provider,
        vec![
            tool_reply(provider, "call_1", "inspect_frame", args),
            json!({"error":{"message":"This model cannot accept vision images."}}),
            end_reply(provider),
        ],
    );
    script.replies.lock().unwrap()[1].status = 400;
    deps.settings.providers.entry(provider).or_default().vision = Some(VisionMode::On);
    let image = test_image();
    let frame = json!({"camera":"sim:0","frame":{"id":42,"seq":42,"width":8,"height":8,"pixel_format":"Mono8","timestamp_ns":1,"age_ms":0,"source":"live","waited_frames":0},"region":{"x":0,"y":0,"width":8,"height":8,"normalized":[0.0,0.0,1.0,1.0]},"exposure":{"mean":0.5},"auto_meter":null,"focus":null,"notes":[],"image":{"mime":"image/jpeg","width":8,"height":8,"bytes":image.data.len(),"base64":STANDARD.encode(&image.data)}});
    let host = Arc::new(FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame)));
    deps.host = host.clone();
    let mut request = PlanRequest::new("Inspect the frame");
    request.allow_images = true;
    let mut events = Vec::new();
    let outcome = plan_turn(
        request,
        &deps,
        &mut |event| events.push(event),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(host.ops(), ["frame"]);
    assert!(outcome.conversation.images().next().is_none());
    outcome.conversation.check_invariant().unwrap();
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(protocol::wire_image_stats(&requests[1].1).0, 1);
    assert_eq!(protocol::wire_image_stats(&requests[2].1).0, 0);
    assert_eq!(requests[1].1["messages"][1], requests[2].1["messages"][1]);
    assert_eq!(requests[1].1["messages"][2], requests[2].1["messages"][2]);
    drop(requests);
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Event::Notice(text) if text.contains("measurements only")))
    );
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

fn follow_up_with_accepted_image_history(reject: bool) {
    use crate::{
        assistant::{
            agent::Event,
            protocol::{self, test_image},
            vision::VisionMode,
        },
        tools::{
            ToolCall,
            tests::{FakeHost, fixture_snapshot},
        },
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    const PRIVATE: &str = "private-plan-before-vision-rejection";
    let provider = ProviderId::DeepSeek;
    let args =
        json!({"camera":"sim:0","fresh":0,"max_dimension":256,"region":null,"include_image":true});
    let mut replies = vec![
        tool_reply(provider, "accepted_inspect", "inspect_frame", args),
        end_reply(provider),
    ];
    if reject {
        replies.push(json!({"error":{"message":"This model does not support image inputs."}}));
    }
    replies.push(end_reply(provider));
    let (mut deps, script, _) = deps(provider, replies);
    if reject {
        script.replies.lock().unwrap()[2].status = 400;
    }
    let settings = deps.settings.providers.entry(provider).or_default();
    settings.vision = Some(VisionMode::On);
    settings.model = Some(format!(
        "deepseek-accepted-image-followup-{}",
        if reject { "rejected" } else { "off" }
    ));
    let image = test_image();
    let frame = json!({"camera":"sim:0","frame":{"id":42,"seq":42,"width":8,"height":8,"pixel_format":"Mono8","timestamp_ns":1,"age_ms":0,"source":"live","waited_frames":0},"region":{"x":0,"y":0,"width":8,"height":8,"normalized":[0.0,0.0,1.0,1.0]},"exposure":{"mean":0.5},"notes":[],"image":{"mime":"image/jpeg","width":8,"height":8,"bytes":image.data.len(),"base64":STANDARD.encode(&image.data)}});
    let host = Arc::new(FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame)));
    deps.host = host.clone();
    let mut first = PlanRequest::new("Inspect framing and keep my planned photograph");
    first.allow_images = true;
    first.allow_data = true;
    let mut args = photo_args();
    args["tags"] = json!([{"key":"sample","value":PRIVATE}]);
    args.as_object_mut().unwrap().remove("replace_step");
    first
        .plan
        .propose(
            ToolCall {
                tool: "capture_photos".into(),
                args,
            },
            None,
        )
        .unwrap();
    let accepted = plan_turn(first, &deps, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    assert!(accepted.error.is_none());
    assert_eq!(accepted.conversation.images().count(), 1);
    let expected_id = accepted.plan.steps[0].id;
    let expected_call = accepted.plan.steps[0].call.clone();
    let expected_revision = accepted.plan.revision;
    let old_history = accepted.conversation.clone();
    if !reject {
        deps.settings.providers.get_mut(&provider).unwrap().vision = Some(VisionMode::Off);
    }
    let mut follow_up =
        PlanRequest::new("Keep the planned photograph and continue with measurements");
    follow_up.allow_images = true;
    follow_up.allow_data = true;
    follow_up.plan = accepted.plan;
    follow_up.conversation = Some(accepted.conversation);
    let mut events = Vec::new();
    let outcome = plan_turn(
        follow_up,
        &deps,
        &mut |event| events.push(event),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.plan.steps.len(), 1);
    assert_eq!(outcome.plan.steps[0].id, expected_id);
    assert_eq!(outcome.plan.steps[0].call, expected_call);
    assert_eq!(outcome.plan.revision, expected_revision);
    assert!(outcome.conversation.images().next().is_none());
    assert!(
        outcome.conversation.data_permission,
        "Rebuilding image history must retain data-permission lineage."
    );
    outcome.conversation.check_invariant().unwrap();
    old_history.check_invariant().unwrap();
    assert_eq!(old_history.images().count(), 1);
    assert_eq!(host.ops(), ["frame"]);
    let requests = script.requests.lock().unwrap();
    let retry = if reject { 3 } else { 2 };
    assert_eq!(requests.len(), retry + 1);
    assert_eq!(protocol::wire_image_stats(&requests[1].1).0, 1);
    if reject {
        assert_eq!(protocol::wire_image_stats(&requests[2].1).0, 1);
        assert!(events.iter().any(
            |event| matches!(event,Event::Notice(text) if text.contains("measurements only"))
        ));
    }
    assert_eq!(protocol::wire_image_stats(&requests[retry].1).0, 0);
    let messages = requests[retry].1["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "A fresh request retains only system and one user turn."
    );
    assert_eq!(messages[1]["role"], "user");
    let current = serde_json::to_string(&messages[1]).unwrap();
    assert!(current.contains("Keep the planned photograph and continue with measurements"));
    assert!(current.contains("capture_photos"));
    drop(requests);

    script.replies.lock().unwrap().push_back(HttpResponse {
        status: 200,
        headers: crate::assistant::http::Headers::new(),
        body: serde_json::to_vec(&end_reply(provider)).unwrap(),
    });
    let mut request = PlanRequest::new("Continue with data permission off.");
    request.plan = outcome.plan;
    request.conversation = Some(outcome.conversation);
    let revoked = plan_turn(request, &deps, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    assert!(!revoked.conversation.data_permission);
    assert_eq!(revoked.plan.steps[0].call.args["tags"][0]["value"], PRIVATE);
    assert!(
        !script
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .to_string()
            .contains(PRIVATE),
        "Data-derived plan values must stay local after consent is revoked following image-history rebuild."
    );
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn follow_up_image_rejection_restarts_sealed_history_and_keeps_the_plan() {
    follow_up_with_accepted_image_history(true);
}

#[test]
fn turning_vision_off_drops_old_images_before_the_first_follow_up_request() {
    follow_up_with_accepted_image_history(false);
}

#[test]
fn oversized_provider_responses_are_not_misreported_as_local_request_limits() {
    let (deps, script, _) = deps(ProviderId::DeepSeek, Vec::new());
    script.replies.lock().unwrap().push_back(HttpResponse {
        status: 200,
        headers: Default::default(),
        body: vec![b' '; crate::assistant::http::MAX_BODY_BYTES + 1],
    });
    let error = plan_turn(
        PlanRequest::new("Read the camera overview"),
        &deps,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ProviderError>().unwrap().kind,
        crate::assistant::http::ProviderErrorKind::CutOff
    );
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    let runs = crate::assistant::log::runs_in(&deps.log_directory).unwrap();
    assert_eq!(runs.len(), 1);
    let records = crate::assistant::log::read_log_in(&deps.log_directory, &runs[0].run_id).unwrap();
    assert_eq!(records.last().unwrap()["kind"], "end");
    assert_eq!(records.last().unwrap()["outcome"], "failed");
    std::fs::remove_dir_all(deps.log_directory).unwrap();
}
