use super::*;

impl GeminiToClaudeStream {
    pub(super) fn abort_streaming_call(&mut self) {
        self.incomplete_call = true;
        if self.active_call.take().is_some() {
            self.pending_tool_signatures.clear();
            self.current_pending_empty = false;
            if self.block == Block::ToolUse {
                self.block_index += 1;
                self.block = Block::None;
            }
        }
    }

    pub(super) fn streaming_call(
        &mut self,
        call: &Value,
        signature: &str,
        out: &mut String,
    ) -> bool {
        let continuation = self
            .active_call
            .as_ref()
            .is_some_and(|active| active.can_continue(call));
        if !continuation && !is_streaming(call) {
            if self.active_call.is_some() {
                self.abort_streaming_call();
            }
            return false;
        }
        let mut active = if continuation {
            self.active_call.take().expect("active continuation")
        } else {
            if self.active_call.is_some() {
                self.abort_streaming_call();
            }
            let raw_name = str_of(call.get("name")).trim().to_owned();
            if raw_name.is_empty() {
                self.incomplete_call = true;
                return true;
            }
            let name = restore_sanitized_tool_name(self.sanitized_names.as_ref(), &raw_name);
            self.current_tool = map_tool_name(self.tool_names.as_ref(), &name);
            let count = TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            StreamingCall::new(
                raw_name,
                str_of(call.get("id")).trim().to_owned(),
                sanitize_tool_id(&format!("{name}-{count}")),
                self.block_index as i64,
            )
        };
        let clean = |args: &Value| {
            clean_arguments(
                args,
                &self.current_tool,
                &self.original_request,
                &self.translated_request,
            )
        };
        let (delta, done) = match active.assembler.push(call, Some(&clean)) {
            Ok(result) => result,
            Err(_) => {
                // The open block must not receive a successful stop on error.
                self.active_call = Some(active);
                self.abort_streaming_call();
                return true;
            }
        };
        self.saw_tool_call = true;
        if !continuation {
            self.push_carrier(signature, out);
            self.close_block(out);
            self.block = Block::None;
            active.index = self.block_index as i64;
            self.current_parameterless = parameterless(&self.original_request, &self.current_tool);
            self.current_input_emitted = false;
            self.current_pending_empty = false;
            self.push_block_start(
                json!({"type":"tool_use","id":active.id,
                "name":self.current_tool,"input":{}}),
                out,
            );
            self.block = Block::ToolUse;
            self.has_content = true;
        } else if !signature.is_empty() {
            self.pending_tool_signatures.push(signature.to_owned());
        }
        let delta = if self.current_parameterless {
            if done && !self.current_input_emitted {
                "{}"
            } else {
                ""
            }
        } else {
            &delta
        };
        if !delta.is_empty() {
            self.push_delta(json!({"type":"input_json_delta","partial_json":delta}), out);
            self.current_input_emitted = true;
            self.current_pending_empty = false;
        }
        if done {
            self.push_block_stop(out);
            self.block_index = active.index as usize + 1;
            self.block = Block::None;
            for signature in std::mem::take(&mut self.pending_tool_signatures) {
                self.push_carrier(&signature, out);
            }
        } else {
            self.active_call = Some(active);
        }
        true
    }
}

#[cfg(test)]
mod tests;
