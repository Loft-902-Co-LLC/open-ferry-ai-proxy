use super::*;

pub(super) struct ActiveCall {
    pub(super) call: StreamingCall,
    patch: Option<CallState>,
    evidence_index: usize,
}

impl GeminiToOpenAIResponsesStream {
    pub(super) fn abort_streaming_call(&mut self) {
        if self.active_call.take().is_some() {
            self.incomplete_call = true;
        }
    }

    pub(super) fn streaming_call(
        &mut self,
        call: &Value,
        explicit_index: i64,
        continuation: bool,
        out: &mut String,
    ) -> bool {
        let mut active = if continuation {
            self.active_call.take().expect("active continuation")
        } else {
            self.abort_streaming_call();
            let raw_name = str_of(call.get("name")).trim().to_owned();
            if raw_name.is_empty() {
                self.incomplete_call = true;
                return true;
            }
            let identity = self.identity(&raw_name);
            let evidence_index =
                match self
                    .evidence
                    .record(&self.identities, call, None, explicit_index)
                {
                    Ok(index) => index,
                    Err(error) => {
                        self.fail(error, out);
                        return false;
                    }
                };
            let evidence = &self.evidence.items[evidence_index];
            if evidence.apply_patch {
                if let Some(error) = evidence.error.clone() {
                    self.fail(error, out);
                    return false;
                }
                // Complete snapshots are authoritative, not prefixes to extend.
                if evidence.input.is_some() || evidence.patch_call.is_some() {
                    self.fail(
                        ToolInputError::Conflict(
                            "partial arguments after a complete apply_patch snapshot",
                        ),
                        out,
                    );
                    return false;
                }
            }
            let upstream_id = str_of(call.get("id")).trim().to_owned();
            let id = if upstream_id.is_empty() {
                new_stream_call_id()
            } else {
                upstream_id.clone()
            };
            let index = self.next_index;
            self.next_index += 1;
            let item_id = format!("{}_{id}", if identity.custom { "ctc" } else { "fc" });
            let patch = identity
                .apply_patch
                .then(|| CallState::new(item_id.clone(), id.clone(), index));
            let mut item = if identity.tool_search {
                super::super::tool_search::call_item(&id, "in_progress", "{}")
            } else {
                let mut item = json!({"id":item_id,"type":if identity.custom {"custom_tool_call"} else {"function_call"},
                    "status":"in_progress"});
                item[if identity.custom {
                    "input"
                } else {
                    "arguments"
                }] = "".into();
                item["call_id"] = id.clone().into();
                set_tool_call_identity(&mut item, &identity.name, &identity.namespace);
                item
            };
            // Move the already-built item instead of serializing arguments again.
            let mut added = json!({"type":"response.output_item.added","sequence_number":self.next_seq(),"output_index":index});
            added["item"] = std::mem::take(&mut item);
            push_event(out, "response.output_item.added", &added);
            self.calls.insert(
                index,
                Call {
                    tool_search: identity.tool_search,
                    call_id: id.clone(),
                    name: identity.name,
                    namespace: identity.namespace,
                    custom: identity.custom,
                    args: String::new(),
                    input: String::new(),
                },
            );
            ActiveCall {
                call: StreamingCall::new(raw_name, upstream_id, id, index),
                patch,
                evidence_index,
            }
        };
        let index = active.call.index;
        if active.patch.is_some() {
            let evidence = &self.evidence.items[active.evidence_index];
            if explicit_index >= 0
                && evidence
                    .part_index
                    .is_some_and(|previous| previous != explicit_index)
            {
                self.fail(
                    ToolInputError::Conflict("conflicting apply_patch part index"),
                    out,
                );
                return false;
            }
            let id = str_of(call.get("id")).trim().to_owned();
            let mut keys = Vec::new();
            if !id.is_empty() {
                keys.push(format!("id:{id}"));
            }
            if explicit_index >= 0 {
                keys.push(format!("part:{explicit_index}"));
            }
            for key in keys {
                if self
                    .evidence
                    .by_key
                    .get(&key)
                    .is_some_and(|prior| *prior != active.evidence_index)
                {
                    self.fail(
                        ToolInputError::Conflict("conflicting apply_patch call indexes"),
                        out,
                    );
                    return false;
                }
                self.evidence.by_key.insert(key, active.evidence_index);
            }
            let evidence = &mut self.evidence.items[active.evidence_index];
            if explicit_index >= 0 {
                evidence.part_index = Some(explicit_index);
            }
            if !id.is_empty() {
                active.call.upstream_id = id.clone();
                evidence.upstream_id = id;
            }
        }
        let (delta, done) = match active.call.assembler.push(call, None) {
            Ok(result) => result,
            Err(error) => {
                self.incomplete_call = true;
                if active.patch.is_some() {
                    self.fail(ToolInputError::Vertex(error), out);
                    return false;
                }
                return true;
            }
        };
        let stored = self.calls.get_mut(&index).expect("streaming call item");
        stored.args.push_str(&delta);
        let custom = stored.custom;
        let tool_search = stored.tool_search;
        if !delta.is_empty() {
            if let Some(patch) = &mut active.patch {
                match patch.push_arguments(&delta) {
                    Ok(input) if !input.is_empty() => {
                        let seq = self.next_seq();
                        push_event(
                            out,
                            "response.custom_tool_call_input.delta",
                            &patch.input_delta(&input, seq),
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.fail(ToolInputError::Arguments(error), out);
                        return false;
                    }
                }
            } else if !custom && !tool_search {
                let seq = self.next_seq();
                push_event(
                    out,
                    "response.function_call_arguments.delta",
                    &json!({
                    "type":"response.function_call_arguments.delta","sequence_number":seq,
                    "item_id":format!("fc_{}", active.call.id),"output_index":index,"delta":delta}),
                );
            }
        }
        if !done {
            self.active_call = Some(active);
            return true;
        }
        let full = active
            .call
            .assembler
            .full_arguments()
            .expect("successful assembly");
        let call = self.calls.get(&index).expect("streaming call item");
        let mut item = if tool_search {
            super::super::tool_search::call_item(&call.call_id, self.tool_search_status, full)
        } else if custom {
            let input = if let Some(patch) = &mut active.patch {
                match patch.finish_arguments(full) {
                    Ok((tail, input)) => {
                        if !tail.is_empty() {
                            let seq = self.next_seq();
                            push_event(
                                out,
                                "response.custom_tool_call_input.delta",
                                &patch.input_delta(&tail, seq),
                            );
                        }
                        input
                    }
                    Err(error) => {
                        self.fail(ToolInputError::Arguments(error), out);
                        return false;
                    }
                }
            } else {
                unwrap_responses_custom_tool_input(full)
            };
            let seq = self.next_seq();
            let payload = if let Some(patch) = &active.patch {
                patch.input_done(&input, seq)
            } else {
                json!({"type":"response.custom_tool_call_input.done","sequence_number":seq,
                    "item_id":format!("ctc_{}",active.call.id),"output_index":index,"input":input})
            };
            push_event(out, "response.custom_tool_call_input.done", &payload);
            self.calls
                .get_mut(&index)
                .expect("streaming call item")
                .input = input.clone();
            if active.patch.is_some() {
                let evidence = &mut self.evidence.items[active.evidence_index];
                evidence.input = Some(input.clone());
                evidence.patch_call = active.patch.take();
            }
            json!({"id":format!("ctc_{}",active.call.id),"type":"custom_tool_call","status":"completed",
                "input":input,"call_id":active.call.id})
        } else {
            let seq = self.next_seq();
            push_event(
                out,
                "response.function_call_arguments.done",
                &json!({
                "type":"response.function_call_arguments.done","sequence_number":seq,
                "item_id":format!("fc_{}",active.call.id),"output_index":index,"arguments":full}),
            );
            json!({"id":format!("fc_{}",active.call.id),"type":"function_call","status":"completed",
                "arguments":full,"call_id":active.call.id})
        };
        let call = self.calls.get(&index).expect("streaming call item");
        if !tool_search {
            set_tool_call_identity(&mut item, &call.name, &call.namespace);
        }
        let mut payload = json!({"type":"response.output_item.done","sequence_number":self.next_seq(),"output_index":index});
        payload["item"] = item;
        push_event(out, "response.output_item.done", &payload);
        self.saw_completed_call = true;
        if !self.pending_signature.is_empty() {
            let signature = std::mem::take(&mut self.pending_signature);
            self.emit_trailing(&signature, out);
        }
        true
    }
}

#[cfg(test)]
mod tests;
