use anyhow::Context;
use wm_common::try_warn;

use crate::{
  commands::monitor::{
    add_monitor, move_bounded_workspaces_to_new_monitor,
    move_workspace_to_monitor, remove_monitor, sort_monitors,
    update_monitor,
  },
  models::{Monitor, NativeMonitorProperties, Workspace},
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::{PendingWorkspaceRestore, WmState},
};

pub fn handle_display_settings_changed(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  tracing::info!("Display settings changed.");

  // Ignore the event if retrieval of the displays or their properties
  // fails (can happen transiently during sleep/wake).
  let displays = try_warn!(state
    .dispatcher
    .sorted_displays()
    .map_err(anyhow::Error::from)
    .and_then(|displays| {
      displays
        .into_iter()
        .map(|display| {
          let properties = NativeMonitorProperties::try_from(&display)?;
          Ok((display, properties))
        })
        .try_collect::<Vec<_>>()
    }));

  let mut pending_monitors = state.monitors();
  let remembered_properties = pending_monitors
    .iter()
    .map(|monitor| (monitor.id(), monitor.native_properties()))
    .collect::<Vec<_>>();
  let mut unmatched_displays = Vec::new();

  // Match each display to an existing monitor and update it.
  for (display, properties) in displays {
    match find_matching_monitor(&pending_monitors, &properties) {
      Some((monitor, index)) => {
        update_monitor(monitor, &display, properties, state)?;
        pending_monitors.remove(index);
      }
      None => unmatched_displays.push((display, properties)),
    }
  }

  let mut new_monitors: Vec<Monitor> = Vec::new();

  // Pair unmatched displays with unmatched monitors, or add new ones.
  for (display, properties) in unmatched_displays {
    if pending_monitors.is_empty() {
      let monitor = add_monitor(display, properties, state)?;
      new_monitors.push(monitor);
    } else {
      let monitor = pending_monitors.remove(0);
      update_monitor(&monitor, &display, properties, state)?;
    }
  }

  // Remove monitors that no longer have a corresponding display and move
  // their workspaces to other monitors.
  //
  // Prevent removal of the last monitor (i.e. for when all monitors are
  // disconnected). This will cause the WM's monitors to temporarily
  // mismatch the OS monitor state, however, it'll be updated correctly
  // when a new monitor is connected again.
  for monitor in pending_monitors {
    if state.monitors().len() > 1 {
      remove_monitor(monitor, state, config)?;
    }
  }

  // Sort monitors by position.
  sort_monitors(&state.root_container)?;

  for new_monitor in new_monitors {
    move_bounded_workspaces_to_new_monitor(&new_monitor, state, config)?;
  }

  // Restore only workspaces still on their temporary monitor. Match physical
  // identity rather than monitor index or a potentially recycled handle.
  let present = state.monitors();
  let pending = std::mem::take(&mut state.pending_workspace_restores);
  for record in &pending {
    let Some(workspace) = state
      .workspaces()
      .into_iter()
      .find(|workspace| workspace.id() == record.workspace_id)
    else {
      continue;
    };
    let Some(current_monitor) = workspace.monitor() else {
      continue;
    };
    if workspace.config().bind_to_monitor.is_some() {
      continue;
    }
    if current_monitor.id() != record.target_monitor_id {
      continue;
    }

    let matching = present
      .iter()
      .filter(|monitor| {
        let properties = monitor.native_properties();
        matches_origin(
          &properties,
          record,
          &present,
          &remembered_properties,
          &pending,
        )
      })
      .collect::<Vec<_>>();

    if matching.len() == 1 {
      let origin = matching[0];
      if origin.id() != current_monitor.id() {
        move_workspace_to_monitor(&workspace, origin, state, config)?;
        restore_floating_placements(&workspace, record);
      }
      continue;
    }
    state.pending_workspace_restores.push(record.clone());
  }

  for window in state.windows() {
    // Display setting changes can spread windows out sporadically, so mark
    // all windows as needing a DPI adjustment (just in case).
    window.set_has_pending_dpi_adjustment(true);

    // Need to update floating position of moved windows when a monitor is
    // disconnected or if the primary display is changed. The primary
    // display dictates the position of 0,0.
    let workspace = window.workspace().context("No workspace.")?;

    let should_recenter = if window.has_custom_floating_placement() {
      let workspace_rect = workspace.to_rect()?;

      // Keep the placement if it still intersects the workspace, since
      // `PlatformEvent::DisplaySettingsChanged` can be triggered by
      // non-monitor changes (e.g. unplugging a USB device).
      window
        .floating_placement()
        .intersection_area(&workspace_rect)
        == 0
    } else {
      true
    };

    if should_recenter {
      window.set_floating_placement(
        window
          .floating_placement()
          .translate_to_center(&workspace.to_rect()?),
      );
    }
  }

  // Redraw full container tree.
  state
    .pending_sync
    .queue_container_to_redraw(state.root_container.clone());

  Ok(())
}

/// Reapplies the floating placements saved before the workspace was moved
/// off its origin monitor.
///
/// Placements that no longer intersect the workspace are skipped, so they
/// stay centered.
fn restore_floating_placements(
  workspace: &Workspace,
  record: &PendingWorkspaceRestore,
) {
  let Ok(workspace_rect) = workspace.to_rect() else {
    return;
  };

  let windows = workspace
    .descendants()
    .filter_map(|descendant| descendant.as_window_container().ok());

  for window in windows {
    let saved = record
      .floating_placements
      .iter()
      .find(|(id, _)| *id == window.id());

    if let Some((_, placement)) = saved {
      if placement.intersection_area(&workspace_rect) > 0 {
        window.set_floating_placement(placement.clone());
      }
    }
  }
}

/// Returns whether the monitor with the given properties is the physical
/// origin of the pending restore.
///
/// A reassigned monitor's old identity is valid only when its new display
/// is also the physical origin. Never infers identity from a recycled
/// monitor ID.
fn matches_origin(
  properties: &NativeMonitorProperties,
  record: &PendingWorkspaceRestore,
  present: &[Monitor],
  remembered: &[(uuid::Uuid, NativeMonitorProperties)],
  pending: &[PendingWorkspaceRestore],
) -> bool {
  let origin = &record.origin_properties;
  #[cfg(target_os = "macos")]
  {
    let _ = (present, remembered, pending);
    !origin.device_uuid.is_empty()
      && properties.device_uuid == origin.device_uuid
  }
  #[cfg(target_os = "windows")]
  {
    let path = origin
      .device_path
      .as_deref()
      .filter(|path| !path.is_empty());
    if let Some(path) = path {
      let path_matches = present
        .iter()
        .filter(|other| {
          other.native_properties().device_path.as_deref() == Some(path)
        })
        .count();
      if path_matches != 0 {
        return path_matches == 1
          && properties.device_path.as_deref() == Some(path)
          && remembered
            .iter()
            .filter(|(_, other)| {
              other.device_path.as_deref() == Some(path)
            })
            .count()
            <= 1
          && !pending.iter().any(|other| {
            other.origin_monitor_id != record.origin_monitor_id
              && other.origin_properties.device_path.as_deref()
                == Some(path)
          });
      }
    }
    let Some(hardware_id) =
      origin.hardware_id.as_deref().filter(|id| !id.is_empty())
    else {
      return false;
    };
    properties.hardware_id.as_deref() == Some(hardware_id)
      && present
        .iter()
        .filter(|other| {
          other.native_properties().hardware_id.as_deref()
            == Some(hardware_id)
        })
        .count()
        == 1
      && remembered
        .iter()
        .filter(|(_, other)| {
          other.hardware_id.as_deref() == Some(hardware_id)
        })
        .count()
        <= 1
      && !remembered.iter().any(|(id, other)| {
        *id != record.origin_monitor_id
          && other.hardware_id.as_deref() == Some(hardware_id)
          && other.device_path != origin.device_path
      })
      && pending
        .iter()
        .filter(|other| {
          other.origin_properties.hardware_id.as_deref()
            == Some(hardware_id)
            && other.origin_monitor_id != record.origin_monitor_id
        })
        .count()
        == 0
  }
}

/// Finds the monitor matching the given display properties.
///
/// Returns the monitor and its index within the list of monitors.
fn find_matching_monitor<'a>(
  monitors: &'a [Monitor],
  properties: &NativeMonitorProperties,
) -> Option<(&'a Monitor, usize)> {
  monitors.iter().enumerate().find_map(|(index, monitor)| {
    let existing = monitor.native_properties();

    let is_match = {
      #[cfg(target_os = "macos")]
      {
        existing.device_uuid == properties.device_uuid
      }

      // On Windows, match the monitor by:
      // 1. Its handle
      // 2. Its device path
      // 3. Its hardware ID (if unique)
      //
      // Monitor handles and device paths are unique, but can change over
      // time. The hardware ID is not guaranteed to be unique, so we
      // match against that last.
      #[cfg(target_os = "windows")]
      {
        existing.handle == properties.handle
          || existing.device_path.as_deref().is_some_and(|device_path| {
            properties.device_path.as_deref() == Some(device_path)
          })
          || existing.hardware_id.as_deref().is_some_and(|hardware_id| {
            let is_unique = monitors
              .iter()
              .filter(|other_monitor| {
                other_monitor.native_properties().hardware_id.as_deref()
                  == Some(hardware_id)
              })
              .count()
              == 1;

            is_unique
              && properties.hardware_id.as_deref() == Some(hardware_id)
          })
      }
    };

    is_match.then_some((monitor, index))
  })
}
