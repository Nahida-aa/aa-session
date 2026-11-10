//! CodeBuddy IDE 的消息文件 → Markdown 对话模型。
//!
//! 每个消息文件的 `message` 字段是一个 **JSON 字符串**，解析后是内容块数组：
//!
//! | 块类型 | 处理 |
//! | --- | --- |
//! | `text` | 用户文本新开 `## User`；助手文本追加到当前 `## Assistant` |
//! | `reasoning` | `<thinking>` 包起来，追加到当前助手消息 |
//! | `tool-call` | `ToolCall`，追加到当前助手消息并登记 `toolCallId` |
//! | `tool-result` | 按 `toolCallId` 回填到对应调用（`role == tool` 的文件） |
//! | `image` | 纯二进制附件，Markdown 无法呈现，跳过 |
//!
//! 与 CLI 版 codebuddy 的差别：IDE 已经是「一条消息一个文件」，不需要按事件
//! 归并；但工具结果仍单独成文件，所以要靠 `toolCallId` 把调用和结果接到一起。

use std::collections::HashMap;

use aa_session_core::markdown::{Message, Part, Role, ToolCall};
use serde_json::Value;

/// 从 [`crate::CodebuddyIdeSource::dump`] 的结果解析出对话。
pub fn from_dump(dump: &Value) -> Vec<Message> {
    let messages = dump["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();

    let mut out: Vec<Message> = Vec::new();
    // toolCallId → (消息下标, 片段下标)
    let mut calls: HashMap<String, (usize, usize)> = HashMap::new();

    for file in messages {
        let role = file["role"].as_str().unwrap_or_default();
        let Some(content) = inner_content(file) else {
            continue;
        };

        let mut user_parts: Vec<Part> = Vec::new();
        for block in &content {
            match block["type"].as_str().unwrap_or_default() {
                "text" => {
                    let text = block["text"].as_str().unwrap_or("");
                    if text.trim().is_empty() {
                        continue;
                    }
                    if role == "user" {
                        user_parts.push(Part::Text(text.to_string()));
                    } else {
                        let ix = assistant_index(&mut out);
                        out[ix].parts.push(Part::Text(text.to_string()));
                    }
                }
                "reasoning" => {
                    let text = block["text"].as_str().unwrap_or("");
                    if text.trim().is_empty() {
                        continue;
                    }
                    let ix = assistant_index(&mut out);
                    out[ix].parts.push(Part::Thinking(text.to_string()));
                }
                "tool-call" => {
                    let Some(tool) = tool_call(block) else {
                        continue;
                    };
                    let call_id = tool.id.clone();
                    let ix = assistant_index(&mut out);
                    let part_ix = out[ix].parts.len();
                    out[ix].parts.push(Part::Tool(tool));
                    calls.insert(call_id, (ix, part_ix));
                }
                "tool-result" => {
                    let call_id = block["toolCallId"].as_str().unwrap_or_default().to_string();
                    let Some((message_ix, part_ix)) = calls.get(&call_id).copied() else {
                        // 孤立结果（截断的存储）：保留正文，别让导出凭空少一段。
                        let mut tool = ToolCall::new(call_id, tool_name(block), Value::Null);
                        fill_result(&mut tool, block);
                        let ix = assistant_index(&mut out);
                        out[ix].parts.push(Part::Tool(tool));
                        continue;
                    };
                    if let Some(Part::Tool(tool)) = out
                        .get_mut(message_ix)
                        .and_then(|m| m.parts.get_mut(part_ix))
                    {
                        fill_result(tool, block);
                    }
                }
                _ => {}
            }
        }

        if role == "user" && !user_parts.is_empty() {
            out.push(Message::user(user_parts));
        }
    }

    out.retain(|message| !message.is_empty());
    out
}

/// 取出消息文件里 `message` 字段解析后的 `content` 数组。
///
/// 正常情况下 `message` 是 JSON 字符串；若上游改成直接给对象也兼容。
pub(crate) fn inner_content(file: &Value) -> Option<Vec<Value>> {
    let message = &file["message"];
    let parsed = match message {
        Value::String(raw) => serde_json::from_str::<Value>(raw).ok()?,
        Value::Object(_) => message.clone(),
        _ => return None,
    };
    parsed.get("content").and_then(Value::as_array).cloned()
}

fn assistant_index(out: &mut Vec<Message>) -> usize {
    if out.last().map(|m| m.role) != Some(Role::Assistant) {
        out.push(Message::assistant(Vec::new()));
    }
    out.len() - 1
}

fn tool_name(block: &Value) -> String {
    block["toolName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("tool")
        .to_string()
}

fn tool_call(block: &Value) -> Option<ToolCall> {
    let id = block["toolCallId"].as_str().unwrap_or_default();
    let name = tool_name(block);
    let input = match block.get("args") {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(value) => value.clone(),
        None => Value::Null,
    };
    Some(ToolCall::new(id, name, input))
}

fn fill_result(tool: &mut ToolCall, block: &Value) {
    let result = block.get("result").unwrap_or(&Value::Null);
    let status = result["status"].as_str();
    let failed = result["success"].as_bool() == Some(false)
        || matches!(status, Some("error" | "failed" | "failure"));

    tool.status = status.map(str::to_string).or_else(|| {
        result["success"]
            .as_bool()
            .map(|ok| if ok { "success" } else { "error" }.to_string())
    });

    let text = result_text(result);
    if failed {
        tool.error = text;
    } else {
        tool.output = text;
    }
}

/// 从工具结果里取可读正文。
///
/// 实测最常见的形态是 MCP 结果：
/// `{"status":"success","success":true,"result":{"data":[{"text":"…"}]}}`。
fn result_text(result: &Value) -> Option<String> {
    if let Some(items) = result.pointer("/result/data").and_then(Value::as_array) {
        let joined = items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }
    if let Some(text) = result
        .pointer("/result/text")
        .or_else(|| result.pointer("/output/text"))
        .and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Some(text.to_string());
    }
    if let Some(text) = result.get("result").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Some(text.to_string());
    }

    if result.is_null() {
        return None;
    }
    // 非文本结果（结构化数据 / 图片元信息）留一个 JSON 快照，别静默丢掉。
    serde_json::to_string_pretty(result).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(role: &str, content: Value) -> Value {
        json!({
            "role": role,
            "id": "m",
            "message": json!({"role": role, "content": content}).to_string(),
        })
    }

    #[test]
    fn groups_user_and_assistant_turns() {
        let dump = json!({
            "messages": [
                file("user", json!([{"type": "text", "text": "你好"}]),
                ),
                file("assistant", json!([
                    {"type": "reasoning", "text": "想一想"},
                    {"type": "tool-call", "toolCallId": "c1", "toolName": "read_file",
                     "args": {"path": "a.rs"}},
                    {"type": "text", "text": "看完了"}
                ])),
                file("tool", json!([{
                    "type": "tool-result", "toolCallId": "c1", "toolName": "read_file",
                    "result": {"status": "success", "success": true,
                               "result": {"data": [{"type": "text", "text": "文件内容"}]}}
                }])),
            ],
        });

        let messages = from_dump(&dump);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].parts.len(), 3);
        assert!(matches!(messages[1].parts[0], Part::Thinking(_)));
        let tool = match &messages[1].parts[1] {
            Part::Tool(tool) => tool,
            other => panic!("expected tool, got {other:?}"),
        };
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.input["path"], "a.rs");
        assert_eq!(tool.output.as_deref(), Some("文件内容"));
        assert_eq!(messages[1].parts[2], Part::Text("看完了".into()));
    }

    #[test]
    fn failed_tool_result_becomes_error() {
        let dump = json!({
            "messages": [
                file("user", json!([{"type": "text", "text": "跑一下"}]),
                ),
                file("assistant", json!([
                    {"type": "tool-call", "toolCallId": "c2", "toolName": "shell", "args": {}}
                ])),
                file("tool", json!([{
                    "type": "tool-result", "toolCallId": "c2", "toolName": "shell",
                    "result": {"status": "error", "success": false, "result": "boom"}
                }])),
            ],
        });
        let messages = from_dump(&dump);
        let tool = match &messages[1].parts[0] {
            Part::Tool(tool) => tool,
            other => panic!("expected tool, got {other:?}"),
        };
        assert_eq!(tool.status.as_deref(), Some("error"));
        assert_eq!(tool.error.as_deref(), Some("boom"));
        assert!(tool.output.is_none());
    }

    #[test]
    fn orphan_result_is_kept() {
        let dump = json!({
            "messages": [
                file("user", json!([{"type": "text", "text": "x"}]),
                ),
                file("tool", json!([{
                    "type": "tool-result", "toolCallId": "gone", "toolName": "read_file",
                    "result": {"status": "success", "success": true,
                               "result": {"data": [{"type": "text", "text": "残留输出"}]}}
                }])),
            ],
        });
        let messages = from_dump(&dump);
        let tool = match &messages[1].parts[0] {
            Part::Tool(tool) => tool,
            other => panic!("expected tool, got {other:?}"),
        };
        assert_eq!(tool.output.as_deref(), Some("残留输出"));
    }

    #[test]
    fn empty_content_is_skipped() {
        let dump = json!({
            "messages": [
                file("user", json!([{"type": "text", "text": "   "}])),
                file("assistant", json!([])),
            ],
        });
        assert!(from_dump(&dump).is_empty());
    }
}
