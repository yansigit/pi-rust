#![cfg(feature = "providers")]

use rpi_ai::model::{MaxTokensField, OpenaiCompletionsCompat, StreamingProtocolCompat};
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_ai::types::{Message, StopReason, UserMessage};
use rpi_ai::{Api, Context, Model, Provider, SimpleStreamOptions};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Exercise the public provider, not just build_request: compare the entire
/// HTTP JSON body with Pi's public onPayload capture. URLs remain loopback;
/// provider-detection URL substrings live in the path, unchanged by the port.
#[tokio::test]
async fn complete_public_request_bodies_match_pi() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../docs/provider-parity/request-parity-reference.json"
    ))
    .unwrap();
    let rows = fixture["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 70);
    for row in rows {
        let name = row["name"].as_str().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let body = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0 && request.len() < 1_048_576);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end])
                        .unwrap()
                        .to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                            .unwrap();
                    }
                }
            };
            let response = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            body
        });
        let input = &row["model"];
        let mut model = Model::new(
            input["id"].as_str().unwrap(),
            "fixture",
            Api::OpenaiCompletions,
            input["provider"].as_str().unwrap(),
            input["baseUrl"]
                .as_str()
                .unwrap()
                .replacen("127.0.0.1", &address.to_string(), 1),
        );
        model.reasoning = input["reasoning"].as_bool().unwrap();
        model.max_tokens = 1024;
        let compat = &input["compat"];
        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                supports_developer_role: Some(false),
                supports_reasoning_effort: Some(false),
                supports_store: compat["supportsStore"].as_bool(),
                max_tokens_field: compat["maxTokensField"].as_str().map(|field| match field {
                    "max_tokens" => MaxTokensField::MaxTokens,
                    "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
                    _ => panic!("unknown max tokens field"),
                }),
                ..Default::default()
            },
        ));
        let provider = OpenAiCompletionsProvider::with_models_without_env_api_key(
            "offline",
            None,
            reqwest::Client::builder().no_proxy().build().unwrap(),
            vec![model.clone()],
        );
        let mut context = Context::new(vec![Message::User(UserMessage::new(
            serde_json::from_value::<rpi_ai::types::UserContent>(row["content"].clone()).unwrap(),
            1,
        ))]);
        context.system_prompt = Some(row["systemPrompt"].as_str().unwrap().into());
        let options = SimpleStreamOptions {
            api_key: Some("offline-fixture".into()),
            max_tokens: Some(37),
            cache_retention: rpi_ai::provider::CacheRetention::None,
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            provider
                .stream(&model, &context, &options)
                .await
                .result()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            result.stop_reason,
            StopReason::Stop,
            "{name}: {:?}",
            result.error_message
        );
        let actual = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual, row["result"], "{name}: complete request body");
    }
}
