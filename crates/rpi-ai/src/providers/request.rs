//! Encode each provider request once, including retries. Buffered reqwest JSON
//! bodies use shared Bytes; cloning an attempt does not clone a JSON tree.
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct PreparedJsonRequest(Arc<PreparedRequest>);

struct PreparedRequest {
    client: reqwest::Client,
    request: reqwest::Request,
}

impl PreparedJsonRequest {
    pub(super) fn new<T: serde::Serialize + ?Sized>(
        builder: reqwest::RequestBuilder,
        body: &T,
    ) -> Result<Self, reqwest::Error> {
        let (client, request) = builder.json(body).build_split();
        Ok(Self(Arc::new(PreparedRequest {
            client,
            request: request?,
        })))
    }

    pub(super) async fn send(&self) -> Result<reqwest::Response, crate::AiError> {
        let request = self
            .0
            .request
            .try_clone()
            .ok_or_else(|| crate::AiError::Provider {
                code: "request_not_replayable".into(),
                message: "encoded JSON request is not replayable".into(),
            })?;
        self.0
            .client
            .execute(request)
            .await
            .map_err(|error| crate::AiError::Http {
                status: None,
                message: format!("http transport error: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted<'a>(&'a AtomicUsize);
    impl serde::Serialize for Counted<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_str(&"history 🐈\n".repeat(4096))
        }
    }

    #[test]
    fn retry_clones_share_encoded_bytes_and_preserve_custom_headers() {
        let calls = AtomicUsize::new(0);
        let client = reqwest::Client::new();
        let prepared = PreparedJsonRequest::new(
            client
                .post("http://127.0.0.1/request")
                .header("Content-Type", "application/x-rpi-json")
                .header("x-test", "kept")
                .timeout(std::time::Duration::from_secs(7)),
            &Counted(&calls),
        )
        .unwrap();
        for _ in 0..8 {
            let request = prepared.0.request.try_clone().unwrap();
            let bytes = request.body().unwrap().as_bytes().unwrap();
            assert_eq!(
                bytes.as_ptr(),
                prepared
                    .0
                    .request
                    .body()
                    .unwrap()
                    .as_bytes()
                    .unwrap()
                    .as_ptr()
            );
            assert_eq!(
                serde_json::from_slice::<String>(bytes).unwrap(),
                "history 🐈\n".repeat(4096)
            );
            assert_eq!(request.headers()["content-type"], "application/x-rpi-json");
            assert_eq!(request.headers()["x-test"], "kept");
            assert_eq!(request.timeout(), Some(&std::time::Duration::from_secs(7)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_url_or_headers_are_rejected_before_sending() {
        let client = reqwest::Client::new();
        assert!(PreparedJsonRequest::new(client.post("not a URL"), &()).is_err());
        assert!(PreparedJsonRequest::new(
            client
                .post("http://127.0.0.1")
                .header("x-test", "invalid\nvalue"),
            &()
        )
        .is_err());
    }
}
