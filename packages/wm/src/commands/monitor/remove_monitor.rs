use anyhow::Context;
use tracing::info;
use wm_common::WmEvent;

use crate::{
  commands::{
    container::{detach_container, move_container_within_tree},
    workspace::sort_workspaces,
  },
  models::Monitor,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::{PendingWorkspaceRestore, WmState},
};

#[allow(clippy::needless_pass_by_value)]
pub fn remove_monitor(
  monitor: Monitor,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  info!("Removing monitor: {monitor}");

  let target_monitor = state
    .monitors()
    .into_iter()
    .find(|m| m.id() != monitor.id())
    .context("No target monitor to move workspaces.")?;

  // Avoid moving empty workspaces.
  let workspaces_to_move =
    monitor.workspaces().into_iter().filter(|workspace| {
      workspace.has_children() || workspace.config().keep_alive
    });

  for workspace in workspaces_to_move {
    let floating_placements = workspace
      .descendants()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .map(|window| (window.id(), window.floating_placement()))
      .collect::<Vec<_>>();

    // Move workspace to target monitor.
    move_container_within_tree(
      &workspace.clone().into(),
      &target_monitor.clone().into(),
      target_monitor.child_count(),
      state,
    )?;

    // Preserve the physical origin across multiple disconnects. Only a
    // successful move creates (or updates) a pending restore.
    if let Some(pending) = state
      .pending_workspace_restores
      .iter_mut()
      .find(|pending| pending.workspace_id == workspace.id())
    {
      pending.target_monitor_id = target_monitor.id();
    } else {
      state
        .pending_workspace_restores
        .push(PendingWorkspaceRestore {
          workspace_id: workspace.id(),
          origin_monitor_id: monitor.id(),
          target_monitor_id: target_monitor.id(),
          origin_properties: monitor.native_properties(),
          floating_placements,
        });
    }

    sort_workspaces(&target_monitor, config)?;

    state.emit_event(WmEvent::WorkspaceUpdated {
      updated_workspace: workspace.to_dto()?,
    });
  }

  detach_container(monitor.clone().into())?;

  state.emit_event(WmEvent::MonitorRemoved {
    removed_id: monitor.id(),
    removed_device_name: monitor.native_properties().device_name,
  });

  Ok(())
}
