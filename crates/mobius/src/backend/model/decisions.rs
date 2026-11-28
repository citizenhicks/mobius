//! Decision models: fixed-choice questions answered with the option's probability, on
//! providers whose manifest names a decisions endpoint (TypeSafe's System One shape).
//! Requests travel through the provider adapter like image generation does.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Model;
use super::router::{ModelCredentialLifetime, while_valid};
use crate::{Error, Result};

pub(super) const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// One fixed-choice question: the options' names are the answers, their descriptions the
/// criteria the model weighs.
#[derive(Debug)]
#[non_exhaustive]
pub struct DecisionQuestion<'a> {
    /// What the model is asked about the state.
    pub instructions: &'a str,
    /// Option name to description; names are returned verbatim.
    pub options: BTreeMap<&'a str, &'a str>,
}

impl<'a> DecisionQuestion<'a> {
    /// Borrows the instructions and offered criteria for one decision.
    #[must_use]
    pub const fn new(instructions: &'a str, options: BTreeMap<&'a str, &'a str>) -> Self {
        Self {
            instructions,
            options,
        }
    }
}

/// Every question asked about one state, in one request.
#[derive(Debug, Clone, Copy)]
pub struct DecisionRequest<'a> {
    /// What the questions are about.
    pub state: &'a str,
    /// Question ID to question.
    pub questions: &'a BTreeMap<&'a str, DecisionQuestion<'a>>,
}

/// The model's pick for one question.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionAnswer {
    /// The chosen option's name.
    pub choice: String,
    /// How concentrated the probabilities are, from 0 (uniform) to 1.
    pub confidence: f64,
}

/// Answers by question ID.
pub type DecisionAnswers = BTreeMap<String, DecisionAnswer>;

/// Asks a decision route while its credential stays valid.
/// # Errors
/// Returns an error if the route cannot decide, the credential ends, or the response
/// cannot be read. Missing and unoffered choices are omitted.
pub async fn decide(
    model: &dyn Model,
    credential: &ModelCredentialLifetime,
    request: DecisionRequest<'_>,
) -> Result<DecisionAnswers> {
    while_valid(credential, None, || model.decide(request)).await
}

#[derive(Serialize)]
struct Choice<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    criteria: &'a BTreeMap<&'a str, &'a str>,
}

/// The decisions endpoint's request body.
#[derive(Serialize)]
pub(super) struct Body<'a> {
    model: &'a str,
    state: &'a str,
    questions: BTreeMap<&'a str, Choice<'a>>,
}

impl<'a> Body<'a> {
    pub(super) fn new(model: &'a str, request: DecisionRequest<'a>) -> Self {
        Self {
            model,
            state: request.state,
            questions: request
                .questions
                .iter()
                .map(|(&id, question)| {
                    let choice = Choice {
                        kind: "choice",
                        instructions: question.instructions,
                        criteria: &question.options,
                    };
                    (id, choice)
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
}

#[derive(Deserialize)]
struct Answer {
    choice: String,
    confidence: f64,
}

/// Keeps each answer that names an offered option with a probability.
pub(super) fn decode(
    bytes: &[u8],
    request: DecisionRequest<'_>,
    provider: &str,
) -> Result<DecisionAnswers> {
    let mut answers = serde_json::from_slice::<Response>(bytes)
        .map_err(|error| Error::Provider(format!("{provider} decision response: {error}").into()))?
        .answers;
    Ok(request
        .questions
        .iter()
        .filter_map(|(&id, question)| {
            let (id, answer) = answers.remove_entry(id)?;
            (question.options.contains_key(answer.choice.as_str())
                && (0.0..=1.0).contains(&answer.confidence))
            .then_some((
                id,
                DecisionAnswer {
                    choice: answer.choice,
                    confidence: answer.confidence,
                },
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::backend::model::provider::{BaseUrl, ProviderBuildConfig, ProviderCredential};
    use crate::backend::model::test_support::{loopback_listener, read_http_request};
    use crate::protocol::ModelCapability;

    fn question<'a>(options: &[&'a str]) -> DecisionQuestion<'a> {
        DecisionQuestion::new(
            "Pick one",
            options.iter().map(|&name| (name, "why")).collect(),
        )
    }

    #[tokio::test]
    async fn the_provider_adapter_asks_every_question_and_drops_unoffered_answers() {
        let (listener, address) = loopback_listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut stream).await;
            let body = r#"{"answers":{"bot":{"choice":"@coder","confidence":0.9},"workspace":{"choice":"/elsewhere","confidence":1.0}}}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("respond");
            request
        });
        let transport = crate::backend::model::ModelTransportSettings::default();
        let model = crate::backend::model::provider::provider("openrouter")
            .expect("openrouter")
            .build(ProviderBuildConfig {
                capability: Some(ModelCapability::Decisions),
                credential: ProviderCredential::ApiKey("test-key".into()),
                model: "decider".into(),
                base_url: Some(BaseUrl::new(format!("http://{address}/api/v1")).expect("URL")),
                reasoning_effort: None,
                service_tier: None,
                web_search: Default::default(),
                tool_discovery: None,
                http: transport.streaming_client().expect("client"),
                transport,
            })
            .expect("decision route");
        let questions = BTreeMap::from([
            ("bot", question(&["@mobius", "@coder"])),
            ("workspace", question(&["/a", "/b"])),
        ]);
        let answers = decide(
            &*model,
            &ModelCredentialLifetime::default(),
            DecisionRequest {
                state: "fix the build",
                questions: &questions,
            },
        )
        .await
        .expect("valid answers survive malformed siblings");
        assert_eq!(answers.len(), 1);
        assert_eq!(answers["bot"].choice, "@coder");
        let request = server.await.expect("server");
        assert!(request.starts_with("POST /api/v1/systemone "), "{request}");
        assert!(
            request.contains("authorization: Bearer test-key"),
            "{request}"
        );
        assert!(request.contains("x-openrouter-title"), "{request}");
        let (_, body) = request.split_once("\r\n\r\n").expect("body");
        let body: Value = serde_json::from_str(body).expect("JSON body");
        assert_eq!(body["model"], "decider");
        assert_eq!(body["state"], "fix the build");
        assert_eq!(body["questions"]["bot"]["type"], "choice");
        assert_eq!(body["questions"]["bot"]["criteria"]["@coder"], "why");
    }
}
