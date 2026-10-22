//! Port of `compaction/utils.ts` file tracking: `createFileOps`,
//! `extractFileOpsFromMessage`, `computeFileLists`, `formatFileOperations`.
//!
//! Sets keep insertion order (`Vec` + membership check) because the upstream
//! `Set` order is observable in the emitted fixture; only `computeFileLists`
//! sorts, and it sorts with JS's UTF-16 code-unit ordering.

use serde_json::Value;

use super::model::{message_role, str_field};

/// Files touched by a branch or compaction range. Mirrors TS `FileOperations`.
/// Insertion-ordered, like the upstream `Set`s.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct FileOperations {
    pub read: Vec<String>,
    pub written: Vec<String>,
    pub edited: Vec<String>,
}

impl FileOperations {
    fn insert(target: &mut Vec<String>, path: &str) {
        if !target.iter().any(|existing| existing == path) {
            target.push(path.to_string());
        }
    }

    pub fn add_read(&mut self, path: &str) {
        Self::insert(&mut self.read, path);
    }

    pub fn add_written(&mut self, path: &str) {
        Self::insert(&mut self.written, path);
    }

    pub fn add_edited(&mut self, path: &str) {
        Self::insert(&mut self.edited, path);
    }
}

/// `addFileOp(toolName, args, fileOps)`.
fn add_file_op(tool_name: &str, args: Option<&Value>, ops: &mut FileOperations) {
    let path = match args.and_then(|args| str_field(args, "path")) {
        Some(path) if !path.is_empty() => path,
        _ => return,
    };
    match tool_name {
        "read" => ops.add_read(path),
        "write" => ops.add_written(path),
        "edit" => ops.add_edited(path),
        _ => {}
    }
}

/// `extractFileOpsFromMessage(message, fileOps)`: assistant tool calls plus the
/// nested calls recorded on codemode tool results.
pub fn extract_file_ops_from_message(message: &Value, ops: &mut FileOperations) {
    if message_role(message) == "toolResult" {
        if let Some(calls) = message
            .get("nestedCalls")
            .and_then(|nested| nested.get("calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                let name = str_field(call, "name").unwrap_or("");
                add_file_op(name, call.get("arguments"), ops);
            }
        }
        return;
    }
    if message_role(message) != "assistant" {
        return;
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if !block.is_object() {
            continue;
        }
        if str_field(block, "type") != Some("toolCall") {
            continue;
        }
        let Some(name) = str_field(block, "name") else {
            continue;
        };
        add_file_op(name, block.get("arguments"), ops);
    }
}

/// JS `Array.prototype.sort()` default comparator: UTF-16 code-unit order.
fn sort_js(strings: &mut [String]) {
    strings.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// `computeFileLists(fileOps)` → `(readFiles, modifiedFiles)`.
pub fn compute_file_lists(ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let mut modified: Vec<String> = Vec::new();
    for path in ops.edited.iter().chain(ops.written.iter()) {
        FileOperations::insert(&mut modified, path);
    }
    let mut read_only: Vec<String> = ops
        .read
        .iter()
        .filter(|path| !modified.iter().any(|m| &m == path))
        .cloned()
        .collect();
    sort_js(&mut read_only);
    let mut modified_files = modified;
    sort_js(&mut modified_files);
    (read_only, modified_files)
}

/// `formatFileOperations(readFiles, modifiedFiles)`.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}
