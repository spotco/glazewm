use anyhow::Context;
use uuid::Uuid;

use super::set_focused_descendant;
use crate::{traits::CommonGetters, wm_state::WmState};

pub fn focus_container_by_id(
  container_id: &Uuid,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let focus_target = state
    .container_by_id(*container_id)
    .context("No container with given id")?;

  // Set focus to the target container.
  set_focused_descendant(&focus_target, None);
  state.pending_sync.queue_focus_change().queue_cursor_jump();
  if let Ok(window) = focus_target.as_window_container() {
    if let Some(workspace) = window.workspace() {
      state.pending_sync.queue_stack_activation(
        &workspace,
        window.id(),
        crate::pending_sync::StackActivationReason::WmCommand,
      );
    }
  }

  Ok(())
}
