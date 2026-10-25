//! OpenCode 2 session messages. Schema reference: packages/schema/src/session-message.ts
//! and packages/core/src/session/sql.ts in OpenCode v2.1.2.

use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::{millis_to_iso, part_records};
use crate::parser::{ExtractedContent, Target};
use crate::sessions::SessionFile;

struct Message {
    rowid: i64,
    id: String,
    kind: String,
    seq: i64,
    data: Value,
    created: i64,
    updated: i64,
}

struct Record {
    index: usize,
    slot: &'static str,
    complete: bool,
    content: ExtractedContent,
}

fn read_messages(
    conn: &Connection,
    session: &str,
    rowid: i64,
    updated: i64,
) -> rusqlite::Result<Vec<Message>> {
    // Inclusive timestamp catches distinct updates committed within one millisecond.
    let mut stmt = conn.prepare(
        "SELECT rowid, id, type, seq, data, time_created, time_updated \
        FROM session_message WHERE session_id=?1 AND (rowid>?2 OR time_updated>=?3) ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![session, rowid, updated], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    let mut messages = vec![];
    for row in rows {
        let (rowid, id, kind, seq, data, created, updated) = row?;
        match serde_json::from_str::<Value>(&data) {
            Ok(data) if data.is_object() => messages.push(Message {
                rowid,
                id,
                kind,
                seq,
                data,
                created,
                updated,
            }),
            result => eprintln!(
                "warning: opencode v2 session {session} message {id}: invalid message object ({})",
                result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "expected JSON object".into())
            ),
        }
    }
    Ok(messages)
}

pub(super) fn extract(
    conn: &Connection,
    session: &SessionFile,
    targets: &HashSet<Target>,
    keep_raw: bool,
) -> rusqlite::Result<Vec<ExtractedContent>> {
    Ok(read_messages(conn, &session.session_id, 0, 0)?
        .iter()
        .flat_map(|m| records(m, session, targets, keep_raw))
        .map(|r| r.content)
        .collect())
}

struct Mapper<'a> {
    message: &'a Message,
    session: &'a SessionFile,
    targets: &'a HashSet<Target>,
    keep_raw: bool,
    out: Vec<Record>,
}

impl Mapper<'_> {
    fn push(
        &mut self,
        index: usize,
        slot: &'static str,
        part_type: &str,
        complete: bool,
        mut content: ExtractedContent,
    ) {
        if !self.targets.contains(&content.target) || content.text.is_empty() {
            return;
        }
        if self.keep_raw {
            // Keep the entire native message, including tool results and metadata.
            // itemType preserves its native kind when type is normalized to a role.
            let mut raw = self.message.data.clone();
            let role = match self.message.kind.as_str() {
                "synthetic" => "user",
                "shell" => "assistant",
                kind => kind,
            };
            raw["id"] = json!(self.message.id);
            raw["seq"] = json!(self.message.seq);
            raw["sessionId"] = json!(self.session.session_id);
            raw["type"] = json!(role);
            raw["role"] = json!(role);
            raw["itemType"] = json!(self.message.kind);
            raw["partType"] = json!(part_type);
            raw["contentIndex"] = json!(index);
            raw["slot"] = json!(slot);
            raw["timestamp"] = json!(content.timestamp);
            content.raw_entry = Some(raw);
        }
        self.out.push(Record {
            index,
            slot,
            complete,
            content,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        index: usize,
        slot: &'static str,
        target: Target,
        subtype: Option<&str>,
        text: String,
        time: i64,
        complete: bool,
    ) {
        self.push(
            index,
            slot,
            slot,
            complete,
            ExtractedContent {
                target,
                text,
                tool_name: subtype.map(str::to_string),
                timestamp: millis_to_iso(time),
                session_id: self.session.session_id.clone(),
                edit_diff: None,
                raw_entry: None,
            },
        );
    }

    fn tool(&mut self, index: usize, block: &Value) {
        let state = &block["state"];
        // Streaming input is an unfinished JSON string, not a callable argument object.
        if state["status"] == "streaming" {
            return;
        }
        let complete = matches!(state["status"].as_str(), Some("completed" | "error"));
        let mut output = text_content(&state["content"]);
        if state["status"] == "error" && !state["error"].is_null() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&error_text(&state["error"]));
        }
        let shell = block["name"] == "shell";
        let part = json!({"type":"tool", "tool":if shell { json!("bash") } else { block["name"].clone() },
            "state":{"input":state["input"], "output":if complete { output } else { String::new() }}});
        let start = time(block, "ran", time(block, "created", self.message.created));
        let end = time(block, "completed", self.message.updated);
        // Reuse v1's target splitting and edit-diff renderer, then restore v2's
        // native JSON envelope and timestamps (which live on the content block).
        for (slot, mut content) in part_records(
            &part,
            "assistant",
            &self.session.session_id,
            self.session.is_subagent,
            self.targets,
            false,
            start,
            end,
        ) {
            content.timestamp = millis_to_iso(if slot == "out" { end } else { start });
            if shell {
                content.tool_name = Some("shell".into());
            }
            if slot == "use" && content.text.is_empty() {
                content.text = "{}".into();
            }
            self.push(index, slot, "tool", slot != "out" || complete, content);
        }
    }
}

fn time(data: &Value, key: &str, fallback: i64) -> i64 {
    data["time"][key].as_i64().unwrap_or(fallback)
}

fn text_content(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn error_text(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error["message"].as_str())
        .or_else(|| error["data"]["message"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn records(
    message: &Message,
    session: &SessionFile,
    targets: &HashSet<Target>,
    keep_raw: bool,
) -> Vec<Record> {
    let data = &message.data;
    let created = time(data, "created", message.created);
    let mut m = Mapper {
        message,
        session,
        targets,
        keep_raw,
        out: vec![],
    };
    match message.kind.as_str() {
        "user" | "synthetic" => {
            let target = if session.is_subagent {
                Target::SubagentPrompt
            } else {
                Target::User
            };
            m.text(
                0,
                "text",
                target,
                None,
                data["text"].as_str().unwrap_or("").into(),
                created,
                true,
            );
            for (index, file) in data["files"].as_array().into_iter().flatten().enumerate() {
                m.text(
                    index,
                    "file",
                    Target::Attachment,
                    Some("file"),
                    file.to_string(),
                    created,
                    true,
                );
            }
        }
        "assistant" => {
            // Content blocks are mutable while the model streams. Follow emits text
            // after streaming ends; tools can settle later, independently.
            let complete =
                data["time"]["completed"].is_number() || data["time"]["streamed"].is_number();
            for (index, block) in data["content"].as_array().into_iter().flatten().enumerate() {
                match block["type"].as_str() {
                    Some("text" | "reasoning") => {
                        let reasoning = block["type"] == "reasoning";
                        m.text(
                            index,
                            if reasoning { "reasoning" } else { "text" },
                            if reasoning {
                                Target::Thinking
                            } else {
                                Target::Assistant
                            },
                            None,
                            block["text"].as_str().unwrap_or("").into(),
                            time(block, "created", created),
                            complete || block["time"]["completed"].is_number(),
                        );
                    }
                    Some("tool") => m.tool(index, block),
                    other => eprintln!(
                        "warning: opencode v2 message {}: unknown content type {:?}",
                        message.id, other
                    ),
                }
            }
            if !data["error"].is_null() {
                m.text(
                    0,
                    "error",
                    Target::System,
                    Some("error"),
                    error_text(&data["error"]),
                    message.updated,
                    true,
                );
            }
        }
        "shell" => {
            m.text(
                0,
                "cmd",
                Target::BashCommand,
                Some("shell"),
                data["command"].as_str().unwrap_or("").into(),
                created,
                true,
            );
            m.text(
                0,
                "out",
                Target::BashOutput,
                Some("shell"),
                data["output"]["output"].as_str().unwrap_or("").into(),
                time(data, "completed", message.updated),
                data["status"] != "running",
            );
        }
        "compaction" => {
            m.text(
                0,
                "summary",
                Target::CompactSummary,
                None,
                data["summary"].as_str().unwrap_or("").into(),
                created,
                data["status"] != "running",
            );
            let recent = data["metadata"]["recent"]
                .as_str()
                .or_else(|| data["recent"].as_str())
                .unwrap_or("");
            m.text(
                0,
                "recent",
                Target::CompactSummary,
                None,
                recent.into(),
                created,
                data["status"] != "running",
            );
            if !data["error"].is_null() {
                m.text(
                    0,
                    "error",
                    Target::System,
                    Some("compaction"),
                    error_text(&data["error"]),
                    message.updated,
                    true,
                );
            }
        }
        "system" | "skill" => m.text(
            0,
            "text",
            Target::System,
            Some(&message.kind),
            data["text"].as_str().unwrap_or("").into(),
            created,
            true,
        ),
        "agent-switched" => m.text(
            0,
            "agent",
            Target::AgentName,
            None,
            data["agent"].as_str().unwrap_or("").into(),
            created,
            true,
        ),
        "model-switched" | "location-switched" | "idle" => m.text(
            0,
            "event",
            Target::System,
            Some(&message.kind),
            data.to_string(),
            created,
            true,
        ),
        other => eprintln!(
            "warning: opencode v2 message {}: unknown message type {}",
            message.id, other
        ),
    }
    m.out
}

#[derive(Default)]
struct Follow {
    rowid: i64,
    updated: i64,
    emitted: HashSet<(String, usize, &'static str)>,
}

impl Follow {
    fn poll(
        &mut self,
        conn: &Connection,
        session: &SessionFile,
        targets: &HashSet<Target>,
    ) -> rusqlite::Result<Vec<ExtractedContent>> {
        let messages = read_messages(conn, &session.session_id, self.rowid, self.updated)?;
        let mut out = vec![];
        for message in messages {
            self.rowid = self.rowid.max(message.rowid);
            self.updated = self.updated.max(message.updated);
            for record in records(&message, session, targets, true) {
                if record.complete
                    && self
                        .emitted
                        .insert((message.id.clone(), record.index, record.slot))
                {
                    out.push(record.content);
                }
            }
        }
        Ok(out)
    }
}

pub(super) fn follow(
    conn: &Connection,
    session: &SessionFile,
    targets: &HashSet<Target>,
    on_records: &mut dyn FnMut(&[ExtractedContent]),
) -> rusqlite::Result<()> {
    let mut cursor = Follow::default();
    // Seed completed facets only: pending tool output and partially streamed
    // text in an existing row must still be emitted when they finish.
    cursor.poll(conn, session, targets)?;
    loop {
        let batch = cursor.poll(conn, session, targets)?;
        if !batch.is_empty() {
            on_records(&batch);
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_handles_existing_streams_late_tools_and_same_millisecond_updates() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT,
            type TEXT, seq INTEGER, data TEXT, time_created INTEGER, time_updated INTEGER)",
        )
        .unwrap();
        let session = SessionFile {
            session_id: "ses_v2".into(),
            file_path: "unused".into(),
            mtime: std::time::UNIX_EPOCH,
            is_subagent: false,
            backend: super::super::OPENCODE,
        };
        let targets = [
            Target::Assistant,
            Target::Thinking,
            Target::ToolUse,
            Target::BashCommand,
            Target::BashOutput,
        ]
        .into_iter()
        .collect();
        let mut data = json!({"time":{"created":1000}, "content":[
            {"type":"text","text":"partial"},
            {"type":"tool","name":"shell","id":"tool1", "state":{"status":"running","input":{"command":"ls"}}}
        ]});
        conn.execute(
            "INSERT INTO session_message VALUES ('msg_a','ses_v2','assistant',1,?1,1000,1000)",
            [data.to_string()],
        )
        .unwrap();
        let mut cursor = Follow::default();
        let seed = cursor.poll(&conn, &session, &targets).unwrap();
        assert_eq!(seed.len(), 2); // Existing command and tool input, not unfinished text.
        assert!(cursor.poll(&conn, &session, &targets).unwrap().is_empty());

        data["content"][0]["text"] = json!("complete first text");
        data["time"]["streamed"] = json!(1001);
        data["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"second text"}));
        // Deliberately do not advance time_updated: an inclusive poll must catch this.
        conn.execute(
            "UPDATE session_message SET data=?1 WHERE id='msg_a'",
            [data.to_string()],
        )
        .unwrap();
        let output = cursor.poll(&conn, &session, &targets).unwrap();
        assert_eq!(
            output.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["complete first text", "second text"]
        );

        // Another row advances the cursor before an older tool finishes.
        conn.execute(
            "INSERT INTO session_message VALUES ('msg_b','ses_v2','assistant',2,?1,2000,2000)",
            [
                json!({"time":{"completed":2000},"content":[{"type":"text","text":"new message"}]})
                    .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            cursor.poll(&conn, &session, &targets).unwrap()[0].text,
            "new message"
        );
        data["content"][1]["state"]["status"] = json!("completed");
        data["content"][1]["state"]["content"] = json!([{"type":"text","text":"late output"}]);
        conn.execute(
            "UPDATE session_message SET data=?1,time_updated=2000 WHERE id='msg_a'",
            [data.to_string()],
        )
        .unwrap();
        let output = cursor.poll(&conn, &session, &targets).unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].target, Target::BashOutput);
        assert_eq!(output[0].text, "late output");
        assert!(cursor.poll(&conn, &session, &targets).unwrap().is_empty());
    }
}
