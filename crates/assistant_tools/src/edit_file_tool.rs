use crate::{replace::replace_with_flexible_indent, schema::json_schema_for};
use anyhow::{Context as _, Result, anyhow};
use assistant_tool::{ActionLog, Tool, ToolCard, ToolResult, ToolUseStatus};
use buffer_diff::{BufferDiff, BufferDiffSnapshot};
use editor::{MultiBuffer, PathKey};
use gpui::{App, AppContext, AsyncApp, Context, Entity, IntoElement, Task, Window, prelude::*};
use language::{Anchor, Buffer, Capability, LanguageRegistry, LineEnding, OffsetRangeExt};
use language_model::{LanguageModelRequestMessage, LanguageModelToolSchemaFormat};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use ui::{Color, IconName, IconSize, prelude::*};

use crate::replace::replace_exact;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditFileToolInput {
    /// The full path of the file to modify in the project.
    ///
    /// WARNING: When specifying which file path need changing, you MUST
    /// start each path with one of the project's root directories.
    ///
    /// The following examples assume we have two root directories in the project:
    /// - backend
    /// - frontend
    ///
    /// <example>
    /// `backend/src/main.rs`
    ///
    /// Notice how the file path starts with root-1. Without that, the path
    /// would be ambiguous and the call would fail!
    /// </example>
    ///
    /// <example>
    /// `frontend/db.js`
    /// </example>
    pub path: PathBuf,

    /// A user-friendly markdown description of what's being replaced. This will be shown in the UI.
    ///
    /// <example>Fix API endpoint URLs</example>
    /// <example>Update copyright year in `page_footer`</example>
    pub display_description: String,

    /// The text to replace.
    pub old_string: String,

    /// The text to replace it with.
    pub new_string: String,
}

pub struct EditFileTool;

impl Tool for EditFileTool {
    fn name(&self) -> String {
        "edit_file".into()
    }

    fn needs_confirmation(&self, _: &serde_json::Value, _: &App) -> bool {
        false
    }

    fn description(&self) -> String {
        include_str!("edit_file_tool/description.md").to_string()
    }

    fn icon(&self) -> IconName {
        IconName::Pencil
    }

    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value> {
        json_schema_for::<EditFileToolInput>(format)
    }

    fn ui_text(&self, input: &serde_json::Value) -> String {
        match serde_json::from_value::<EditFileToolInput>(input.clone()) {
            Ok(input) => input.display_description,
            Err(_) => "Edit file".to_string(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: serde_json::Value,
        _messages: &[LanguageModelRequestMessage],
        project: Entity<Project>,
        action_log: Entity<ActionLog>,
        cx: &mut App,
    ) -> ToolResult {
        let input = match serde_json::from_value::<EditFileToolInput>(input) {
            Ok(input) => input,
            Err(err) => return Task::ready(Err(anyhow!(err))).into(),
        };

        let output = cx.spawn(async move |cx: &mut AsyncApp| {
            let project_path = project.read_with(cx, |project, cx| {
                project
                    .find_project_path(&input.path, cx)
                    .context("Path not found in project")
            })??;

            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))?
                .await?;

            let snapshot = buffer.read_with(cx, |buffer, _cx| buffer.snapshot())?;

            if input.old_string.is_empty() {
                return Err(anyhow!("`old_string` cannot be empty. Use a different tool if you want to create a file."));
            }

            if input.old_string == input.new_string {
                return Err(anyhow!("The `old_string` and `new_string` are identical, so no changes would be made."));
            }

            let result = cx
                .background_spawn(async move {
                    // Try to match exactly
                    let diff = replace_exact(&input.old_string, &input.new_string, &snapshot)
                    .await
                    // If that fails, try being flexible about indentation
                    .or_else(|| replace_with_flexible_indent(&input.old_string, &input.new_string, &snapshot))?;

                    if diff.edits.is_empty() {
                        return None;
                    }

                    let old_text = snapshot.text();

                    Some((old_text, diff))
                })
                .await;

            let Some((old_text, diff)) = result else {
                let err = buffer.read_with(cx, |buffer, _cx| {
                    let file_exists = buffer
                        .file()
                        .map_or(false, |file| file.disk_state().exists());

                    if !file_exists {
                        anyhow!("{} does not exist", input.path.display())
                    } else if buffer.is_empty() {
                        anyhow!(
                            "{} is empty, so the provided `old_string` wasn't found.",
                            input.path.display()
                        )
                    } else {
                        anyhow!("Failed to match the provided `old_string`")
                    }
                })?;

                return Err(err)
            };

            let snapshot = cx.update(|cx| {
                action_log.update(cx, |log, cx| {
                    log.buffer_read(buffer.clone(), cx)
                });
                let snapshot = buffer.update(cx, |buffer, cx| {
                    buffer.finalize_last_transaction();
                    buffer.apply_diff(diff, cx);
                    buffer.finalize_last_transaction();
                    buffer.snapshot()
                });
                action_log.update(cx, |log, cx| {
                    log.buffer_edited(buffer.clone(), cx)
                });
                snapshot
            })?;

            project.update(cx, |project, cx| {
                project.save_buffer(buffer.clone(), cx)
            })?.await?;

            let buffer_diff =
                build_buffer_diff(Some(old_text.clone()), &buffer, cx).await?;

            let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadOnly)).unwrap();

            multibuffer.update(cx, |multibuffer, cx| {
                let snapshot = buffer.read(cx).snapshot();
                let diff = buffer_diff.read(cx);
                let diff_hunk_ranges = diff
                    .hunks_intersecting_range(Anchor::MIN..Anchor::MAX, &snapshot, cx)
                    .map(|diff_hunk| diff_hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>();
                let path = snapshot.file().unwrap().path().clone();
                const FILE_NAMESPACE: u32 = 1;
                let _is_newly_added = multibuffer.set_excerpts_for_path(
                    PathKey::namespaced(FILE_NAMESPACE, path),
                    buffer.clone(),
                    diff_hunk_ranges,
                    0, // context
                    cx,
                );
                multibuffer.add_diff(buffer_diff, cx);
            });

            let diff_str = cx.background_spawn(async move {
                let new_text = snapshot.text();
                language::unified_diff(&old_text, &new_text)
            }).await;

            Ok((format!("Edited {}:\n\n```diff\n{}\n```", input.path.display(), diff_str)))
        });

        let card = cx
            .new(|cx| {
                let (_, diff_buffer) = output.read(cx)?;
                EditFileToolCard::new(diff_buffer, cx)
            })
            .into();

        ToolResult {
            output,
            card: Some(card),
        }
    }
}

struct EditFileToolCard {
    diff: Option<Result<Entity<MultiBuffer>>>,
    _task: Task<()>,
}

impl EditFileToolCard {
    fn new(diff: Entity<BufferDiff>, cx: &mut Context<Self>) -> Self {
        let _task = cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.diff = Some(Ok(diff));
                cx.notify();
            })
            .ok();
        });

        Self { diff: None, _task }
    }
}

impl ToolCard for EditFileToolCard {
    fn render(
        &mut self,
        _status: &ToolUseStatus,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let header = h_flex()
            .id("tool-label-container")
            .gap_1p5()
            .max_w_full()
            .overflow_x_scroll()
            .child(
                Icon::new(IconName::Pencil)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child("File Edit")
            .into_any();

        let content = self.diff.as_ref().and_then(|diff| match diff {
            Ok(diff) => {
                let snapshot = diff.read(cx).snapshot();
                let hunks = snapshot
                    .hunks_intersecting_range(Anchor::MIN..Anchor::MAX, &snapshot, cx)
                    .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>();

                Some(
                    v_flex()
                        .ml_1p5()
                        .pl_1p5()
                        .border_l_1()
                        .border_color(cx.theme().colors().border_variant)
                        .gap_1()
                        .child(diff.clone())
                        .into_any(),
                )
            }
            Err(_) => None,
        });

        v_flex().my_2().gap_1().child(header).children(children)
    }
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let buffer = cx.update(|cx| buffer.read(cx).snapshot())?;

    let base_buffer = cx
        .update(|cx| {
            Buffer::build_snapshot(
                old_text.as_deref().unwrap_or("").into(),
                buffer.language().cloned(),
                // TODO: provide LanguageRegistry to have syntax highlighting
                None,
                cx,
            )
        })?
        .await;

    let diff_snapshot = cx
        .update(|cx| {
            BufferDiffSnapshot::new_with_base_buffer(
                buffer.text.clone(),
                old_text.map(Arc::new),
                base_buffer,
                cx,
            )
        })?
        .await;

    cx.new(|cx| {
        let mut diff = BufferDiff::new(&buffer.text, cx);
        diff.set_snapshot(diff_snapshot, &buffer.text, cx);
        diff
    })
}
