use super::*;
use serde_json::json;
fn event(value: Value) -> String {
    format!("{}\n", value)
}
fn thread() -> String {
    event(json!({"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}))
}
fn started() -> String {
    event(json!({"type":"turn.started"}))
}
fn item(id: &str, kind: &str, text: &str) -> String {
    event(json!({"type":"item.completed","item":{"id":id,"type":kind,"text":text}}))
}
fn usage() -> Value {
    json!({"input_tokens":20,"cached_input_tokens":3,"output_tokens":4,"reasoning_output_tokens":1})
}
fn completed() -> String {
    event(json!({"type":"turn.completed","usage":usage()}))
}
fn prefix() -> String {
    thread() + &started()
}
fn valid(message: &str) -> String {
    prefix() + &item("item_1", "agent_message", message) + &completed()
}
fn decode(text: &str) -> Result<ParsedTurn, ProtocolError> {
    let mut decoder = CodexJsonlDecoder::new();
    decoder.feed(text.as_bytes())?;
    decoder.finish_stdout()
}
fn error(text: &str) -> ProtocolError {
    decode(text).err().expect("stream must fail closed")
}

#[test]
fn pinned_lifecycle_parses_provider_thread_ids_without_fake_uuid_v4_coercion() {
    let result = decode(&valid("ACK")).unwrap();
    assert_eq!(result.thread_id, "0199a213-81c0-7800-8aa1-bbab2a035a53");
    assert_eq!(result.message, "ACK");
    assert_eq!(result.usage.input_tokens, 20);
    assert_eq!(result.usage.cache_write_input_tokens, None);
    assert_eq!(result.events_seen, 4);
    assert_eq!(result.reasoning_items_discarded, 0);
}
#[test]
fn every_fragment_size_preserves_split_utf8_and_final_line_without_lf() {
    let text = valid("🧪ACK");
    for chunk in 1..=text.len() {
        let mut decoder = CodexJsonlDecoder::new();
        for piece in text.as_bytes().chunks(chunk) {
            decoder.feed(piece).unwrap();
        }
        assert_eq!(decoder.finish_stdout().unwrap().message, "🧪ACK");
    }
    assert_eq!(
        decode(text.trim_end_matches('\n')).unwrap().message,
        "🧪ACK"
    );
    assert_eq!(
        decode(&text.replace('\n', "\r\n")).unwrap().message,
        "🧪ACK"
    );
}
#[test]
fn reasoning_is_discarded_not_returned_and_optional_fields_are_tolerated() {
    let text = prefix()
        + &item("thought_1", "reasoning", "PRIVATE_REASONING_SENTINEL")
        + &item("answer_1", "agent_message", "ACK")
        + &event(
            json!({"type":"turn.completed","usage":usage(),"optional_new_field":{"note":"ignored"}}),
        );
    let result = decode(&text).unwrap();
    assert_eq!(result.message, "ACK");
    assert_eq!(result.reasoning_items_discarded, 1);
    // ParsedTurn intentionally has no raw events/reasoning/Debug/Serialize API.
    assert_eq!(result.events_seen, 5);
}
#[test]
fn invalid_lifecycle_and_trailing_events_never_produce_a_turn() {
    let cases = [
        started(),
        thread() + &thread(),
        thread() + &completed(),
        prefix() + &completed(),
        valid("ACK") + &thread(),
        valid("ACK") + "\n",
    ];
    for text in cases {
        assert_eq!(error(&text), ProtocolError::InvalidLifecycle);
    }
}
#[test]
fn missing_terminal_and_truncated_json_are_not_completion() {
    assert_eq!(error(""), ProtocolError::Incomplete);
    assert_eq!(error(&prefix()), ProtocolError::Incomplete);
    assert_eq!(
        error(&(prefix() + &item("i", "agent_message", "ACK"))),
        ProtocolError::Incomplete
    );
    let complete = valid("ACK");
    for end in (complete.len() - completed().len() + 1)..complete.len() - 2 {
        assert!(decode(&complete[..end]).is_err());
    }
}
#[test]
fn tools_plans_started_items_and_unverified_aliases_are_forbidden() {
    for kind in [
        "command_execution",
        "file_change",
        "mcp_tool_call",
        "collab_tool_call",
        "web_search",
        "todo_list",
        "error",
        "future_tool",
    ] {
        assert_eq!(
            error(&(thread() + &item("pre", kind, "PRIVATE_PRE_TURN_ITEM"))),
            ProtocolError::ForbiddenActivity
        );
        assert_eq!(
            error(&(prefix() + &item("i", kind, "PRIVATE_TOOL_BODY"))),
            ProtocolError::ForbiddenActivity
        );
        assert_eq!(
            error(
                &(prefix()
                    + &item("msg", "agent_message", "ACK")
                    + &item("tool", kind, "PRIVATE_TOOL_BODY"))
            ),
            ProtocolError::ForbiddenActivity
        );
    }
    for name in [
        "item.started",
        "item.updated",
        "item.agent_message.completed",
        "future.event",
        "approval.requested",
    ] {
        assert_eq!(
            error(
                &(prefix()
                    + &event(
                        json!({"type":name,"item":{"id":"i","type":"reasoning","text":"PRIVATE"}})
                    ))
            ),
            ProtocolError::ForbiddenActivity
        );
    }
}
#[test]
fn failures_expose_only_codes_and_poison_decoder_permanently() {
    for name in ["error", "turn.failed"] {
        let mut decoder = CodexJsonlDecoder::new();
        decoder.feed(prefix().as_bytes()).unwrap();
        let e=decoder.feed(event(json!({"type":name,"message":"C:\\PRIVATE_PATH\\token","error":{"message":"PRIVATE_SECRET"}})).as_bytes()).unwrap_err();
        assert_eq!(e, ProtocolError::ProviderFailure);
        assert_eq!(e.to_string(), "codex_turn_failed");
        assert!(!format!("{e:?}").contains("PRIVATE"));
        assert_eq!(decoder.feed(valid("ACK").as_bytes()).unwrap_err(), e);
        assert_eq!(decoder.finish_stdout().err().unwrap(), e);
    }
}
#[test]
fn utf8_json_duplicates_and_bad_shapes_fail_closed() {
    for raw in [
        "not-json\n",
        "null\n",
        "{}\n",
        "{\"type\":3}\n",
        "{\"type\":\"thread.started\",\"thread_id\":true}\n",
        "{\"type\":\"thread.started\",\"thread_id\":\"t\",\"type\":\"turn.started\"}\n",
        "{\"type\":\"thread.started\",\"thread_id\":\"t\",\"extra\":{\"x\":1,\"x\":2}}\n",
    ] {
        assert!(decode(raw).is_err());
    }
    let mut decoder = CodexJsonlDecoder::new();
    assert_eq!(
        decoder.feed(&[0xff, b'\n']).unwrap_err(),
        ProtocolError::InvalidJson
    );
    assert_eq!(
        decoder.finish_stdout().err().unwrap(),
        ProtocolError::InvalidJson
    );
}
#[test]
fn provider_identifiers_and_completed_item_ids_are_bounded_and_unique() {
    for id in [
        "".to_owned(),
        "x".repeat(257),
        "bad id".to_owned(),
        "bad\nidentity".to_owned(),
    ] {
        assert_eq!(
            error(&event(json!({"type":"thread.started","thread_id":id}))),
            ProtocolError::InvalidShape
        );
    }
    assert_eq!(
        error(
            &(prefix()
                + &item("same", "reasoning", "private")
                + &item("same", "agent_message", "ACK"))
        ),
        ProtocolError::InvalidLifecycle
    );
}
#[test]
fn usage_counters_preserve_zero_and_missing_optional_values_without_estimates() {
    let mut count = usage();
    count["input_tokens"] = 0.into();
    count["cache_write_input_tokens"] = 0.into();
    let zero_turn = decode(
        &(prefix()
            + &item("i", "agent_message", "ACK")
            + &event(json!({"type":"turn.completed","usage":count}))),
    )
    .unwrap();
    assert_eq!(zero_turn.usage.input_tokens, 0);
    assert_eq!(zero_turn.usage.cache_write_input_tokens, Some(0));
    count["cache_write_input_tokens"] = 7.into();
    let turn = decode(
        &(prefix()
            + &item("i", "agent_message", "ACK")
            + &event(json!({"type":"turn.completed","usage":count}))),
    )
    .unwrap();
    assert_eq!(turn.usage.input_tokens, 0);
    assert_eq!(turn.usage.cache_write_input_tokens, Some(7));
    for bad in [
        json!(-1),
        json!(1.5),
        json!(null),
        json!(MAX_SAFE_COUNTER + 1),
    ] {
        let mut count = usage();
        count["input_tokens"] = bad;
        assert_eq!(
            error(
                &(prefix()
                    + &item("i", "agent_message", "ACK")
                    + &event(json!({"type":"turn.completed","usage":count})))
            ),
            ProtocolError::InvalidShape
        );
    }
    let mut count = usage();
    count
        .as_object_mut()
        .unwrap()
        .remove("reasoning_output_tokens");
    assert_eq!(
        error(
            &(prefix()
                + &item("i", "agent_message", "ACK")
                + &event(json!({"type":"turn.completed","usage":count})))
        ),
        ProtocolError::InvalidShape
    );
}
#[test]
fn assistant_message_limit_is_utf8_bytes_and_inclusive() {
    assert_eq!(
        decode(&valid(&"x".repeat(MAX_MESSAGE_BYTES)))
            .unwrap()
            .message
            .len(),
        MAX_MESSAGE_BYTES
    );
    assert_eq!(
        decode(&valid(&"🧪".repeat(MAX_MESSAGE_BYTES / 4)))
            .unwrap()
            .message
            .len(),
        MAX_MESSAGE_BYTES
    );
    assert_eq!(
        error(&valid(&"x".repeat(MAX_MESSAGE_BYTES + 1))),
        ProtocolError::LimitExceeded
    );
    assert_eq!(error(&valid("")), ProtocolError::LimitExceeded);
}
#[test]
fn line_byte_limit_is_inclusive_and_checked_before_unbounded_growth() {
    let mut decoder = CodexJsonlDecoder::new();
    decoder.feed(&vec![b' '; MAX_LINE_BYTES]).unwrap();
    assert_eq!(
        decoder.feed(b" ").unwrap_err(),
        ProtocolError::LimitExceeded
    );
    let base = event(json!({"type":"thread.started","thread_id":"t","padding":""}));
    let line = event(
        json!({"type":"thread.started","thread_id":"t","padding":"x".repeat(MAX_LINE_BYTES+1-base.len())}),
    );
    assert_eq!(line.len() - 1, MAX_LINE_BYTES);
    let mut decoder = CodexJsonlDecoder::new();
    decoder.feed(line.as_bytes()).unwrap();
}
#[test]
fn total_stdout_and_event_limits_are_inclusive() {
    // 4 lifecycle records +60 discarded reasoning records is the real64limit.
    let mut exact = prefix();
    for i in 0..60 {
        exact += &item(&format!("r{i}"), "reasoning", "discard");
    }
    exact += &item("answer", "agent_message", "ACK");
    exact += &completed();
    assert_eq!(decode(&exact).unwrap().events_seen, MAX_EVENTS);
    let overflow = prefix()
        + &(0..61)
            .map(|i| item(&format!("r{i}"), "reasoning", "discard"))
            .collect::<String>()
        + &item("answer", "agent_message", "ACK")
        + &completed();
    assert_eq!(error(&overflow), ProtocolError::LimitExceeded);
    let mut decoder = CodexJsonlDecoder::new();
    assert_eq!(
        decoder.feed(&vec![b' '; MAX_STDOUT_BYTES + 1]).unwrap_err(),
        ProtocolError::LimitExceeded
    );
    let empty_records = (0..4)
        .map(|i| item(&format!("r{i}"), "reasoning", ""))
        .collect::<String>();
    let remaining = MAX_STDOUT_BYTES
        - (prefix().len()
            + empty_records.len()
            + item("answer", "agent_message", "ACK").len()
            + completed().len());
    let mut exact_bytes = prefix();
    for i in 0..4 {
        let size = remaining / 4 + usize::from(i < remaining % 4);
        exact_bytes += &item(&format!("r{i}"), "reasoning", &"x".repeat(size));
    }
    exact_bytes += &item("answer", "agent_message", "ACK");
    exact_bytes += &completed();
    assert_eq!(exact_bytes.len(), MAX_STDOUT_BYTES);
    assert_eq!(decode(&exact_bytes).unwrap().message, "ACK");
    assert_eq!(error(&(exact_bytes + " ")), ProtocolError::LimitExceeded);
    // Multiple legal-sized records eventually exceed the cumulative budget even
    // though every individual line fits and event count remains small.
    let mut large = prefix();
    for i in 0..5 {
        large += &item(&format!("r{i}"), "reasoning", &"x".repeat(60_000));
    }
    assert_eq!(error(&large), ProtocolError::LimitExceeded);
}
#[test]
fn framing_completion_is_explicitly_not_an_os_or_durable_success() {
    let result = decode(&valid("ACK")).unwrap();
    assert_eq!(result.message, "ACK");
    // No process spawn/exit/EOF-of-stderr/Job/SQLite APIs exist in this phase.
    // A later owned driver must independently judge all transport facts.
    assert_eq!(crate::codex::PINNED_CLI_VERSION, "0.163.0");
    assert_eq!(crate::codex::SMOKE_MODEL, "gpt-6.1-sol");
}

#[test]
fn milestones_follow_accepted_events_and_the_trace_is_text_free() {
    let mut decoder = CodexJsonlDecoder::new();
    decoder
        .feed((prefix() + &item("r1", "reasoning", "SECRET reasoning")).as_bytes())
        .unwrap();
    assert_eq!(
        decoder.take_milestones(),
        [
            Milestone::ThreadStarted,
            Milestone::TurnStarted,
            Milestone::ReasoningObserved
        ]
    );
    assert!(decoder.take_milestones().is_empty());
    // A repeated item id is a lifecycle violation; the trace names the event
    // kinds only, never provider text.
    decoder
        .feed(item("m1", "agent_message", "SECRET one").as_bytes())
        .unwrap();
    assert_eq!(
        decoder.feed(item("m1", "agent_message", "SECRET two").as_bytes()),
        Err(ProtocolError::InvalidLifecycle)
    );
    assert!(decoder.take_milestones().is_empty());
    let trace = decoder.trace();
    assert_eq!(
        trace,
        "thread.started,turn.started,item.completed:reasoning,item.completed:agent_message,item.completed:agent_message"
    );
    assert!(!trace.contains("SECRET"));
}

#[test]
fn tool_and_unknown_events_get_fixed_labels_not_provider_strings() {
    let mut decoder = CodexJsonlDecoder::new();
    decoder.feed(prefix().as_bytes()).unwrap();
    let bad = event(json!({"type":"item.started","item":{"id":"x","type":"web_search","query":"oakland weather"}}));
    assert_eq!(decoder.feed(bad.as_bytes()), Err(ProtocolError::ForbiddenActivity));
    assert!(decoder.trace().ends_with("item.started:web_search"));
    let mut other = CodexJsonlDecoder::new();
    let odd = event(json!({"type":"leak-this-string"}));
    assert_eq!(other.feed(odd.as_bytes()), Err(ProtocolError::ForbiddenActivity));
    assert_eq!(other.trace(), "unknown");
}

#[test]
fn a_preamble_message_is_accepted_and_the_last_message_is_the_reply() {
    let mut decoder = CodexJsonlDecoder::new();
    decoder
        .feed(
            (prefix()
                + &item("m1", "agent_message", "Let me think.")
                + &item("r1", "reasoning", "private")
                + &item("m2", "agent_message", "The answer.")
                + &completed())
                .as_bytes(),
        )
        .unwrap();
    // MessageComplete is reported once, at the first message.
    let milestones = decoder.take_milestones();
    assert_eq!(
        milestones
            .iter()
            .filter(|m| **m == Milestone::MessageComplete)
            .count(),
        1
    );
    let turn = decoder.finish_stdout().unwrap();
    assert_eq!(turn.message, "The answer.");
    assert_eq!(turn.reasoning_items_discarded, 1);
}

#[test]
fn later_messages_keep_every_bound_and_nothing_follows_turn_completed() {
    let first = prefix() + &item("m1", "agent_message", "one");
    let big = "x".repeat(MAX_MESSAGE_BYTES + 1);
    assert_eq!(
        error(&(first.clone() + &item("m2", "agent_message", &big) + &completed())),
        ProtocolError::LimitExceeded
    );
    assert_eq!(
        error(&(first.clone() + &item("m2", "agent_message", "") + &completed())),
        ProtocolError::LimitExceeded
    );
    assert_eq!(
        error(&(first.clone() + &item("m1", "agent_message", "dup") + &completed())),
        ProtocolError::InvalidLifecycle
    );
    assert_eq!(
        error(&(first + &completed() + &item("m2", "agent_message", "late"))),
        ProtocolError::InvalidLifecycle
    );
}
