// Ported from CLIProxyAPI internal/runtime/executor/meta_executor_execute.go
// and meta_executor_stream.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Keep subscription telemetry out of client responses, preserving event boundaries.

use open_ferry_translate::go::trim_space;
use serde_json::Value;

#[derive(Default)]
pub(super) struct Filter {
    pending_event: Option<Vec<u8>>,
    filtering: bool,
    skip_blank: bool,
}

impl Filter {
    pub(super) fn line(&mut self, line: Vec<u8>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let trimmed = trim_space(&line);
        if let Some(event) = trimmed.strip_prefix(b"event:") {
            if let Some(pending) = self.pending_event.take() {
                out.push(pending);
            }
            self.filtering = trim_space(event) == b"response.subscription_usage";
            self.skip_blank = self.filtering;
            if !self.filtering {
                self.pending_event = Some(line);
            }
            return out;
        }
        let Some(data) = trimmed.strip_prefix(b"data:") else {
            if trimmed.is_empty() {
                self.filtering = false;
                if std::mem::take(&mut self.skip_blank) {
                    return out;
                }
            }
            if self.filtering {
                return out;
            }
            self.skip_blank = false;
            if let Some(pending) = self.pending_event.take() {
                out.push(pending);
            }
            out.push(line);
            return out;
        };
        let event = serde_json::from_slice::<Value>(trim_space(data)).ok();
        let kind = event
            .as_ref()
            .and_then(|event| event["type"].as_str())
            .unwrap_or_default();
        if kind == "response.subscription_usage"
            || (kind.is_empty()
                && event
                    .as_ref()
                    .is_some_and(|event| event["subscription"].is_object()))
        {
            self.pending_event = None;
            self.filtering = false;
            self.skip_blank = true;
            return out;
        }
        if self.filtering && kind.is_empty() {
            self.pending_event = None;
            if event.is_some() {
                self.filtering = false;
            }
            self.skip_blank = true;
            return out;
        }
        self.filtering = false;
        self.skip_blank = false;
        if matches!(kind, "error" | "response.failed") {
            // stream_event_error publishes the failure before emitting a held header.
            self.pending_event = None;
        }
        if let Some(pending) = self.pending_event.take() {
            out.push(pending);
        }
        out.push(line);
        out
    }

    pub(super) fn finish(&mut self) -> Option<Vec<u8>> {
        self.pending_event.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_a_held_header_before_an_error_payload() {
        for kind in ["error", "response.failed"] {
            let mut filter = Filter::default();
            assert!(filter.line(b"event: error".to_vec()).is_empty());
            let data =
                format!("data: {{\"type\":\"{kind}\",\"error\":{{\"message\":\"failed\"}}}}")
                    .into_bytes();
            assert_eq!(filter.line(data.clone()), vec![data]);
            assert!(filter.finish().is_none());
        }
    }

    #[test]
    fn filters_all_subscription_forms_without_eating_adjacent_output() {
        for private in [
            "event: response.subscription_usage\ndata: {\"remaining\":100}\n\n",
            "event: response.subscription_usage\n\n",
            "event: private\ndata: {\"type\":\"response.subscription_usage\"}\n\n",
            "data: {\"subscription\":{\"remaining\":100}}\n\n",
            "event: response.subscription_usage\ndata: {\ndata: \"subscription\":{}}\n\n",
        ] {
            let public = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
            let input = format!("{public}{private}{public}");
            let mut filter = Filter::default();
            let mut output = Vec::new();
            for line in input.split_terminator('\n') {
                for line in filter.line(line.as_bytes().to_vec()) {
                    output.extend(line);
                    output.push(b'\n');
                }
            }
            assert!(filter.finish().is_none());
            assert_eq!(
                String::from_utf8(output).unwrap(),
                public.repeat(2),
                "{private}"
            );
        }
    }
}
