//! SSE helpers. Anthropic and OpenAI streams are standard SSE and are parsed
//! with `eventsource-stream` (the `Eventsource` extension trait); Google
//! streams are `data: {json}` lines without event names and get a small
//! line-based parser here.

use anyhow::Result;
use futures_core::Stream;
use futures_util::StreamExt as _;

/// Adapter for eventsource-stream: yields (event_name, data) pairs.
pub fn parse_sse<S, E>(stream: S) -> impl Stream<Item = Result<(String, String)>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    use eventsource_stream::Eventsource as _;
    stream.eventsource().map(|r| {
        let ev: eventsource_stream::Event = r.map_err(|e| anyhow::anyhow!("sse error: {e}"))?;
        Ok((ev.event, ev.data))
    })
}

/// Parse a Google-style SSE payload where each `data:` line is a JSON object.
/// Returns raw JSON strings for the caller to deserialize.
#[allow(dead_code)]
pub fn parse_json_lines<S, E>(stream: S) -> impl Stream<Item = Result<String>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    futures_util::stream::unfold(
        (Box::pin(stream), String::new()),
        |(mut stream, mut buf)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        let text = String::from_utf8_lossy(&chunk);
                        buf.push_str(&text);
                        // Split complete lines.
                        if let Some(pos) = buf.find('\n') {
                            let line = buf[..pos].trim().to_string();
                            let rest = buf[pos + 1..].to_string();
                            buf = rest;
                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if !data.is_empty() && data != "[DONE]" {
                                    return Some((Ok(data.to_string()), (stream, buf)));
                                }
                            }
                            continue;
                        }
                    }
                    Some(Err(e)) => {
                        return Some((Err(anyhow::anyhow!("stream error: {e}")), (stream, buf)))
                    }
                    None => {
                        let rest = buf.trim().to_string();
                        if let Some(data) = rest.strip_prefix("data:") {
                            let data = data.trim().to_string();
                            if !data.is_empty() && data != "[DONE]" {
                                return Some((Ok(data), (stream, String::new())));
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Cloud Code PA streams a **JSON array** piecewise: `[{...},{...}]` with
/// chunks split at arbitrary byte boundaries. This parser extracts each
/// complete top-level object as it arrives (string-aware, so braces inside
/// JSON strings don't break the framing).
pub fn parse_json_array_chunks<S, E>(stream: S) -> impl Stream<Item = Result<String>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    futures_util::stream::unfold(
        (Box::pin(stream), JsonArrayState::default()),
        |(mut stream, mut st)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        let text = String::from_utf8_lossy(&chunk);
                        for ch in text.chars() {
                            if let Some(obj) = st.push(ch) {
                                return Some((Ok(obj), (stream, st)));
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Some((Err(anyhow::anyhow!("stream error: {e}")), (stream, st)));
                    }
                    None => {
                        if let Some(obj) = st.finish() {
                            return Some((Ok(obj), (stream, st)));
                        }
                        return None;
                    }
                }
            }
        },
    )
}

#[derive(Default)]
struct JsonArrayState {
    buf: String,
    depth: i32,
    in_string: bool,
    escaped: bool,
    started: bool,
}

impl JsonArrayState {
    fn push(&mut self, ch: char) -> Option<String> {
        if !self.started {
            if ch == '[' {
                self.started = true;
            }
            return None; // skip any preamble before the array
        }
        if self.in_string {
            self.buf.push(ch);
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '"' {
                self.in_string = false;
            }
            return None;
        }
        match ch {
            '"' => {
                self.in_string = true;
                self.buf.push(ch);
                None
            }
            '{' => {
                self.depth += 1;
                self.buf.push(ch);
                None
            }
            '}' => {
                self.depth -= 1;
                self.buf.push(ch);
                if self.depth == 0 {
                    Some(std::mem::take(&mut self.buf))
                } else {
                    None
                }
            }
            _ => {
                if self.depth > 0 {
                    self.buf.push(ch);
                }
                None
            }
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.depth == 0 {
            let obj = std::mem::take(&mut self.buf);
            if obj.trim().is_empty() {
                None
            } else {
                Some(obj)
            }
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn byte_stream(
        chunks: Vec<&'static str>,
    ) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> {
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Bytes::from(c))),
        )
    }

    #[tokio::test]
    async fn parse_standard_sse() {
        let raw = "event: message_start\ndata: {\"x\":1}\n\nevent: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut s = Box::pin(parse_sse(byte_stream(vec![raw])));
        let (ev, data) = s.next().await.unwrap().unwrap();
        assert_eq!(ev, "message_start");
        assert!(data.contains("\"x\":1"));
        let (ev, data) = s.next().await.unwrap().unwrap();
        assert_eq!(ev, "content_block_delta");
        assert!(data.contains("text_delta"));
        let (ev, _) = s.next().await.unwrap().unwrap();
        assert_eq!(ev, "message_stop");
    }

    #[tokio::test]
    async fn parse_google_json_lines_split_across_chunks() {
        // Line deliberately split across two chunks.
        let chunks = vec!["data: {\"candid", "ates\":[1]}\n\ndata: [DONE]\n\n"];
        let mut s = Box::pin(parse_json_lines(byte_stream(chunks)));
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(first, "{\"candidates\":[1]}");
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_json_array_chunks_incremental() {
        // Cloud Code PA format: a JSON array streamed in awkward splits.
        let chunks = vec![
            "[{\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"te",
            "xt\":\"hi\"}]}}]}}",
            ",{\"response\":{\"candidates\":[]}}",
            "]",
        ];
        let mut s = Box::pin(parse_json_array_chunks(byte_stream(chunks)));
        let first = s.next().await.unwrap().unwrap();
        assert!(first.contains("\"hi\""));
        let second = s.next().await.unwrap().unwrap();
        assert!(second.contains("candidates"));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_json_array_chunks_braces_in_strings() {
        let chunks = vec![
            r#"[{"response":{"candidates":[{"content":{"parts":[{"text":"a {b} c \"d\" [e]"}}]}}]}"#,
            "]",
        ];
        let mut s = Box::pin(parse_json_array_chunks(byte_stream(chunks)));
        let first = s.next().await.unwrap().unwrap();
        assert!(first.contains("a {b} c"));
        assert!(s.next().await.is_none());
    }
}
