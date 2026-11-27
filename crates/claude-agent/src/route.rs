//! Which `claude` process serves a request: a pure function of the live sessions, the stopped
//! ones and the request. Sessions are matched by the digests of the turns they have seen.

use std::collections::HashSet;

use serde_json::Value;

use crate::conversation::{
    Conversation, Digest, Fingerprint, ToolResult, checkpoint, results, user_content,
};

/// What a live session is doing.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Activity<'a> {
    /// Its last turn ended; it waits for the next user message.
    Idle,
    /// Its turn stopped on these tool calls, which it waits for the harness to answer.
    Holding(&'a [String]),
    /// A turn is streaming, or the process is starting.
    Busy,
}

/// A live session as routing sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Live<'a> {
    pub(crate) fingerprint: &'a Fingerprint,
    pub(crate) seen: &'a [Digest],
    pub(crate) activity: Activity<'a>,
}

/// Where a resumed session comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// A stopped session's record, by index.
    Record(usize),
    /// A live session whose settings changed: it stops, then resumes with the new ones.
    Live(usize),
}

/// How a request is served. Each `message` is the content of the user message Claude Code
/// gets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Route {
    /// The live session holds exactly the calls the request's last turn answers.
    Continue {
        session: usize,
        results: Vec<ToolResult>,
    },
    /// The live session has seen the whole history; the new user turn goes into it.
    Append { session: usize, message: Vec<Value> },
    /// A session that has seen the whole history continues in a new process.
    Resume { from: Stopped, message: Vec<Value> },
    /// No session has seen this history: a new one starts from a checkpoint of it.
    Fresh { message: Vec<Value> },
    /// A request without tools, such as a title: one turn in a throwaway process.
    Side { message: Vec<Value> },
}

/// Routes `conversation` among the live sessions and the stopped sessions' records (their
/// seen turns, oldest first).
pub(crate) fn route(
    lives: &[Live<'_>],
    records: &[&[Digest]],
    conversation: &Conversation,
) -> Route {
    let tail = &conversation.tail;
    let history = conversation.history_digests();
    let fresh = || user_content(checkpoint(&conversation.history).as_deref(), tail);
    if conversation.settings.tools.is_empty() {
        return Route::Side { message: fresh() };
    }
    if history.is_empty() {
        return Route::Fresh { message: fresh() };
    }
    let answered: HashSet<&str> = tail.result_ids().into_iter().collect();
    let only_answers = !answered.is_empty() && !tail.has_message();
    let fingerprint = conversation.settings.fingerprint();
    let mut changed = None;
    for (index, live) in lives.iter().enumerate() {
        if live.seen != history.as_slice() {
            continue;
        }
        let same = *live.fingerprint == fingerprint;
        match live.activity {
            Activity::Holding(calls)
                if same
                    && only_answers
                    && calls.len() == answered.len()
                    && calls.iter().all(|call| answered.contains(call.as_str())) =>
            {
                return Route::Continue {
                    session: index,
                    results: results(tail),
                };
            }
            Activity::Idle if answered.is_empty() && same => {
                return Route::Append {
                    session: index,
                    message: user_content(None, tail),
                };
            }
            Activity::Idle if answered.is_empty() => changed = changed.or(Some(index)),
            _ => {}
        }
    }
    if let Some(index) = changed {
        return Route::Resume {
            from: Stopped::Live(index),
            message: user_content(None, tail),
        };
    }
    if answered.is_empty()
        && let Some(index) = records.iter().rposition(|seen| *seen == history.as_slice())
    {
        return Route::Resume {
            from: Stopped::Record(index),
            message: user_content(None, tail),
        };
    }
    Route::Fresh { message: fresh() }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::conversation::{Part, parse};

    const TOOLS: &str = r#"[{ "name": "read", "input_schema": { "type": "object" } }]"#;

    fn request(messages: &Value) -> Conversation {
        request_with(messages, "claude-opus-5-5")
    }

    fn request_with(messages: &Value, model: &str) -> Conversation {
        let tools: Value = serde_json::from_str(TOOLS).unwrap();
        parse(
            json!({ "model": model, "tools": tools, "messages": messages })
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    }

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": text })
    }

    fn assistant(text: &str, calls: &[&str]) -> Value {
        let mut content = vec![json!({ "type": "text", "text": text })];
        content.extend(
            calls
                .iter()
                .map(|id| json!({ "type": "tool_use", "id": id, "name": "read", "input": {} })),
        );
        json!({ "role": "assistant", "content": content })
    }

    fn answers(ids: &[&str]) -> Value {
        let content: Vec<Value> = ids
            .iter()
            .map(|id| json!({ "type": "tool_result", "tool_use_id": id, "content": format!("out {id}") }))
            .collect();
        json!({ "role": "user", "content": content })
    }

    /// The digests of every turn of `messages`: what a session that answered them has seen.
    fn seen(messages: &Value) -> Vec<Digest> {
        let mut with_tail = messages.as_array().unwrap().clone();
        with_tail.push(user("next"));
        request(&Value::Array(with_tail)).history_digests()
    }

    fn text(value: &str) -> Value {
        json!({ "type": "text", "text": value })
    }

    #[test]
    fn a_new_conversation_starts_fresh_with_just_its_message() {
        let conversation = request(&json!([user("hi")]));
        assert_eq!(
            route(&[], &[], &conversation),
            Route::Fresh {
                message: vec![text("hi")]
            }
        );
    }

    #[test]
    fn a_next_message_appends_to_the_idle_session_that_has_seen_the_history() {
        let earlier = json!([user("hi"), assistant("hello", &[])]);
        let seen_other = seen(&json!([user("other"), assistant("hello", &[])]));
        let seen_this = seen(&earlier);
        let fingerprint = request(&json!([user("x")])).settings.fingerprint();
        let lives = [
            Live {
                fingerprint: &fingerprint,
                seen: &seen_other,
                activity: Activity::Idle,
            },
            Live {
                fingerprint: &fingerprint,
                seen: &seen_this,
                activity: Activity::Idle,
            },
        ];
        let conversation = request(&json!([user("hi"), assistant("hello", &[]), user("more")]));
        assert_eq!(
            route(&lives, &[], &conversation),
            Route::Append {
                session: 1,
                message: vec![text("more")]
            }
        );
        // The same history on a busy session cannot go into it.
        let busy = [Live {
            activity: Activity::Busy,
            ..lives[1]
        }];
        assert!(matches!(
            route(&busy, &[], &conversation),
            Route::Fresh { .. }
        ));
    }

    #[test]
    fn answers_to_exactly_the_held_calls_continue_the_turn() {
        let earlier = json!([
            user("read both"),
            assistant("Reading.", &["toolu_a", "toolu_b"])
        ]);
        let seen = seen(&earlier);
        let held = ["toolu_a".to_owned(), "toolu_b".to_owned()];
        let fingerprint = request(&json!([user("x")])).settings.fingerprint();
        let lives = [Live {
            fingerprint: &fingerprint,
            seen: &seen,
            activity: Activity::Holding(&held),
        }];
        let both = request(&json!([
            user("read both"),
            assistant("Reading.", &["toolu_a", "toolu_b"]),
            answers(&["toolu_b", "toolu_a"]),
        ]));
        let Route::Continue { session, results } = route(&lives, &[], &both) else {
            panic!("not continued")
        };
        assert_eq!(session, 0);
        let ids: Vec<&str> = results.iter().map(|result| result.id.as_str()).collect();
        assert_eq!(ids, ["toolu_b", "toolu_a"]);
        assert_eq!(
            results[1].content,
            vec![Part::Text("out toolu_a".to_owned())]
        );
        // One answer missing, or a message beside the answers: a fresh session replays it.
        let one = request(&json!([
            user("read both"),
            assistant("Reading.", &["toolu_a", "toolu_b"]),
            answers(&["toolu_a"]),
        ]));
        assert!(matches!(route(&lives, &[], &one), Route::Fresh { .. }));
        let mut with_text = answers(&["toolu_a", "toolu_b"]);
        with_text["content"]
            .as_array_mut()
            .unwrap()
            .push(text("also this"));
        let mixed = request(&json!([
            user("read both"),
            assistant("Reading.", &["toolu_a", "toolu_b"]),
            with_text,
        ]));
        assert!(matches!(route(&lives, &[], &mixed), Route::Fresh { .. }));
    }

    #[test]
    fn changed_settings_resume_the_live_session_and_records_resume_stopped_ones() {
        let earlier = json!([user("hi"), assistant("hello", &[])]);
        let seen = seen(&earlier);
        let old = request_with(&json!([user("x")]), "claude-sonnet-5-5")
            .settings
            .fingerprint();
        let lives = [Live {
            fingerprint: &old,
            seen: &seen,
            activity: Activity::Idle,
        }];
        let next = request(&json!([user("hi"), assistant("hello", &[]), user("more")]));
        assert_eq!(
            route(&lives, &[], &next),
            Route::Resume {
                from: Stopped::Live(0),
                message: vec![text("more")]
            }
        );
        let other = self::seen(&json!([user("other"), assistant("x", &[])]));
        let records: [&[Digest]; 3] = [&seen, &other, &seen];
        assert_eq!(
            route(&[], &records, &next),
            Route::Resume {
                from: Stopped::Record(2),
                message: vec![text("more")]
            }
        );
    }

    #[test]
    fn an_edited_history_starts_fresh_from_a_checkpoint() {
        let seen = seen(&json!([user("hi"), assistant("hello", &[])]));
        let fingerprint = request(&json!([user("x")])).settings.fingerprint();
        let lives = [Live {
            fingerprint: &fingerprint,
            seen: &seen,
            activity: Activity::Idle,
        }];
        let edited = request(&json!([user("hi!"), assistant("hello", &[]), user("more")]));
        let Route::Fresh { message } = route(&lives, &[&seen], &edited) else {
            panic!("not fresh")
        };
        assert_eq!(message.len(), 2);
        let checkpoint = message[0]["text"].as_str().unwrap();
        assert!(
            checkpoint.starts_with("<conversation-checkpoint>"),
            "{checkpoint}"
        );
        assert!(checkpoint.contains("<user>\nhi!\n</user>"), "{checkpoint}");
        assert_eq!(message[1], text("more"));
    }

    #[test]
    fn a_request_without_tools_is_a_side_call() {
        let side = parse(
            json!({ "model": "m", "messages": [user("Title this: hi")] })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            route(&[], &[], &side),
            Route::Side {
                message: vec![text("Title this: hi")]
            }
        );
    }
}
