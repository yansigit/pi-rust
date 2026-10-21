#![cfg(feature = "providers")]

use rpi_ai::{Api, Context, Model, Provider, SimpleStreamOptions};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn every_http_protocol_delivers_private_headers_before_its_terminal_event() {
    for api in [
        Api::AnthropicMessages,
        Api::OpenaiCompletions,
        Api::OpenaiResponses,
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = Model::new(
            "m",
            "M",
            api.clone(),
            "test",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![];
            let mut buffer = [0; 4096];
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0 && request.len() < 1_048_576);
                request.extend_from_slice(&buffer[..n]);
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
                        break;
                    }
                }
            }
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nSet-Cookie: private-one\r\nSet-Cookie: private-two\r\nContent-Length: 4\r\nConnection: close\r\n\r\nrate").await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let provider: Arc<dyn Provider> = match api {
            Api::AnthropicMessages => Arc::new(rpi_ai::providers::anthropic::AnthropicProvider::with_models_without_env_api_key(None, client, vec![model.clone()])),
            Api::OpenaiCompletions => Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key("test", None, client, vec![model.clone()])),
            Api::OpenaiResponses => Arc::new(rpi_ai::providers::openai_responses::OpenAiResponsesProvider::with_models_without_env_api_key("test", None, client, vec![model.clone()])),
            _ => unreachable!(),
        };
        let receipts = Arc::new(Mutex::new(vec![]));
        let options = SimpleStreamOptions {
            api_key: Some("test-only".into()),
            max_retries: Some(0),
            timeout: Some(Duration::from_secs(5)),
            response_observer: Some(Arc::new({
                let receipts = receipts.clone();
                move |receipt| {
                    let receipts = receipts.clone();
                    Box::pin(async move {
                        receipts.lock().unwrap().push(receipt);
                    })
                }
            })),
            ..Default::default()
        };
        let mut stream = provider
            .stream_simple(&model, &Context::new(vec![]), &options)
            .await;
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert!(event.is_terminal());
        let receipts = receipts.lock().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].status, 429);
        assert_eq!(
            receipts[0]
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .count(),
            2
        );
        assert!(receipts[0]
            .headers
            .contains(&("retry-after".into(), b"7".to_vec())));
        assert!(!serde_json::to_string(&event).unwrap().contains("private-"));
        server.await.unwrap();
    }
}
