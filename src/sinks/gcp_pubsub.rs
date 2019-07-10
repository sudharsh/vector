use crate::{
    buffers::Acker,
    event::{self, Event},
    sinks::util::{
        http::{HttpRetryLogic, HttpService},
        retries::FixedRetryPolicy,
        BatchServiceSink, Buffer, SinkExt,
    },
};

use base64;
use std::collections::HashMap;
use std::time::Duration;

use futures::{Future, Sink};

use http::Method;
use hyper::{Client, Uri};
use serde::{Deserialize, Serialize};
use tower::ServiceBuilder;

const HEALTHCHECK_ENDPOINT: &'static str = "https://www.googleapis.com/auth/cloud-platform";

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct GCPPubsubSinkConfig {
    project_id: String,
    topic: String,

    batch_size: Option<usize>,

    // Tower Request configurations since we're using
    // HTTP Rest APIs.
    // TODO: GRPC interfaces requires rust binding to protos
    pub request_timeout: Option<u64>,
    pub request_rate_limit_duration_secs: Option<u64>,
    pub request_rate_limit_num: Option<u64>,
    pub request_in_flight_limit: Option<usize>,
    pub request_retry_attempts: Option<usize>,
    pub request_retry_backoff_secs: Option<u64>,
}

pub struct GCPPubsubSink;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PubsubMessage {
    pub data: Vec<u8>,
    pub attributes: HashMap<String, String>,
}

#[typetag::serde(name = "gcp_pubsub")]
impl crate::topology::config::SinkConfig for GCPPubsubSinkConfig {
    fn build(&self, acker: Acker) -> Result<(super::RouterSink, super::Healthcheck), String> {
        let sink = gcp_pubsub_sink(self.clone(), acker);
        let check = healthcheck()?;
        Ok((sink, check))
    }
}

fn healthcheck() -> Result<super::Healthcheck, String> {
    let client = Client::new();
    let fut = client
        .get(Uri::from_static(HEALTHCHECK_ENDPOINT))
        .map_err(|e| format!("GCP healthcheck failed: {}", e))
        .and_then(|_| Ok(())); // FIXME: Check for 200
    Ok(Box::new(fut))
}

fn gcp_pubsub_sink(config: GCPPubsubSinkConfig, acker: Acker) -> super::RouterSink {
    let retry_attempts = config.request_retry_attempts.unwrap_or(100);
    let retry_backoff_secs = config.request_retry_backoff_secs.unwrap_or(1);
    let in_flight_limit = config.request_in_flight_limit.unwrap_or(5);
    let rate_limit_num = config.request_rate_limit_num.unwrap_or(5);
    let rate_limit_duration = config.request_rate_limit_duration_secs.unwrap_or(1);
    let timeout = config.request_timeout.unwrap_or(0);
    let batch_size = config.batch_size.unwrap_or(0usize);

    let policy = FixedRetryPolicy::new(
        retry_attempts,
        Duration::from_secs(retry_backoff_secs),
        HttpRetryLogic,
    );

    let gcp_pubsub_service = HttpService::new(move |body: Vec<u8>| {
        let publish_url = format!("https://pubsub.googleapis.com/v1/{}:publish", config.topic);
        let mut builder = hyper::Request::builder();
        builder.method(Method::POST);
        builder.uri(&publish_url);
        builder.header("Content-Type", "application/json");
        builder.body(body).unwrap()
    });

    let service = ServiceBuilder::new()
        .concurrency_limit(in_flight_limit)
        .rate_limit(rate_limit_num, Duration::from_secs(rate_limit_duration))
        .retry(policy)
        .timeout(Duration::from_secs(timeout))
        .service(gcp_pubsub_service);

    let sink = BatchServiceSink::new(service, acker)
        .batched_with_min(Buffer::new(false), batch_size, Duration::from_secs(timeout))
        .with(move |event: Event| {
            let mut body: Vec<u8> = vec![];
            serde_json::to_writer(&mut body, &event.as_log().all_fields()).unwrap();
            Ok(body)
        });
    Box::new(sink)
}

fn encode_event(event: &Event) -> PubsubMessage {
    let log = event.as_log();
    let payload = base64::encode(
        &log.get(&event::MESSAGE)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or(Vec::new()),
    );
    if (log.is_structured()) {
        let all = log
            .explicit_fields()
            .map(|(k, v)| (k.as_ref().to_string(), v.to_string_lossy()))
            .collect::<HashMap<String, String>>();
        PubsubMessage {
            data: payload.as_bytes().to_vec(),
            attributes: all,
        }
    } else {
        PubsubMessage {
            data: payload.as_bytes().to_vec(),
            attributes: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubsub_encode_event_non_structured() {
        let raw_message = "hello cruel world".to_string();
        let b64_message = "aGVsbG8gY3J1ZWwgd29ybGQ=".to_string();

        let payload = encode_event(&raw_message.clone().into());
        assert_eq!(payload.data, b64_message.as_bytes().to_vec());
    }

    #[test]
    fn pubsub_encode_event_structured() {
        let raw_message = "hello cruel world".to_string();
        let b64_message = "aGVsbG8gY3J1ZWwgd29ybGQ=".to_string();

        let mut event = Event::from(raw_message);
        event.as_mut_log().insert_explicit("k".into(), "v".into());
        event
            .as_mut_log()
            .insert_explicit("foo".into(), "bar".into());
        let payload = encode_event(&event);

        let expected: HashMap<String, String> = [
            ("k".to_string(), "v".to_string()),
            ("foo".to_string(), "bar".to_string()),
        ]
        .iter()
        .cloned()
        .collect();

        assert_eq!(payload.attributes, expected);
        assert_eq!(payload.data, b64_message.as_bytes().to_vec())
    }
}
