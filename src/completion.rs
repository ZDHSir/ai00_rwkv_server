use anyhow::Result;
use axum::{
    extract::State,
    response::{sse::Event, IntoResponse, Response, Sse},
    Json,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::{
    sampler::Sampler, FinishReason, GenerateRequest, OptionArray, RequestKind, ThreadRequest,
    ThreadState, Token, TokenCounter,
};

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CompletionRequest {
    pub prompt: OptionArray<String>,
    pub max_tokens: usize,
    pub stop: OptionArray<String>,
    pub stream: bool,
    pub temperature: f32,
    pub top_p: f32,
    pub presence_penalty: f32,
    pub frequency_penalty: f32,
}

impl Default for CompletionRequest {
    fn default() -> Self {
        Self {
            prompt: OptionArray::default(),
            max_tokens: 256,
            stop: OptionArray::default(),
            stream: false,
            temperature: 1.0,
            top_p: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
        }
    }
}

impl From<CompletionRequest> for GenerateRequest {
    fn from(value: CompletionRequest) -> Self {
        let CompletionRequest {
            prompt,
            max_tokens,
            stop,
            temperature,
            top_p,
            presence_penalty,
            frequency_penalty,
            ..
        } = value;

        let prompt = Vec::from(prompt).join("");
        let max_tokens = max_tokens.min(crate::MAX_TOKENS);
        let stop = stop.into();

        Self {
            prompt,
            max_tokens,
            stop,
            sampler: Sampler {
                temperature,
                top_p,
                presence_penalty,
                frequency_penalty,
            },
            occurrences: Default::default(),
            embedding: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CompletionChoice {
    pub text: String,
    pub index: usize,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Serialize)]
pub struct CompletionResponse {
    pub object: String,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(rename = "usage")]
    pub counter: TokenCounter,
}

async fn completions_one(
    State(ThreadState { sender, model_name }): State<ThreadState>,
    Json(request): Json<CompletionRequest>,
) -> Json<CompletionResponse> {
    let (token_sender, token_receiver) = flume::unbounded();

    let _ = sender.send(ThreadRequest {
        request: RequestKind::Completion(request),
        token_sender,
    });

    let mut token_counter = TokenCounter::default();
    let mut finish_reason = FinishReason::Null;
    let mut text = String::new();
    let mut stream = token_receiver.into_stream();

    while let Some(token) = stream.next().await {
        match token {
            Token::Start => {}
            Token::Token(token) => {
                text += &token;
            }
            Token::Stop(reason, counter) => {
                finish_reason = reason;
                token_counter = counter;
                break;
            }
            _ => unreachable!(),
        }
    }

    Json(CompletionResponse {
        object: "text_completion".into(),
        model: model_name,
        choices: vec![CompletionChoice {
            text,
            index: 0,
            finish_reason,
        }],
        counter: token_counter,
    })
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialCompletionRecord {
    #[default]
    #[serde(rename = "")]
    None,
    Content(String),
}

#[derive(Debug, Default, Serialize)]
pub struct PartialCompletionChoice {
    pub delta: PartialCompletionRecord,
    pub index: usize,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Serialize)]
pub struct PartialCompletionResponse {
    pub object: String,
    pub model: String,
    pub choices: Vec<PartialCompletionChoice>,
}

async fn completions_stream(
    State(ThreadState { sender, model_name }): State<ThreadState>,
    Json(request): Json<CompletionRequest>,
) -> Sse<impl Stream<Item = Result<Event>>> {
    let (token_sender, token_receiver) = flume::unbounded();

    let _ = sender.send(ThreadRequest {
        request: RequestKind::Completion(request),
        token_sender,
    });

    let stream = token_receiver.into_stream().skip(1).map(move |token| {
        let choice = match token {
            Token::Token(token) => PartialCompletionChoice {
                delta: PartialCompletionRecord::Content(token),
                ..Default::default()
            },
            Token::Stop(finish_reason, _) => PartialCompletionChoice {
                finish_reason,
                ..Default::default()
            },
            Token::Done => return Ok(Event::default().data("[DONE]")),
            _ => unreachable!(),
        };

        let json = serde_json::to_string(&PartialCompletionResponse {
            object: "text_completion.chunk".into(),
            model: model_name.clone(),
            choices: vec![choice],
        })?;
        Ok(Event::default().data(json))
    });

    Sse::new(stream)
}

pub async fn completions(
    state: State<ThreadState>,
    Json(request): Json<CompletionRequest>,
) -> Response {
    if request.stream {
        completions_stream(state, Json(request))
            .await
            .into_response()
    } else {
        completions_one(state, Json(request)).await.into_response()
    }
}
