//! Structured output: asking for JSON and actually getting it.
//!
//! Models are asked, not told. Even a good one occasionally wraps its answer in
//! a fenced code block or writes a sentence before it, so this module extracts
//! the JSON from whatever came back and, if that fails, hands the parse error
//! straight back to the model and asks once more.

use crate::message::Message;
use crate::provider::Provider;
use crate::request::Request;
use crate::tool::Schema;
use rustlavel_core::{Error, Json, Result};

/// How many times to ask again after unparseable output.
///
/// One. A model that cannot produce JSON twice in a row will not manage it on
/// the fifth attempt either, and every retry costs the caller money and a
/// request they are waiting on.
pub const REPAIR_ATTEMPTS: usize = 1;

/// Ask for JSON matching a schema, and return it parsed.
pub async fn generate_as(
    provider: &dyn Provider,
    request: &Request,
    schema: &Schema,
) -> Result<Json> {
    let mut request = instruct(request, schema);
    let mut attempts = 0;

    loop {
        let completion = provider.complete(&request).await?;
        let text = completion.text.clone();

        match extract(&text) {
            Ok(value) => return Ok(value),
            Err(error) if attempts < REPAIR_ATTEMPTS => {
                attempts += 1;
                // Show the model its own answer and the parser's complaint.
                // Saying what was wrong works far better than asking again.
                request.messages.push(Message::assistant(text));
                request.messages.push(Message::user(format!(
                    "That could not be parsed as JSON: {error}. \
                     Reply with the JSON value only — no prose, no code fence."
                )));
            }
            Err(error) => {
                return Err(Error::msg(format!(
                    "the model did not return JSON matching the schema after \
                     {} attempts: {error}. It last said: {}",
                    attempts + 1,
                    excerpt(&text)
                )));
            }
        }
    }
}

/// Add the schema and the "JSON only" instruction to a request.
fn instruct(request: &Request, schema: &Schema) -> Request {
    let instruction = format!(
        "Reply with a single JSON value matching this JSON Schema, and nothing \
         else — no explanation, no code fence.\n\n{}",
        schema.to_json().to_string_pretty()
    );

    let mut request = request.clone();
    request.system = Some(match request.effective_system() {
        Some(existing) => format!("{existing}\n\n{instruction}"),
        None => instruction,
    });
    // The instruction now carries every system message, so keeping them would
    // send each one twice.
    request.messages.retain(|message| message.role != crate::message::Role::System);
    request
}

/// Find the JSON in whatever the model said, and parse it.
pub fn extract(text: &str) -> Result<Json> {
    let candidate = strip_fence(text.trim());
    if let Ok(value) = Json::parse(candidate) {
        return Ok(value);
    }
    // Prose before or after the value is common; take the outermost braces or
    // brackets and try that.
    match slice(candidate) {
        Some(inner) => Json::parse(inner),
        None => Json::parse(candidate),
    }
}

/// Remove a ```json … ``` wrapper, which models add however firmly they are
/// asked not to.
fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    // The opening fence may name a language: ```json
    match rest.split_once('\n') {
        Some((first, body)) if !first.trim().contains(['{', '[', '"']) => body.trim(),
        _ => rest.trim(),
    }
}

/// The widest `{…}` or `[…]` span in the text.
fn slice(text: &str) -> Option<&str> {
    let open = text.find(['{', '['])?;
    let closing = if text.as_bytes()[open] == b'{' { '}' } else { ']' };
    let close = text.rfind(closing)?;
    if close <= open {
        return None;
    }
    Some(&text[open..=close])
}

/// A short piece of the model's answer, for an error message.
fn excerpt(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::Completion;
    use crate::fake::Fake;

    fn person() -> Schema {
        Schema::object().string("name", "Their name").integer("age", "Their age in years")
    }

    #[test]
    fn extracts_plain_json() {
        let value = extract(r#"{"name":"Ada","age":36}"#).unwrap();
        assert_eq!(value.get("name").unwrap().as_str(), Some("Ada"));
    }

    #[test]
    fn extracts_json_from_a_fenced_code_block() {
        let value = extract("```json\n{\"name\": \"Ada\"}\n```").unwrap();
        assert_eq!(value.get("name").unwrap().as_str(), Some("Ada"));

        let unlabelled = extract("```\n[1, 2, 3]\n```").unwrap();
        assert_eq!(unlabelled.as_array().unwrap().len(), 3);
    }

    #[test]
    fn extracts_json_buried_in_prose() {
        let value = extract("Sure! Here you go:\n{\"name\": \"Ada\"}\nHope that helps.").unwrap();
        assert_eq!(value.get("name").unwrap().as_str(), Some("Ada"));
    }

    #[test]
    fn text_with_no_json_in_it_is_an_error() {
        assert!(extract("I would rather not.").is_err());
    }

    #[tokio::test]
    async fn asks_for_the_schema_and_returns_the_parsed_value() {
        let fake = Fake::new().reply(r#"{"name":"Ada","age":36}"#);
        let request = Request::new("claude-sonnet-5").system("Be brief.").user("Who wrote the first program?");

        let value = generate_as(&fake, &request, &person()).await.unwrap();

        assert_eq!(value.get("name").unwrap().as_str(), Some("Ada"));
        assert_eq!(value.get("age").unwrap().as_i64(), Some(36));

        // The schema and the original instructions both reached the model.
        let sent = fake.last_request().unwrap();
        let system = sent.effective_system().unwrap();
        assert!(system.contains("Be brief."), "{system}");
        assert!(system.contains("\"age\""), "{system}");
        assert!(system.contains("JSON Schema"), "{system}");
    }

    #[tokio::test]
    async fn unparseable_output_is_handed_back_to_the_model_once() {
        let fake = Fake::new()
            .reply("Sorry, I cannot do that.")
            .reply(r#"{"name":"Ada","age":36}"#);
        let request = Request::new("claude-sonnet-5").user("Who wrote the first program?");

        let value = generate_as(&fake, &request, &person()).await.unwrap();

        assert_eq!(value.get("name").unwrap().as_str(), Some("Ada"));
        fake.assert_count(2);

        // The retry showed the model its own answer and the parse error.
        let retry = fake.last_request().unwrap();
        assert_eq!(retry.messages.len(), 3);
        assert!(retry.messages[1].text().contains("cannot do that"));
        assert!(retry.messages[2].text().contains("could not be parsed"));
    }

    #[tokio::test]
    async fn giving_up_names_what_the_model_actually_said() {
        let fake = Fake::new().always("I would rather not.");
        let request = Request::new("claude-sonnet-5").user("Who wrote the first program?");

        let error = generate_as(&fake, &request, &person()).await.unwrap_err().to_string();

        assert!(error.contains("I would rather not."), "{error}");
        assert!(error.contains("2 attempts"), "{error}");
        fake.assert_count(2);
    }

    #[tokio::test]
    async fn a_provider_failure_is_not_retried_as_a_parse_problem() {
        let fake = Fake::new().fails("the model is overloaded");
        let request = Request::new("claude-sonnet-5").user("Hello");

        let error = generate_as(&fake, &request, &person()).await.unwrap_err().to_string();

        assert_eq!(error, "the model is overloaded");
        fake.assert_count(1);
    }

    #[tokio::test]
    async fn a_long_answer_is_truncated_in_the_error() {
        let fake = Fake::new().always("no ".repeat(500));
        let request = Request::new("claude-sonnet-5").user("JSON please");

        let error = generate_as(&fake, &request, &person()).await.unwrap_err().to_string();

        assert!(error.contains('…'), "{error}");
        assert!(error.len() < 400, "the excerpt should be short, got {} chars", error.len());
    }

    #[tokio::test]
    async fn the_schema_instruction_replaces_system_messages_rather_than_duplicating_them() {
        let fake = Fake::new().reply("{}");
        let request = Request::new("claude-sonnet-5")
            .message(Message::system("Answer in English."))
            .user("Hi");

        generate_as(&fake, &request, &person()).await.unwrap();

        let sent = fake.last_request().unwrap();
        assert_eq!(sent.messages.len(), 1);
        assert_eq!(sent.effective_system().unwrap().matches("Answer in English.").count(), 1);
    }

    #[tokio::test]
    async fn a_completion_carrying_only_tool_calls_is_reported_as_unparseable() {
        let fake = Fake::new().always("");
        let request = Request::new("claude-sonnet-5").user("JSON please");

        assert!(generate_as(&fake, &request, &person()).await.is_err());
        // A hand-built empty completion behaves the same way.
        assert!(extract(&Completion::default().text).is_err());
    }
}
