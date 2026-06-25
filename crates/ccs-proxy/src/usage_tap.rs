//! The L0 cache-usage tap: a read-only stream adapter over reqwest's `bytes_stream()`.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use bytes::Bytes;
use ccs_core::ModelId;
use ccs_economics::CacheUsage;
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::mpsc;

const SCAN_CAP: usize = 64 * 1024;

const TARGET_EVENT: &str = "message_start";

#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub usage: CacheUsage,
    pub model: ModelId,
}

pub type UsageSink = mpsc::Sender<Observed>;

#[derive(Deserialize)]
struct MessageStart {
    message: StartMessage,
}

#[derive(Deserialize)]
struct StartMessage {
    model: ModelId,
    usage: CacheUsage,
}

struct Scanner {
    buf: Vec<u8>,
    sink: UsageSink,
    done: bool,
}

impl Scanner {
    fn new(sink: UsageSink) -> Self {
        Self {
            buf: Vec::new(),
            sink,
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        self.buf.extend_from_slice(chunk);
        if let Some(observed) = self.scan() {
            let _ = self.sink.try_send(observed);
            self.finish();
        } else if self.buf.len() >= SCAN_CAP {
            self.finish();
        }
    }

    fn finish(&mut self) {
        self.done = true;
        self.buf = Vec::new();
    }

    fn scan(&self) -> Option<Observed> {
        let mut event: Option<&[u8]> = None;
        for line in self.buf.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            match parse_field(line, b"event:") {
                Some(name) => event = Some(name),
                None => {
                    if let Some(data) = parse_field(line, b"data:") {
                        if event == Some(TARGET_EVENT.as_bytes()) {
                            if let Some(observed) = parse_message_start(data) {
                                return Some(observed);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

fn parse_field<'a>(line: &'a [u8], field: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(field)?;
    Some(rest.strip_prefix(b" ").unwrap_or(rest))
}

fn parse_message_start(data: &[u8]) -> Option<Observed> {
    let ms: MessageStart = serde_json::from_slice(data).ok()?;
    Some(Observed {
        usage: ms.message.usage,
        model: ms.message.model,
    })
}

pub fn tap<S>(upstream: S, sink: UsageSink) -> impl Stream<Item = reqwest::Result<Bytes>>
where
    S: Stream<Item = reqwest::Result<Bytes>>,
{
    let mut scanner = Scanner::new(sink);
    futures_util::StreamExt::map(upstream, move |item| {
        if let Ok(chunk) = &item {
            scanner.feed(chunk);
        }
        item
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccs_core::TokenCount;
    use futures_util::{stream, StreamExt};

    fn message_start(creation: u32, read: u32, input: u32) -> String {
        format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":\
             {{\"id\":\"msg_1\",\"model\":\"claude-opus-4-20250514\",\"usage\":\
             {{\"input_tokens\":{input},\"cache_creation_input_tokens\":{creation},\
             \"cache_read_input_tokens\":{read}}}}}}}\n\n"
        )
    }

    async fn run(chunks: Vec<&str>) -> (Vec<u8>, Option<Observed>) {
        let (tx, mut rx) = mpsc::channel(1);
        let src = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<Bytes, reqwest::Error>(Bytes::from(c.to_owned()))),
        );
        let out: Vec<u8> = tap(src, tx)
            .map(|r| r.expect("chunk"))
            .collect::<Vec<_>>()
            .await
            .concat();
        (out, rx.try_recv().ok())
    }

    #[tokio::test]
    async fn passes_bytes_verbatim_and_observes_usage() {
        let sse = format!(
            "{}event: message_stop\ndata: {{}}\n\n",
            message_start(100, 250, 7)
        );
        let (out, observed) = run(vec![&sse]).await;
        assert_eq!(out, sse.as_bytes(), "bytes pass through verbatim");
        let observed = observed.expect("observation");
        assert_eq!(observed.usage.cache_creation_input_tokens, TokenCount(100));
        assert_eq!(observed.usage.cache_read_input_tokens, TokenCount(250));
        assert_eq!(observed.usage.input_tokens, TokenCount(7));
        assert_eq!(observed.model.as_str(), "claude-opus-4-20250514");
    }

    #[tokio::test]
    async fn observes_across_a_split_event() {
        let sse = message_start(0, 0, 13);
        let (head, tail) = sse.split_at(40);
        let (out, observed) = run(vec![head, tail]).await;
        assert_eq!(out, sse.as_bytes());
        let observed = observed.expect("observation");
        assert_eq!(observed.usage.input_tokens, TokenCount(13));
    }

    #[tokio::test]
    async fn garbage_stream_passes_through_without_observation() {
        let garbage = "x".repeat(8 * 1024);
        let (out, observed) = run(vec![&garbage]).await;
        assert_eq!(out, garbage.as_bytes());
        assert!(observed.is_none(), "no message_start, no observation");
    }

    #[tokio::test]
    async fn no_newline_flood_past_cap_gives_up_cleanly() {
        let flood = "x".repeat(SCAN_CAP + 4096);
        let (out, observed) = run(vec![&flood]).await;
        assert_eq!(out, flood.as_bytes(), "flood still passes through");
        assert!(observed.is_none());
    }
}
