use std::{
  collections::{HashMap, HashSet},
  time::Instant,
};

use anyhow::Context;
use tokio::sync::mpsc::{self};
use tracing::warn;
use uuid::Uuid;
use wm_common::{
  BindingModeConfig, HideCorner, LayoutSnapshot, TilingDirection,
  WindowState, WmEvent,
};
use wm_platform::{
  Direction, Dispatcher, Display, NativeWindow, Point, Rect, WindowId,
};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, OpacityValue};

use crate::{
  commands::{
    container::set_focused_descendant,
    general::{platform_sync, LayoutHistory},
    monitor::{add_monitor, move_bounded_workspaces_to_new_monitor},
    window::{manage_window, unmanage_window},
  },
  models::{
    Container, Monitor, NativeMonitorProperties, RootContainer,
    WindowContainer, Workspace, WorkspaceTarget,
  },
  pending_sync::PendingSync,
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
};

pub(crate) const DEFAULT_GLOBAL_TILING_DIRECTION: TilingDirection =
  TilingDirection::Horizontal;

pub struct WmState {
  /// Root node of the container tree. Monitors are the children of the
  /// root node, followed by workspaces, then split containers/windows.
  pub root_container: RootContainer,

  pub dispatcher: Dispatcher,

  pub pending_sync: PendingSync,

  /// Name of the most recently focused workspace.
  ///
  /// Used for the `general.toggle_workspace_on_refocus` option on
  /// workspace focus.
  pub recent_workspace_name: Option<String>,

  /// The previously focused window that had focus effects applied.
  ///
  /// Used to efficiently update window effects by only removing focus
  /// effects from the previous window rather than all windows when focus
  /// changes.
  pub prev_effects_window: Option<WindowContainer>,

  /// Time since a previously focused window was unmanaged or minimized.
  ///
  /// Used to decide whether to override incoming focus events.
  pub unmanaged_or_minimized_timestamp: Option<Instant>,

  /// Windows natively minimized by the active Show Desktop session. Their
  /// logical minimized transitions may temporarily simplify the live
  /// tree; the session snapshot restores the original topology
  /// afterward.
  show_desktop_minimized: HashSet<Uuid>,

  /// Last intended normal-window z-order for each workspace, top to
  /// bottom. This is transient native state, not layout state. It lets
  /// focus changes promote only the selected detached window while
  /// preserving the rest of the existing stack.
  normal_z_order_by_workspace: HashMap<Uuid, Vec<WindowId>>,

  /// Layout captured before Show Desktop temporarily minimizes managed
  /// windows. The snapshot is used to rebuild the exact tree after the
  /// last Show Desktop window is restored.
  show_desktop_snapshot: Option<LayoutSnapshot>,

  /// Configs of currently enabled binding modes.
  pub binding_modes: Vec<BindingModeConfig>,

  /// Windows that the WM should ignore. Windows can be added via the
  /// `ignore` command.
  pub ignored_windows: Vec<NativeWindow>,

  /// Ignored window that currently owns native foreground.
  ///
  /// Logical focus stays on the last managed container. While this is
  /// set, focus enforcement and normal z-order reconciliation are
  /// suspended so already-queued work cannot cover the ignored window.
  /// Cleared by the next non-ignored focus event, or when the OS
  /// foreground is a managed window or the desktop.
  pub ignored_native_foreground: Option<WindowId>,

  /// WM-wide insertion / stack axis for directional moves (spotcobuild).
  pub global_tiling_direction: TilingDirection,

  /// Exact, bounded undo/redo history for structural tiling moves.
  pub layout_history: LayoutHistory,

  /// Whether the WM is paused.
  pub is_paused: bool,

  /// Whether the OS focused window is the same as the WM focused window.
  pub is_focus_synced: bool,

  /// Whether the initial state has been populated.
  has_initialized: bool,

  /// Sender for emitting WM-related events.
  event_tx: mpsc::UnboundedSender<WmEvent>,

  /// Sender for gracefully shutting down the WM.
  exit_tx: mpsc::UnboundedSender<()>,
}

impl WmState {
  pub fn new(
    dispatcher: Dispatcher,
    event_tx: mpsc::UnboundedSender<WmEvent>,
    exit_tx: mpsc::UnboundedSender<()>,
  ) -> Self {
    Self {
      root_container: RootContainer::new(),
      dispatcher,
      pending_sync: PendingSync::default(),
      prev_effects_window: None,
      recent_workspace_name: None,
      unmanaged_or_minimized_timestamp: None,
      show_desktop_minimized: HashSet::new(),
      normal_z_order_by_workspace: HashMap::new(),
      show_desktop_snapshot: None,
      binding_modes: Vec::new(),
      ignored_windows: Vec::new(),
      ignored_native_foreground: None,
      global_tiling_direction: DEFAULT_GLOBAL_TILING_DIRECTION,
      layout_history: LayoutHistory::default(),
      is_paused: false,
      is_focus_synced: false,
      has_initialized: false,
      event_tx,
      exit_tx,
    }
  }

  /// Populates the initial WM state by creating containers for all
  /// existing windows and monitors.
  pub fn populate(
    &mut self,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    // Get the originally focused window when the WM was started.
    let focused_window = self.dispatcher.focused_window().ok();

    // Create a monitor, and consequently a workspace, for each detected
    // native monitor.
    for native_display in self.dispatcher.sorted_displays()? {
      if let Ok(native_properties) =
        NativeMonitorProperties::try_from(&native_display)
      {
        let monitor =
          add_monitor(native_display, native_properties, self)?;
        move_bounded_workspaces_to_new_monitor(&monitor, self, config)?;
      }
    }

    // Manage windows in reverse z-order (bottom to top). This helps to
    // preserve the original stacking order.
    for native_window in
      self.dispatcher.visible_windows()?.into_iter().rev()
    {
      let nearest_workspace = self
        .nearest_monitor(&native_window)
        .and_then(|m| m.displayed_workspace());

      if let Some(workspace) = nearest_workspace {
        manage_window(
          native_window,
          Some(workspace.into()),
          self,
          config,
        )?;
      }
    }

    let container_to_focus = focused_window
      .and_then(|focused_window| {
        self.window_from_native(&focused_window).map(Into::into)
      })
      .or_else(|| self.windows().pop().map(Into::into))
      .or_else(|| self.workspaces().pop().map(Into::into))
      .context("Failed to get container to focus.")?;

    set_focused_descendant(&container_to_focus, None);
    self.is_focus_synced = true;

    self
      .pending_sync
      .queue_focus_change()
      .queue_all_effects_update();

    for workspace in self.workspaces() {
      self.pending_sync.queue_workspace_to_reorder(workspace);
    }

    platform_sync(self, config)?;
    self.has_initialized = true;

    Ok(())
  }

  pub fn monitors(&self) -> Vec<Monitor> {
    self.root_container.monitors()
  }

  pub fn workspaces(&self) -> Vec<Workspace> {
    self
      .monitors()
      .iter()
      .flat_map(Monitor::workspaces)
      .collect()
  }

  /// Gets workspaces sorted by their position in the user config.
  pub fn sorted_workspaces(&self, config: &UserConfig) -> Vec<Workspace> {
    let mut workspaces = self.workspaces();
    config.sort_workspaces(&mut workspaces);
    workspaces
  }

  pub fn windows(&self) -> Vec<WindowContainer> {
    self
      .root_container
      .descendants()
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the monitor that encompasses the largest portion of a given
  /// window.
  ///
  /// Defaults to the first monitor if the nearest monitor is invalid.
  pub fn nearest_monitor(
    &self,
    native_window: &NativeWindow,
  ) -> Option<Monitor> {
    self
      .monitor_from_native(
        &self.dispatcher.nearest_display(native_window).ok()?,
      )
      .or(self.monitors().first().cloned())
  }

  /// Gets monitor that corresponds to the given `Display`.
  pub fn monitor_from_native(
    &self,
    native_display: &Display,
  ) -> Option<Monitor> {
    self
      .monitors()
      .into_iter()
      .find(|monitor| monitor.native() == *native_display)
  }

  /// Gets the closest monitor in a given direction.
  ///
  /// Uses i3wm's algorithm for finding best guess.
  pub fn monitor_in_direction(
    &self,
    origin_monitor: &Monitor,
    direction: &Direction,
  ) -> anyhow::Result<Option<Monitor>> {
    let origin_rect = origin_monitor.native_properties().bounds;

    // Create a tuple of monitors and their rect.
    let monitors_with_rect = self
      .monitors()
      .into_iter()
      .map(|monitor| {
        let rect = monitor.native_properties().bounds;
        anyhow::Ok((monitor, rect))
      })
      .try_collect::<Vec<_>>()?;

    let closest_monitor = monitors_with_rect
      .into_iter()
      .filter(|(_, rect)| match direction {
        Direction::Right => {
          rect.x() > origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Left => {
          rect.x() < origin_rect.x() && rect.y_overlap(&origin_rect) > 0
        }
        Direction::Down => {
          rect.y() > origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
        Direction::Up => {
          rect.y() < origin_rect.y() && rect.x_overlap(&origin_rect) > 0
        }
      })
      .min_by(|(_, rect_a), (_, rect_b)| match direction {
        Direction::Right => rect_a.x().cmp(&rect_b.x()),
        Direction::Left => rect_b.x().cmp(&rect_a.x()),
        Direction::Down => rect_a.y().cmp(&rect_b.y()),
        Direction::Up => rect_b.y().cmp(&rect_a.y()),
      })
      .map(|(monitor, _)| monitor);

    Ok(closest_monitor)
  }

  /// Determines the preferred hide corner for each monitor. Used for
  /// [`HideMethod::PlaceInCorner`].
  ///
  /// The corner is chosen by simulating a 400x400 window frame in the
  /// bottom-left and bottom-right of the monitor's working area, then
  /// picking the side that overlaps the least with other monitors'
  /// working areas (ties favor bottom-right).
  pub fn monitors_by_hide_corner(&self) -> Vec<(Monitor, HideCorner)> {
    const TEST_FRAME_SIZE: i32 = 400;
    const VISIBLE_SLIVER: i32 = 1;

    let monitors = self.monitors();
    let working_areas = monitors
      .iter()
      .map(|monitor| monitor.native_properties().working_area)
      .collect::<Vec<_>>();

    monitors
      .into_iter()
      .enumerate()
      .map(|(idx, monitor)| {
        let monitor_rect = &working_areas[idx];
        let test_frame_y = monitor_rect.bottom - TEST_FRAME_SIZE;

        let left_test_frame = Rect::from_xy(
          monitor_rect.left - TEST_FRAME_SIZE + VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let right_test_frame = Rect::from_xy(
          monitor_rect.right - VISIBLE_SLIVER,
          test_frame_y,
          TEST_FRAME_SIZE,
          TEST_FRAME_SIZE,
        );

        let overlap_area = |test_frame: &Rect| -> i32 {
          working_areas
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, rect)| test_frame.intersection_area(rect))
            .sum()
        };

        let left_overlap = overlap_area(&left_test_frame);
        let right_overlap = overlap_area(&right_test_frame);

        let corner = if left_overlap < right_overlap {
          HideCorner::BottomLeft
        } else {
          HideCorner::BottomRight
        };

        (monitor, corner)
      })
      .collect()
  }

  /// Gets window that corresponds to the given `NativeWindow`.
  pub fn window_from_native(
    &self,
    native_window: &NativeWindow,
  ) -> Option<WindowContainer> {
    self
      .windows()
      .into_iter()
      .find(|window| &*window.native() == native_window)
  }

  pub fn workspace_by_name(
    &self,
    workspace_name: &str,
  ) -> Option<Workspace> {
    self
      .workspaces()
      .into_iter()
      .find(|workspace| workspace.config().name == workspace_name)
  }

  /// Gets a workspace and its name by the given target.
  ///
  /// Returns a tuple of the workspace name and the `Workspace` instance
  /// if active.
  #[allow(clippy::too_many_lines)]
  pub fn workspace_by_target(
    &self,
    origin_workspace: &Workspace,
    target: WorkspaceTarget,
    config: &UserConfig,
  ) -> anyhow::Result<(Option<String>, Option<Workspace>)> {
    let (name, workspace) = match target {
      WorkspaceTarget::Name(name) => {
        #[allow(clippy::match_bool)]
        match origin_workspace.config().name == name {
          false => (Some(name.clone()), self.workspace_by_name(&name)),
          // Toggle the workspace if it's already focused.
          true if config.value.general.toggle_workspace_on_refocus => (
            self.recent_workspace_name.clone(),
            self
              .recent_workspace_name
              .as_ref()
              .and_then(|name| self.workspace_by_name(name)),
          ),
          true => (None, None),
        }
      }
      WorkspaceTarget::Recent => (
        self.recent_workspace_name.clone(),
        self
          .recent_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name)),
      ),
      WorkspaceTarget::NextActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let next_active_workspace = active_workspaces
          .get(origin_index + 1)
          .or_else(|| active_workspaces.first());

        (
          next_active_workspace.map(|workspace| workspace.config().name),
          next_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::PreviousActive => {
        let active_workspaces = self.sorted_workspaces(config);
        let origin_index = active_workspaces
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of given workspace.")?;

        let prev_active_workspace = active_workspaces.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(active_workspaces.len() - 1),
        );

        (
          prev_active_workspace.map(|workspace| workspace.config().name),
          prev_active_workspace.cloned(),
        )
      }
      WorkspaceTarget::NextActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let next_active_workspace_in_monitor = workspace_in_monitor
          .get(origin_index + 1)
          .or_else(|| workspace_in_monitor.first());

        (
          next_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          next_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::PreviousActiveInMonitor => {
        let monitor = origin_workspace
          .monitor()
          .context("No monitor in workspace")?;

        let mut workspace_in_monitor = monitor.workspaces();
        config.sort_workspaces(&mut workspace_in_monitor);

        let origin_index = workspace_in_monitor
          .iter()
          .position(|workspace| workspace.id() == origin_workspace.id())
          .context("Failed to get index of give workspace")?;

        let prev_active_workspace_in_monitor = workspace_in_monitor.get(
          origin_index
            .checked_sub(1)
            .unwrap_or(workspace_in_monitor.len() - 1),
        );

        (
          prev_active_workspace_in_monitor
            .map(|workspace| workspace.config().name),
          prev_active_workspace_in_monitor.cloned(),
        )
      }
      WorkspaceTarget::Next => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let next_workspace_config = workspaces
          .get(origin_index + 1)
          .or_else(|| workspaces.first());

        let next_workspace_name =
          next_workspace_config.map(|config| config.name.clone());

        let next_workspace = next_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (next_workspace_name, next_workspace)
      }
      WorkspaceTarget::Previous => {
        let workspaces = &config.value.workspaces;
        let origin_name = origin_workspace.config().name.clone();
        let origin_index = workspaces
          .iter()
          .position(|workspace| workspace.name == origin_name)
          .context("Failed to get index of given workspace.")?;

        let previous_workspace_config = workspaces.get(
          origin_index.checked_sub(1).unwrap_or(workspaces.len() - 1),
        );

        let previous_workspace_name =
          previous_workspace_config.map(|config| config.name.clone());

        let previous_workspace = previous_workspace_name
          .as_ref()
          .and_then(|name| self.workspace_by_name(name));

        (previous_workspace_name, previous_workspace)
      }

      WorkspaceTarget::Direction(direction) => {
        let origin_monitor =
          origin_workspace.monitor().context("No focused monitor.")?;

        let target_workspace = self
          .monitor_in_direction(&origin_monitor, &direction)?
          .and_then(|monitor| monitor.displayed_workspace());

        (
          target_workspace
            .as_ref()
            .map(|workspace| workspace.config().name),
          target_workspace,
        )
      }
    };

    Ok((name, workspace))
  }

  /// Gets windows that should be redrawn.
  ///
  /// When redrawing after a command that changes a window's type (e.g.
  /// tiling -> floating), the original detached window might still be
  /// queued for a redraw and should be filtered out.
  pub fn windows_to_redraw(&self) -> Vec<WindowContainer> {
    self
      .pending_sync
      .containers_to_redraw()
      .values()
      .flat_map(CommonGetters::self_and_descendants)
      .filter(|container| !container.is_detached())
      .filter_map(|container| container.try_into().ok())
      .collect()
  }

  /// Gets the logically focused container. This can either be a window or
  /// a workspace without any descendant windows.
  pub fn focused_container(&self) -> Option<Container> {
    self.root_container.descendant_focus_order().next()
  }

  /// Gets the container that is safe to focus natively.
  ///
  /// Logical focus remains on a minimized window so minimize/removal logic
  /// can identify the window that was focused. Native focus must instead
  /// go to its workspace, which resets focus to the desktop without
  /// restoring the minimized window.
  pub fn native_focus_target(&self) -> Option<Container> {
    let focused_container = self.focused_container()?;
    if let Ok(window) = focused_container.as_window_container() {
      if window.state() == WindowState::Minimized {
        return window.workspace().map(Into::into);
      }
    }
    Some(focused_container)
  }

  pub fn mark_show_desktop_minimized(&mut self, window_id: Uuid) {
    self.show_desktop_minimized.insert(window_id);
  }

  pub fn is_show_desktop_minimized(&self, window_id: Uuid) -> bool {
    self.show_desktop_minimized.contains(&window_id)
  }

  pub fn clear_show_desktop_minimized(&mut self, window_id: Uuid) {
    self.show_desktop_minimized.remove(&window_id);
  }

  pub fn show_desktop_minimized_count(&self) -> usize {
    self.show_desktop_minimized.len()
  }

  pub fn show_desktop_session_active(&self) -> bool {
    self.show_desktop_snapshot.is_some()
  }

  pub fn begin_show_desktop_session(&mut self, snapshot: LayoutSnapshot) {
    self.show_desktop_snapshot = Some(snapshot);
  }

  pub fn take_show_desktop_snapshot(&mut self) -> Option<LayoutSnapshot> {
    self.show_desktop_snapshot.take()
  }

  pub fn clear_show_desktop_session(&mut self) {
    self.show_desktop_snapshot = None;
  }

  pub fn normal_z_order_for_workspace(
    &self,
    workspace_id: Uuid,
  ) -> Option<&[WindowId]> {
    self
      .normal_z_order_by_workspace
      .get(&workspace_id)
      .map(Vec::as_slice)
  }

  pub fn set_normal_z_order_for_workspace(
    &mut self,
    workspace_id: Uuid,
    window_ids: Vec<WindowId>,
  ) {
    self
      .normal_z_order_by_workspace
      .insert(workspace_id, window_ids);
  }

  /// Emits a WM event through an MSPC channel.
  ///
  /// Does not emit events while the WM is paused or populating initial
  /// state. This is to prevent events (e.g. workspace activation events)
  /// from being emitted via IPC server before the initial state is
  /// prepared.
  pub fn emit_event(&self, event: WmEvent) {
    if self.has_initialized
      && (!self.is_paused || matches!(event, WmEvent::PauseChanged { .. }))
    {
      if let Err(err) = self.event_tx.send(event) {
        warn!("Failed to send event: {}", err);
      }
    }
  }

  /// Starts graceful shutdown via an MSPC channel.
  pub fn emit_exit(&self) -> anyhow::Result<()> {
    self.exit_tx.send(())?;
    Ok(())
  }

  pub fn container_by_id(&self, id: Uuid) -> Option<Container> {
    self
      .root_container
      .self_and_descendants()
      .find(|container| container.id() == id)
  }

  /// Gets container to focus after the given window is unmanaged,
  /// minimized, or moved to another workspace.
  pub fn focus_target_after_removal(
    &self,
    removed_window: &WindowContainer,
  ) -> Option<Container> {
    // If the removed window is not focused, no need to change focus.
    if self.focused_container() != Some(removed_window.clone().into()) {
      return None;
    }

    // Get descendant focus order excluding the removed container.
    let workspace = removed_window.workspace()?;
    let descendant_focus_order = workspace
      .descendant_focus_order()
      .filter(|descendant| descendant.id() != removed_window.id())
      .collect::<Vec<_>>();

    // Windows such as an application's Save As dialog can be created as a
    // managed floating window even though they are transient popups from
    // the user's perspective. When one closes, the native owner is the
    // correct focus target. Otherwise the usual same-state fallback
    // below can focus a detached peer instead (for example, Notepad),
    // which then promotes that peer above the tiled owner.
    #[cfg(target_os = "windows")]
    if let Some(owner_handle) =
      removed_window.native().debug_info().owner_handle
    {
      if let Some(owner_target) = managed_owner_focus_target(
        &descendant_focus_order,
        &workspace,
        owner_handle,
      ) {
        return Some(owner_target);
      }
    }

    // Get focus target that matches the removed window type. This applies
    // for windows that aren't in a minimized state.
    let focus_target_of_type = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| {
        matches!(
          (descendant.state(), removed_window.state()),
          (WindowState::Tiling, WindowState::Tiling)
            | (WindowState::Floating(_), WindowState::Floating(_))
            | (WindowState::Fullscreen(_), WindowState::Fullscreen(_))
        )
      })
      .map(Into::into);

    if focus_target_of_type.is_some() {
      return focus_target_of_type;
    }

    let non_minimized_focus_target = descendant_focus_order
      .iter()
      .filter_map(|descendant| descendant.as_window_container().ok())
      .find(|descendant| descendant.state() != WindowState::Minimized)
      .map(Into::into);

    non_minimized_focus_target.or(Some(workspace.into()))
  }

  /// Returns all containers that contain the given point.
  #[allow(clippy::unused_self)]
  pub fn containers_at_point(
    &self,
    origin_container: &Container,
    point: &Point,
  ) -> Vec<Container> {
    origin_container
      .descendants()
      .filter(|descendant| {
        descendant
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .collect()
  }

  /// Returns the monitor that contains the given point.
  pub fn monitor_at_point(&self, point: &Point) -> Option<Monitor> {
    self
      .monitors()
      .iter()
      .find(|monitor| {
        monitor
          .to_rect()
          .is_ok_and(|rect| rect.contains_point(point))
      })
      .cloned()
  }

  /// Records that `native_window` owns native foreground and cancels
  /// pending focus or z-order work.
  ///
  /// Also invalidates in-flight Windows z-order retries. A retry scheduled
  /// before this focus event must not replay a chain whose head is the
  /// previous managed window.
  pub fn suspend_z_order_for_ignored_foreground(
    &mut self,
    native_window: &NativeWindow,
  ) {
    let window_id = native_window.id();
    let dropped_foreground_work =
      self.pending_sync.has_foreground_assertions();

    self.ignored_native_foreground = Some(window_id);
    self.pending_sync.cancel_foreground_assertions();
    self.invalidate_pending_z_order_retries();

    if dropped_foreground_work {
      crate::commands::general::layout_debug_log(format!(
        "ignored foreground suspended z-order handle={window_id:?}"
      ));
    }
  }

  /// Returns whether z-order and focus enforcement must not run.
  ///
  /// The suspension flag covers work that is drained immediately after
  /// the ignored-focus event. The live foreground check covers a
  /// reconciliation that was already inside `platform_sync` when native
  /// focus changed, and any later redraw while that window is still
  /// foreground. An unrelated unmanaged foreground window does not clear
  /// the flag; only a managed window or the desktop does.
  pub fn ignored_foreground_suspends_z_order(&mut self) -> bool {
    if let Ok(foreground) = self.dispatcher.focused_window() {
      if self
        .ignored_windows
        .iter()
        .any(|window| window == &foreground)
      {
        self.suspend_z_order_for_ignored_foreground(&foreground);
        return true;
      }

      let foreground_is_managed =
        self.window_from_native(&foreground).is_some();
      let foreground_is_desktop =
        foreground.is_desktop_window().unwrap_or(false);

      if foreground_is_managed || foreground_is_desktop {
        if self.ignored_native_foreground.take().is_some() {
          crate::commands::general::layout_debug_log(
            "ignored foreground suspension cleared by native focus",
          );
        }
        return false;
      }
    }

    if self.ignored_native_foreground.is_some() {
      let dropped_foreground_work =
        self.pending_sync.has_foreground_assertions();
      self.pending_sync.cancel_foreground_assertions();
      self.invalidate_pending_z_order_retries();
      if dropped_foreground_work {
        crate::commands::general::layout_debug_log(
          "skipped z-order reconcile while ignored window owns native foreground",
        );
      }
      return true;
    }

    false
  }

  /// Bumps the Windows z-order generation so delayed retries abort.
  fn invalidate_pending_z_order_retries(&self) {
    #[cfg(target_os = "windows")]
    {
      let _ = wm_platform::begin_z_order_batch();
    }
  }

  /// Best-effort: manage any currently visible top-level windows that are
  /// not already in the container tree (e.g. after uncloaking orphans).
  pub fn manage_new_visible_windows(
    &mut self,
    config: &mut UserConfig,
  ) -> anyhow::Result<()> {
    for native_window in
      self.dispatcher.visible_windows()?.into_iter().rev()
    {
      if self.window_from_native(&native_window).is_some() {
        continue;
      }
      let nearest_workspace = self
        .nearest_monitor(&native_window)
        .and_then(|m| m.displayed_workspace());
      if let Some(workspace) = nearest_workspace {
        manage_window(
          native_window,
          Some(workspace.into()),
          self,
          config,
        )?;
      }
    }
    Ok(())
  }

  /// Cleans up windows that are no longer alive.
  ///
  /// This addresses the "ghost window" issue where applications may
  /// terminate without sending window destroy events, leaving invalid
  /// windows in WM state.
  ///
  /// See: <https://github.com/glzr-io/glazewm/issues/1219>
  pub fn cleanup_invalid_windows(&mut self) -> anyhow::Result<()> {
    let invalid_windows = self
      .windows()
      .into_iter()
      .filter(|window| !window.native().is_valid());

    for window in invalid_windows {
      tracing::info!("Removing invalid window: {}", window);
      unmanage_window(window, self)?;
    }

    Ok(())
  }
}

/// Finds a managed native owner in the removed window's workspace.
///
/// Keeping this selection separate makes the transient-popup behavior
/// independently testable without depending on a real Win32 owner
/// relation.
#[cfg(target_os = "windows")]
fn managed_owner_focus_target(
  descendants: &[Container],
  workspace: &Workspace,
  owner_handle: isize,
) -> Option<Container> {
  descendants
    .iter()
    .filter_map(|descendant| descendant.as_window_container().ok())
    .find(|candidate| {
      candidate.native().id().0 == owner_handle
        && candidate.workspace().is_some_and(|candidate_workspace| {
          candidate_workspace.id() == workspace.id()
        })
    })
    .map(Into::into)
}

impl Drop for WmState {
  fn drop(&mut self) {
    let managed_windows = self.windows();

    for window in &managed_windows {
      // Redraw windows to their intended positions. On macOS, this will
      // unhide windows that are on other workspaces.
      if let Ok(rect) = window.to_rect() {
        if let Err(err) = window.native().set_frame(&rect) {
          warn!("Failed to redraw window on cleanup: {:?}", err);
        }
      }

      // Reset any effects on Windows.
      #[cfg(target_os = "windows")]
      {
        if let Err(err) = window.native().show() {
          warn!("Failed to show window: {:?}", err);
        }

        let _ = window.native().set_taskbar_visibility(true);
        let _ = window.native().set_border_color(None);
        let _ = window
          .native()
          .set_transparency(&OpacityValue::from_alpha(u8::MAX));
      }
    }
  }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::{
    GapsConfig, TilingDirection, WindowState, WorkspaceConfig,
  };
  use wm_platform::{
    EventLoop, NativeWindow, NativeWindowWindowsExt, Rect, RectDelta,
  };

  use super::*;
  use crate::{
    commands::{container::attach_container, monitor::add_monitor},
    models::{
      Container, NativeMonitorProperties, NativeWindowProperties,
      NonTilingWindow, TilingWindow, Workspace,
    },
    traits::CommonGetters,
  };

  fn test_properties(title: &str) -> NativeWindowProperties {
    NativeWindowProperties {
      title: title.into(),
      class_name: "test".into(),
      process_name: "test".into(),
      process_path: None,
      frame: Rect::from_xy(0, 0, 100, 100),
      is_minimized: false,
      is_maximized: false,
      is_resizable: true,
      shadow_borders: RectDelta::zero(),
    }
  }

  #[test]
  fn transient_owned_popup_removal_prefers_managed_owner() {
    let (_event_loop, dispatcher) = EventLoop::new().expect("event loop");
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let mut state = WmState::new(dispatcher, event_tx, exit_tx);

    let display = state
      .dispatcher
      .sorted_displays()
      .expect("test display")
      .into_iter()
      .next()
      .expect("at least one display");
    let monitor_properties = NativeMonitorProperties::try_from(&display)
      .expect("monitor properties");
    let monitor = add_monitor(display, monitor_properties, &mut state)
      .expect("attach test monitor");

    let workspace = Workspace::new(
      WorkspaceConfig {
        name: "owned-popup-removal-test".into(),
        display_name: None,
        bind_to_monitor: None,
        keep_alive: false,
      },
      GapsConfig::default(),
      TilingDirection::Horizontal,
    );
    let workspace_container: Container = workspace.clone().into();
    attach_container(&workspace_container, &monitor.clone().into(), None)
      .expect("attach workspace");

    let owner = TilingWindow::new(
      None,
      NativeWindow::from_handle(1),
      test_properties("managed owner"),
      None,
      RectDelta::zero(),
      Rect::from_xy(0, 0, 100, 100),
      false,
      GapsConfig::default(),
      Vec::new(),
      None,
    );
    let popup = NonTilingWindow::new(
      None,
      NativeWindow::from_handle(3),
      test_properties("owned popup"),
      WindowState::Floating(Default::default()),
      Some(WindowState::Tiling),
      RectDelta::zero(),
      None,
      Rect::from_xy(0, 0, 100, 100),
      false,
      Vec::new(),
      None,
    );
    let detached_peer = NonTilingWindow::new(
      None,
      NativeWindow::from_handle(2),
      test_properties("detached peer"),
      WindowState::Floating(Default::default()),
      Some(WindowState::Tiling),
      RectDelta::zero(),
      None,
      Rect::from_xy(0, 0, 100, 100),
      false,
      Vec::new(),
      None,
    );

    attach_container(&owner.clone().into(), &workspace_container, None)
      .expect("attach owner");
    attach_container(&popup.clone().into(), &workspace_container, None)
      .expect("attach popup");
    attach_container(
      &detached_peer.clone().into(),
      &workspace_container,
      None,
    )
    .expect("attach detached peer");

    let descendants =
      workspace.descendant_focus_order().collect::<Vec<_>>();
    let target = managed_owner_focus_target(&descendants, &workspace, 1)
      .expect("managed owner target");

    assert_eq!(target.id(), owner.id());
    assert_ne!(target.id(), detached_peer.id());
  }
}
