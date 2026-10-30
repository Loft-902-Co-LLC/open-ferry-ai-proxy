use super::*;

impl GeminiToOpenAIStream {
    pub(super) fn abort_streaming_call(&mut self, candidate: i64) {
        if self.active_calls.remove(&candidate).is_some() {
            self.incomplete_calls.insert(candidate);
        }
    }

    pub(super) fn function_call_delta(&mut self, candidate: i64, call: &Value) -> Option<Value> {
        let continuation = self
            .active_calls
            .get(&candidate)
            .is_some_and(|active| active.can_continue(call));
        if continuation {
            let mut active = self
                .active_calls
                .remove(&candidate)
                .expect("active continuation");
            let (delta, done) = match active.assembler.push(call, None) {
                Ok(result) => result,
                Err(_) => {
                    self.incomplete_calls.insert(candidate);
                    return None;
                }
            };
            let index = active.index;
            if !done {
                self.active_calls.insert(candidate, active);
            }
            return (!delta.is_empty())
                .then(|| json!({"index":index,"function":{"arguments":delta}}));
        }
        self.abort_streaming_call(candidate);
        let raw_name = str_of(call.get("name")).trim().to_owned();
        let streaming = is_streaming(call);
        if raw_name.is_empty() && (streaming || call.get("args").is_none()) {
            if streaming {
                self.incomplete_calls.insert(candidate);
            }
            return None;
        }
        let name = restore_sanitized_tool_name(self.sanitized_names.as_ref(), &raw_name);
        let id = supplied_or_generated_call_id(call, &name);
        let index = *self.function_index.entry(candidate).or_default();
        let arguments = if streaming {
            let mut active = StreamingCall::new(
                raw_name,
                str_of(call.get("id")).trim().to_owned(),
                id.clone(),
                index,
            );
            let (delta, done) = match active.assembler.push(call, None) {
                Ok(result) => result,
                Err(_) => {
                    self.incomplete_calls.insert(candidate);
                    return None;
                }
            };
            if !done {
                self.active_calls.insert(candidate, active);
            }
            delta
        } else {
            call.get("args").map_or_else(String::new, Value::to_string)
        };
        *self.function_index.entry(candidate).or_default() += 1;
        self.saw_tool_call.insert(candidate);
        Some(json!({"id":id,"index":index,"type":"function",
            "function":{"name":name,"arguments":arguments}}))
    }
}

#[cfg(test)]
mod tests;
