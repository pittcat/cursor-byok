//! Accepts ordered Cursor Bidi append requests and routes them by request_id.
use prost::Message;

use crate::{
    cursor::{
        conversation::TransportCommand,
        protocol::{
            events,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        transport::{TransportParent, TransportRegistry},
    },
    Error, Result,
};

pub struct DecodedAppend {
    pub request_id: String,
    pub seqno: i64,
    pub message: agent::AgentClientMessage,
}

impl DecodedAppend {
    pub fn model_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request
            .requested_model
            .as_ref()
            .map(|model| model.model_id.as_str())
            .filter(|model| !model.is_empty())
            .or_else(|| {
                request
                    .model_details
                    .as_ref()
                    .map(|model| model.model_id.as_str())
                    .filter(|model| !model.is_empty())
            })
    }

    pub fn conversation_id(&self) -> Option<&str> {
        let agent::agent_client_message::Message::RunRequest(request) =
            self.message.message.as_ref()?
        else {
            return None;
        };
        request.conversation_id.as_deref()
    }

    pub fn is_background_task_completion(&self) -> bool {
        let Some(agent::agent_client_message::Message::RunRequest(request)) =
            self.message.message.as_ref()
        else {
            return false;
        };
        matches!(
            request
                .action
                .as_ref()
                .and_then(|action| action.action.as_ref()),
            Some(agent::conversation_action::Action::BackgroundTaskCompletionAction(_))
        )
    }

    pub fn trace_metadata(&self) -> serde_json::Value {
        let Some(message) = self.message.message.as_ref() else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": "empty",
            });
        };
        let agent::agent_client_message::Message::RunRequest(request) = message else {
            return serde_json::json!({
                "append_seqno": self.seqno,
                "message_type": client_message_type(message),
            });
        };
        let (action_type, history_messages, history_images) = request
            .action
            .as_ref()
            .and_then(|action| action.action.as_ref())
            .map(|action| match action {
                agent::conversation_action::Action::UserMessageAction(action) => {
                    let history = action.conversation_history.as_ref();
                    (
                        "user_message",
                        history.map_or(0, |history| history.messages.len()),
                        history.map_or(0, history_image_count),
                    )
                }
                agent::conversation_action::Action::BackgroundTaskCompletionAction(_) => {
                    ("background_task_completion", 0, 0)
                }
                agent::conversation_action::Action::ExecutePlanAction(_) => ("execute_plan", 0, 0),
                agent::conversation_action::Action::SummarizeAction(_) => ("summarize", 0, 0),
                _ => ("other", 0, 0),
            })
            .unwrap_or(("none", 0, 0));
        let state = request.conversation_state.as_ref();
        serde_json::json!({
            "append_seqno": self.seqno,
            "message_type": "run_request",
            "conversation_id": request.conversation_id,
            "model_id": self.model_id(),
            "action_type": action_type,
            "conversation_history_messages": history_messages,
            "conversation_history_images": history_images,
            "root_message_count": state.map_or(0, |state| state.root_prompt_messages_json.len()),
            "turn_count": state.map_or(0, |state| state.turns.len()),
            "prefetched_blob_count": request.pre_fetched_blobs.len(),
        })
    }
}

fn client_message_type(message: &agent::agent_client_message::Message) -> &'static str {
    use agent::agent_client_message::Message;
    match message {
        Message::RunRequest(_) => "run_request",
        Message::ExecClientMessage(_) => "exec_client_message",
        Message::ExecClientControlMessage(_) => "exec_client_control_message",
        Message::KvClientMessage(_) => "kv_client_message",
        Message::ConversationAction(_) => "conversation_action",
        Message::InteractionResponse(_) => "interaction_response",
        Message::ClientHeartbeat(_) => "client_heartbeat",
        Message::PrewarmRequest(_) => "prewarm_request",
    }
}

fn history_image_count(history: &agent::ConversationHistory) -> usize {
    use agent::{
        conversation_history_message::Message,
        conversation_history_tool_result_content::Content as ToolContent,
        conversation_history_user_content::Content as UserContent,
    };
    history
        .messages
        .iter()
        .map(|message| match message.message.as_ref() {
            Some(Message::User(user)) => user
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(UserContent::Image(_))))
                .count(),
            Some(Message::Tool(tool)) => tool
                .content
                .iter()
                .filter(|content| matches!(content.content, Some(ToolContent::Image(_))))
                .count(),
            _ => 0,
        })
        .sum()
}

pub fn decode(request: &ai::BidiAppendRequest) -> Result<DecodedAppend> {
    let request_id = request
        .request_id
        .as_ref()
        .map(|id| id.request_id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::Protocol("BidiAppend request_id is required".into()))?;
    if !request.data_binary.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend data_binary is not part of the captured protocol".into(),
        ));
    }
    if request.data.is_empty() {
        return Err(Error::Protocol(
            "BidiAppend contains no AgentClientMessage".into(),
        ));
    }
    let payload = hex::decode(&request.data)
        .map_err(|error| Error::Protocol(format!("invalid BidiAppend hex: {error}")))?;
    let message = match agent::AgentClientMessage::decode(payload.as_slice()) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(%error, "AgentClientMessage decode failed; salvaging fields");
            salvage(&payload)
                .ok_or_else(|| Error::Protocol(format!("failed to decode Protobuf message: {error}")))?
        }
    };
    Ok(DecodedAppend {
        request_id: request_id.into(),
        seqno: request.append_seqno,
        message,
    })
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let (mut value, mut shift) = (0u64, 0u32);
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn fields(buf: &[u8]) -> Vec<(u32, u8, &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let Some(tag) = read_varint(buf, &mut pos) else { break };
        let (number, wire) = (tag >> 3, (tag & 7) as u8);
        let len = match wire {
            0 => match read_varint(buf, &mut pos) {
                Some(_) => 0,
                None => break,
            },
            1 => 8,
            5 => 4,
            2 => match read_varint(buf, &mut pos) {
                Some(len) => len as usize,
                None => break,
            },
            _ => break,
        };
        if pos + len > buf.len() {
            break;
        }
        out.push((number as u32, wire, &buf[pos..pos + len]));
        pos += len;
    }
    out
}

fn salvage(payload: &[u8]) -> Option<agent::AgentClientMessage> {
    for (number, wire, value) in fields(payload) {
        if number == 1 && wire == 2 {
            let request = salvage_run_request(value);
            return Some(agent::AgentClientMessage {
                message: Some(agent::agent_client_message::Message::RunRequest(request)),
            });
        }
    }
    Some(agent::AgentClientMessage { message: None })
}

fn salvage_run_request(buf: &[u8]) -> agent::AgentRunRequest {
    let mut request = agent::AgentRunRequest::default();
    for (number, wire, value) in fields(buf) {
        if wire != 2 {
            continue;
        }
        match number {
            1 => {
                if let Ok(state) = agent::ConversationStateStructure::decode(value) {
                    request.conversation_state = Some(state);
                }
            }
            3 => {
                if let Ok(details) = agent::ModelDetails::decode(value) {
                    request.model_details = Some(details);
                }
            }
            9 => {
                if let Ok(model) = agent::RequestedModel::decode(value) {
                    request.requested_model = Some(model);
                }
            }
            _ => {
                if request.action.is_none() {
                    if let Ok(action) = agent::ConversationAction::decode(value) {
                        request.action = Some(action);
                    } else if let Some(text) = user_text(value) {
                        request.action = Some(user_message_action(text));
                    }
                }
            }
        }
    }
    request
}

fn user_text(buf: &[u8]) -> Option<String> {
    let parts = fields(buf);
    match parts.as_slice() {
        [(_, 0, _), (_, 2, text)] => {
            String::from_utf8(text.to_vec()).ok().filter(|text| !text.is_empty())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salvages_cursor_327_run_request() {
        let payload = include_bytes!("../../../tests/fixtures/bidiappend-3.27.bin");
        let request = ai::BidiAppendRequest {
            data: hex::encode(payload),
            request_id: Some(ai::BidiRequestId {
                request_id: "test-request".into(),
            }),
            append_seqno: 0,
            data_binary: Vec::new(),
        };
        let decoded = decode(&request).expect("salvaged decode");
        assert_eq!(decoded.model_id(), Some("ca6ef05c4f6688be"));
        let Some(agent::agent_client_message::Message::RunRequest(run)) = decoded.message.message
        else {
            panic!("expected run request");
        };
        assert!(run.conversation_state.is_none());
        let Some(agent::conversation_action::Action::UserMessageAction(action)) =
            run.action.and_then(|action| action.action)
        else {
            panic!("expected user message action");
        };
        assert_eq!(action.user_message.unwrap().text, "hello");
    }
}

fn user_message_action(text: String) -> agent::ConversationAction {
    agent::ConversationAction {
        action: Some(agent::conversation_action::Action::UserMessageAction(
            agent::UserMessageAction {
                user_message: Some(agent::UserMessage {
                    text,
                    message_id: uuid::Uuid::new_v4().to_string(),
                    mode: agent::AgentMode::Agent as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

pub async fn append(
    registry: &TransportRegistry,
    request: DecodedAppend,
    parent: Option<TransportParent>,
) -> Result<ai::BidiAppendResponse> {
    let replace_closing = request.model_id().is_some();
    let handle = registry
        .get_or_create_for_append(&request.request_id, replace_closing)
        .await?;
    let _admission = handle.admit()?;
    if let Some(conversation_id) = request.conversation_id() {
        handle.set_conversation_id(conversation_id)?;
    }
    if let Some(parent) = parent {
        handle.set_parent(parent)?;
    }
    if matches!(
        request.message.message.as_ref(),
        Some(agent::agent_client_message::Message::ClientHeartbeat(_))
    ) {
        handle.emit(&events::heartbeat())?;
    }
    handle
        .command(TransportCommand::Append {
            seqno: request.seqno,
            message: Box::new(request.message),
        })
        .await?;
    Ok(ai::BidiAppendResponse {})
}
