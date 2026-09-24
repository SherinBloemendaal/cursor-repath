//! What `ls` and `stats` know about one installation, read live or loaded from the index.

use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::fsops::dir_size;
use super::stats::{Usage, add_usage};
use super::view::{ListRow, header_workspace};
use super::{Kind, Runtime, Workspace, db, discover, open_global_ro, user_profiles};
use crate::cursor::install::UserProfile;
use crate::cursor::registry::{ComposerHeader, load_headers};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatUsage {
    pub model: Option<String>,
    pub cost_cents: u64,
    pub requests: u64,
    pub context_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Deserialize)]
struct Slim {
    #[serde(rename = "usageData", default)]
    usage_data: Option<Value>,
    #[serde(rename = "contextTokensUsed", default)]
    context_tokens_used: Option<Value>,
    #[serde(default)]
    conversation: Option<Vec<SlimMessage>>,
    #[serde(rename = "modelConfig", default)]
    model_config: Option<Value>,
}

#[derive(Deserialize)]
struct SlimMessage {
    #[serde(rename = "tokenCount", default)]
    token_count: Option<Value>,
}

impl Slim {
    fn into_json(self) -> Value {
        json!({
            "usageData": self.usage_data,
            "contextTokensUsed": self.context_tokens_used,
            "conversation": self.conversation.map(|messages| {
                messages
                    .into_iter()
                    .map(|message| json!({"tokenCount": message.token_count}))
                    .collect::<Vec<_>>()
            }),
            "modelConfig": self.model_config,
        })
    }
}

impl ChatUsage {
    pub fn from_json(json: &Value) -> Self {
        let mut usage = Usage::default();
        add_usage(&mut usage, json);
        Self {
            model: model_name(json),
            cost_cents: usage.cost_cents,
            requests: usage.requests,
            context_tokens: usage.context_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }
    }

    /// Usage of one `composerData` value. Large conversations are skimmed for the few fields
    /// that count, and anything unusual goes through the full JSON parse.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.trim_start().starts_with('{')
            && let Ok(slim) = serde_json::from_str::<Slim>(raw)
        {
            return Some(Self::from_json(&slim.into_json()));
        }
        serde_json::from_str::<Value>(raw)
            .ok()
            .map(|json| Self::from_json(&json))
    }
}

pub fn model_name(json: &Value) -> Option<String> {
    json.pointer("/modelConfig/modelName")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("modelConfig").and_then(|v| v.as_str()))
        .map(str::to_string)
}

pub fn read_usage(conn: &Connection, composer_id: &str) -> Result<Option<ChatUsage>> {
    Ok(db::read_text(conn, &format!("composerData:{composer_id}"))?
        .and_then(|raw| ChatUsage::parse(&raw)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFacts {
    pub id: String,
    pub workspace_id: String,
    pub is_subagent: bool,
    pub is_archived: bool,
    pub created_at: Option<i64>,
    pub last_updated_at: Option<i64>,
    pub title: Option<String>,
    pub origin_kind: Option<Kind>,
    pub origin_path: Option<PathBuf>,
    pub usage: Option<ChatUsage>,
}

impl ChatFacts {
    pub fn new(header: &ComposerHeader, usage: Option<ChatUsage>) -> Self {
        let (origin_kind, origin_path) = header_workspace(header);
        Self {
            id: header.composer_id.clone(),
            workspace_id: header.workspace_id.clone(),
            is_subagent: header.is_subagent,
            is_archived: header.is_archived,
            created_at: header.created_at,
            last_updated_at: header.last_updated_at,
            title: header.title.clone(),
            origin_kind,
            origin_path,
            usage,
        }
    }

    pub fn header(&self) -> ComposerHeader {
        ComposerHeader {
            composer_id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            created_at: self.created_at,
            last_updated_at: self.last_updated_at,
            is_archived: self.is_archived,
            is_subagent: self.is_subagent,
            recency: None,
            checkpoint_at: None,
            value: String::new(),
            subagent_type_name: None,
            title: self.title.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceFacts {
    pub workspace: Workspace,
    pub size: u64,
    pub local_db: Option<u64>,
}

impl WorkspaceFacts {
    pub fn read(workspace: Workspace, sizes: bool) -> Self {
        Self {
            size: if sizes {
                dir_size(&workspace.dir).unwrap_or(0)
            } else {
                0
            },
            local_db: file_len(&workspace.dir.join("state.vscdb")),
            workspace,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallFacts {
    pub name: String,
    pub root: PathBuf,
    pub global_db: Option<u64>,
    pub profiles: Vec<UserProfile>,
    pub workspaces: Vec<WorkspaceFacts>,
    pub chats: Vec<ChatFacts>,
}

impl InstallFacts {
    /// Workspaces of the runtime's VS Code profile, or all of them.
    pub fn workspaces_for<'a>(&'a self, rt: &Runtime) -> Vec<&'a WorkspaceFacts> {
        self.workspaces
            .iter()
            .filter(|facts| {
                rt.profile
                    .as_ref()
                    .is_none_or(|profile| &facts.workspace.profile == profile)
            })
            .collect()
    }

    pub fn list_rows(&self, rt: &Runtime, unsaved_only: bool) -> Vec<ListRow> {
        let mut counts: HashMap<&str, (usize, usize, usize)> = HashMap::new();
        for chat in &self.chats {
            let entry = counts.entry(chat.workspace_id.as_str()).or_default();
            entry.0 += 1;
            entry.1 += usize::from(chat.is_subagent);
            entry.2 += usize::from(chat.is_archived);
        }
        self.workspaces_for(rt)
            .into_iter()
            .filter(|facts| !unsaved_only || facts.workspace.kind == Kind::Unsaved)
            .map(|facts| {
                let (total, subagents, archived) = counts
                    .get(facts.workspace.id.as_str())
                    .copied()
                    .unwrap_or_default();
                ListRow {
                    workspace: facts.workspace.clone(),
                    chats: total.saturating_sub(subagents),
                    subagents,
                    archived,
                    size: facts.size,
                }
            })
            .collect()
    }

    pub fn headers_of(&self, workspace_id: &str) -> Vec<ComposerHeader> {
        self.chats
            .iter()
            .filter(|chat| chat.workspace_id == workspace_id)
            .map(ChatFacts::header)
            .collect()
    }
}

/// Everything `stats` needs, read straight from Cursor's files. Usage is only read for the
/// chats of the pinned VS Code profile.
pub fn read_live(rt: &Runtime, sizes: bool) -> Result<InstallFacts> {
    let workspaces: Vec<WorkspaceFacts> = discover(rt)?
        .into_iter()
        .map(|workspace| WorkspaceFacts::read(workspace, sizes))
        .collect();
    let wanted: Option<HashSet<String>> = rt.profile.as_ref().map(|profile| {
        workspaces
            .iter()
            .filter(|facts| &facts.workspace.profile == profile)
            .map(|facts| facts.workspace.id.clone())
            .collect()
    });
    let conn = open_global_ro(rt)?;
    let mut chats = Vec::new();
    if let Some(conn) = &conn {
        for header in load_headers(conn)? {
            let usage = if wanted
                .as_ref()
                .is_none_or(|ids| ids.contains(&header.workspace_id))
            {
                read_usage(conn, &header.composer_id)?
            } else {
                None
            };
            chats.push(ChatFacts::new(&header, usage));
        }
    }
    chats.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(InstallFacts {
        name: rt.layout.name.clone(),
        root: rt.layout.cursor_root.clone(),
        global_db: file_len(&rt.layout.global_db()),
        profiles: user_profiles(rt)?,
        workspaces,
        chats,
    })
}

pub fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len()).or_else(|| {
        if path.is_dir() {
            dir_size(path).ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skimmed_usage_matches_the_full_parse() {
        let samples = [
            json!({
                "usageData": {"opus": {"costInCents": 5768, "amount": 202}, "tool": {"costInCents": 5, "amount": 7}},
                "contextTokensUsed": 63408,
                "conversation": [
                    {"tokenCount": {"inputTokens": 1200, "outputTokens": 300}, "text": "x"},
                    {"tokenCount": {"inputTokens": 0, "outputTokens": 0}},
                    {"text": "no counts", "richText": {"root": [1, 2, 3]}}
                ],
                "modelConfig": {"modelName": "claude-opus"}
            }),
            json!({"usageData": [], "contextTokensUsed": -4, "conversation": {"a": 1}, "modelConfig": "gpt"}),
            json!({"conversation": [null, 3, {"tokenCount": {"inputTokens": 9}}], "contextTokensUsed": 2.5}),
            json!({"usageData": null, "modelConfig": {"modelName": 7}}),
            json!(["not", "an", "object"]),
            json!("text"),
        ];
        for sample in samples {
            let raw = sample.to_string();
            assert_eq!(
                ChatUsage::parse(&raw),
                Some(ChatUsage::from_json(&sample)),
                "{raw}"
            );
        }
        assert_eq!(ChatUsage::parse("not json"), None);
        let usage = ChatUsage::parse(
            r#"{"modelConfig":{"modelName":"opus"},"usageData":{"m":{"costInCents":10,"amount":2}}}"#,
        )
        .unwrap();
        assert_eq!(usage.model.as_deref(), Some("opus"));
        assert_eq!((usage.cost_cents, usage.requests), (10, 2));
    }
}
