//! Offline checks at the model-session boundary, independent of the plan
//! executor. Every HTTP request is captured by an injected transport.
use super::{
    tests::{deps, end_reply, tool_reply},
    *,
};
use crate::{
    assistant::{
        http::{Headers, HttpResponse, Transport},
        protocol::{test_image, test_vision},
        provider::Effort,
    },
    tools::tests::{FakeHost, fixture_snapshot},
};
use std::{collections::VecDeque, fs, sync::Mutex};

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}
struct Script {
    responses: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<Recorded>>,
}
impl Script {
    fn install(deps: &mut Deps, responses: Vec<(u16, Value)>) -> Arc<Self> {
        let script = Arc::new(Self {
            responses: Mutex::new(
                responses
                    .into_iter()
                    .map(|(status, body)| HttpResponse {
                        status,
                        headers: Headers::new(),
                        body: serde_json::to_vec(&body).unwrap(),
                    })
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
        });
        deps.http = Arc::new(Http::with_transport(script.clone()));
        script
    }
}
impl Transport for Script {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
        let body = if request.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).unwrap()
        };
        self.requests.lock().unwrap().push(Recorded {
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request.headers.clone(),
            body,
        });
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::Malformed))
    }
}
fn conversation(session: &ModelSession) -> Conversation {
    let mut conv = Conversation::new(
        session.provider,
        &session.model,
        prompt::system(session.mode),
        protocol::assistant_tools(session.mode),
    );
    conv.append_user("Inspect sim:0.", session.wire.as_ref())
        .unwrap();
    conv
}
fn image_conversation(session: &ModelSession) -> Conversation {
    let mut conv = conversation(session);
    conv.mark_sent().unwrap();
    let reply=session.wire.parse(&tool_reply(session.provider,"frame-1","inspect_frame",json!({"camera":"sim:0","fresh":null,"max_dimension":null,"region":null,"include_image":true})),&Headers::new()).unwrap();
    let call = reply.calls[0].clone();
    conv.push_reply(reply).unwrap();
    let mut image = test_image();
    image.width = 1280;
    image.height = 960;
    conv.append_results(
        vec![ResultRecord {
            id: call.id,
            name: call.name,
            content: json!({"camera":"sim:0","image":"attached as Image 1"}),
            is_error: false,
            images: vec![image],
        }],
        session.wire.as_ref(),
        &test_vision(session.provider, &session.model),
    )
    .unwrap();
    conv
}

#[test]
fn effort_400_drops_the_field_once_and_persists_for_the_conversation() {
    let provider = ProviderId::DeepSeek;
    let (mut deps, _, creds) = deps(provider, Vec::new());
    let script = Script::install(
        &mut deps,
        vec![
            (
                400,
                json!({"error":{"message":"reasoning_effort is unsupported for this model"}}),
            ),
            (200, end_reply(provider)),
            (200, end_reply(provider)),
        ],
    );
    let settings = ProviderSettings {
        effort: Some(Effort::Medium),
        ..Default::default()
    };
    let mut session = ModelSession::resolve(&deps, provider, settings, Mode::Plan, false).unwrap();
    let mut conv = conversation(&session);
    let reply = session
        .request(&mut conv, &deps, &AtomicBool::new(false))
        .unwrap();
    assert!(conv.effort_suppressed);
    assert_eq!(reply.stop, Stop::EndTurn);
    conv.push_reply(reply).unwrap();
    conv.append_user("Inspect again.", session.wire.as_ref())
        .unwrap();
    session
        .request(&mut conv, &deps, &AtomicBool::new(false))
        .unwrap();
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body["reasoning_effort"], "high");
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.body.get("reasoning_effort").is_none())
    );
    assert!(
        requests
            .iter()
            .all(|request| request.url == session.endpoint.url)
    );
    assert!(!format!("{requests:?}").contains("fixture-auth-never-in-body"));
    assert_eq!(creds.0.load(Ordering::Relaxed), 1);
    drop(requests);
    fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn effort_retry_has_one_attempt_and_unrelated_400s_are_returned_unchanged() {
    for (message, expected) in [
        ("effort is unavailable", 2),
        ("the tool schema is invalid", 1),
    ] {
        let provider = ProviderId::DeepSeek;
        let (mut deps, _, _) = deps(provider, Vec::new());
        let script = Script::install(
            &mut deps,
            vec![
                (400, json!({"error":{"message":message}})),
                (400, json!({"error":{"message":message}})),
                (200, end_reply(provider)),
            ],
        );
        let mut session = ModelSession::resolve(
            &deps,
            provider,
            ProviderSettings::default(),
            Mode::Plan,
            false,
        )
        .unwrap();
        let mut conv = conversation(&session);
        let error = session
            .request(&mut conv, &deps, &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProviderError>().unwrap().kind,
            ProviderErrorKind::Rejected
        );
        assert_eq!(script.requests.lock().unwrap().len(), expected);
        conv.check_invariant().unwrap();
        fs::remove_dir_all(deps.log_directory).unwrap();
    }
}

#[test]
fn missing_usage_counts_images_but_preserves_reported_input_tokens() {
    let provider = ProviderId::Anthropic;
    for reported in [false, true] {
        let (mut deps, _, _) = deps(provider, Vec::new());
        let mut body = end_reply(provider);
        if reported {
            body["usage"] = json!({"input_tokens":100});
        }
        let script = Script::install(&mut deps, vec![(200, body)]);
        let settings = ProviderSettings {
            vision: Some(VisionMode::On),
            ..Default::default()
        };
        let mut session =
            ModelSession::resolve(&deps, provider, settings, Mode::Plan, true).unwrap();
        let mut conv = image_conversation(&session);
        let reply = session
            .request(&mut conv, &deps, &AtomicBool::new(false))
            .unwrap();
        assert!(reply.usage.estimated);
        if reported {
            assert_eq!(reply.usage.input, 100);
        } else {
            let request = &script.requests.lock().unwrap()[0].body;
            let text_tokens = text_bytes(request).div_ceil(3) as u64;
            assert!(
                reply.usage.input >= text_tokens + 46 * 35,
                "missing usage must include the 1280x960 Claude image: {:?}",
                reply.usage
            );
        }
        fs::remove_dir_all(deps.log_directory).unwrap();
    }
}

#[test]
fn text_estimates_exclude_only_encoded_pixels_and_opaque_provider_artifacts() {
    let plain = json!({"data":"ordinary observation text","bytes":"another human readable value"});
    assert!(
        text_bytes(&plain) >= 27 + 28,
        "ordinary data/bytes fields are text"
    );
    let image = test_image();
    let b64 = protocol::image_base64(&image);
    for body in [
        json!({"type":"image","source":{"type":"base64","data":b64}}),
        json!({"inlineData":{"mimeType":"image/jpeg","data":b64}}),
        json!({"image":{"format":"jpeg","source":{"bytes":b64}}}),
        json!({"type":"input_image","image_url":format!("data:image/jpeg;base64,{b64}")}),
    ] {
        assert!(text_bytes(&body) < b64.len());
    }
    let short =
        json!({"encrypted_content":"opaque","signature":"opaque","thoughtSignature":"opaque"});
    let long = json!({"encrypted_content":"opaque".repeat(1000),"signature":"opaque".repeat(1000),"thoughtSignature":"opaque".repeat(1000)});
    assert_eq!(text_bytes(&short), text_bytes(&long));
}

#[test]
fn vision_metadata_uses_pinned_routes_and_skips_probes_when_images_are_disabled() {
    for (provider, model, settings, metadata, path, method) in [
        (
            ProviderId::Anthropic,
            "claude-metadata-fixture",
            ProviderSettings::default(),
            json!({"capabilities":{"image_input":{"supported":true}}}),
            "/models/claude-metadata-fixture",
            "GET",
        ),
        (
            ProviderId::OpenRouter,
            "fixture/vision-metadata",
            ProviderSettings::default(),
            json!({"data":[{"id":"fixture/vision-metadata","architecture":{"input_modalities":["text","image"]}}]}),
            "/models",
            "GET",
        ),
        (
            ProviderId::Ollama,
            "metadata-fixture",
            ProviderSettings {
                base_url: Some("http://127.0.0.1:19872/v1".into()),
                ..Default::default()
            },
            json!({"capabilities":["vision","tools"]}),
            "/api/show",
            "POST",
        ),
    ] {
        let (mut deps, _, creds) = deps(provider, Vec::new());
        let script = Script::install(&mut deps, vec![(200, metadata)]);
        let settings = ProviderSettings {
            model: Some(model.into()),
            vision: Some(VisionMode::Auto),
            ..settings
        };
        let session = ModelSession::resolve(&deps, provider, settings, Mode::Run, true).unwrap();
        assert!(session.vision.enabled);
        assert_eq!(
            session.vision.source,
            vision::VisionSource::ProviderModelsApi
        );
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.ends_with(path));
        assert_eq!(requests[0].method, method);
        if provider == ProviderId::Anthropic {
            assert!(
                requests[0]
                    .headers
                    .iter()
                    .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01")
            );
        }
        if provider == ProviderId::Ollama {
            assert_eq!(requests[0].body, json!({"model":model}));
        } else {
            assert!(requests[0].body.is_null());
        }
        assert_eq!(creds.0.load(Ordering::Relaxed), 1);
        drop(requests);
        fs::remove_dir_all(deps.log_directory).unwrap();
    }
    for (allow_images, vision) in [
        (false, VisionMode::Auto),
        (true, VisionMode::Off),
        (true, VisionMode::On),
    ] {
        let (mut deps, _, _) = deps(ProviderId::Anthropic, Vec::new());
        let script = Script::install(&mut deps, Vec::new());
        let session = ModelSession::resolve(
            &deps,
            ProviderId::Anthropic,
            ProviderSettings {
                vision: Some(vision),
                ..Default::default()
            },
            Mode::Plan,
            allow_images,
        )
        .unwrap();
        assert_eq!(
            session.vision.enabled,
            allow_images && vision == VisionMode::On
        );
        assert!(script.requests.lock().unwrap().is_empty());
        fs::remove_dir_all(deps.log_directory).unwrap();
    }
}

#[test]
fn image_rejection_rebuilds_only_the_unsent_candidate_without_changing_the_prefix() {
    let provider = ProviderId::Anthropic;
    let (mut deps, _, _) = deps(provider, Vec::new());
    deps.settings.providers.insert(
        provider,
        ProviderSettings {
            vision: Some(VisionMode::On),
            ..Default::default()
        },
    );
    let image = test_image();
    let frame = json!({"camera":"sim:0","frame":{"id":42,"seq":42,"timestamp_ns":1,"width":8,"height":8,"pixel_format":"RGB8","source":"live","age_ms":0,"waited_frames":0},"region":{"x":0,"y":0,"width":8,"height":8,"normalized":[0.0,0.0,1.0,1.0]},"exposure":{"mean":0.5},"auto_meter":null,"focus":null,"notes":[],"image":{"mime":image.mime,"width":8,"height":8,"bytes":image.data.len(),"base64":protocol::image_base64(&image)}});
    let host = Arc::new(FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame)));
    deps.host = host.clone();
    let script = Script::install(
        &mut deps,
        vec![
            (
                200,
                tool_reply(
                    provider,
                    "frame-1",
                    "inspect_frame",
                    json!({"camera":"sim:0","fresh":null,"max_dimension":null,"region":null,"include_image":true}),
                ),
            ),
            (
                400,
                json!({"error":{"message":"Image inputs are not supported for this model"}}),
            ),
            (200, end_reply(provider)),
        ],
    );
    let mut request = PlanRequest::new("Inspect sim:0");
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
    outcome.conversation.check_invariant().unwrap();
    assert_eq!(outcome.conversation.images().count(), 0);
    assert!(events.iter().any(
        |event| matches!(event,Event::Notice(message)if message.contains("can't see images"))
    ));
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(protocol::wire_image_stats(&requests[1].body).0, 1);
    assert_eq!(protocol::wire_image_stats(&requests[2].body).0, 0);
    assert_eq!(
        requests[0].body["messages"][0],
        requests[2].body["messages"][0]
    );
    assert_eq!(
        requests[1].body["messages"][1],
        requests[2].body["messages"][1]
    );
    assert_eq!(host.ops(), ["frame"]);
    drop(requests);
    fs::remove_dir_all(deps.log_directory).unwrap();
}

#[test]
fn credential_filter_checks_every_adjacent_url_without_rejecting_camera_words() {
    for prompt in [
        "Compare http://camera/live,rtsp://alice:planted-password@other/live",
        "Inspect [camera](http://camera/live)[other](rtsp://alice:planted-password@other/live)",
        "Watch http://camera/live,rtmp://host/live/abcdefgh",
        "Record https://camera/live,https://other/live?%70assphrase=planted-passphrase",
    ] {
        assert_eq!(
            prompt::validate_prompt(prompt).unwrap_err().to_string(),
            prompt::SECRET_MESSAGE,
            "{prompt}"
        );
    }
    for prompt in [
        "Record desk-cam, kiosk-2 and disk-2.",
        "Compare http://first/live,http://second/live",
        "secret 0123456789abcdef0123456789abcdef01234567",
    ] {
        assert!(prompt::validate_prompt(prompt).is_ok(), "{prompt}");
    }
}

#[test]
fn run_helper_schemas_match_the_runtime_arguments() {
    let tools = protocol::assistant_tools(Mode::Run);
    for (name, args) in [
        (
            "remember",
            json!({"notes":"Gain 2 gave mean brightness 0.46."}),
        ),
        (
            "finish_task",
            json!({"outcome":"succeeded","summary":"Captured and verified three frames."}),
        ),
    ] {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        let (_, issues) = tools::schema::check(name, &args, &tool.parameters);
        assert!(issues.is_empty(), "{name}: {issues:?}");
    }
}
