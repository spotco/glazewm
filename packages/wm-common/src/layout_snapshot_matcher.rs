//! Pure best-effort matching of snapshot windows to live windows.
//!
//! No HWND / platform types - callers supply identity fields and opaque ids.

use serde::{Deserialize, Serialize};

use crate::{
  LayoutSnapshot, SnapshotNode, SnapshotNodeKind, SnapshotWindow,
  SnapshotWindowIdentity,
};

/// Minimum score required to accept a match. Weak pairs are skipped.
pub const MATCH_SCORE_THRESHOLD: u32 = 50;

const SCORE_PROCESS_PATH: u32 = 100;
const SCORE_PROCESS_NAME: u32 = 50;
const SCORE_CLASS_NAME: u32 = 25;
const SCORE_TITLE_EXACT: u32 = 40;
const SCORE_TITLE_CONTAINS: u32 = 20;
const SCORE_TITLE_FUZZY: u32 = 10;

/// Identity fields used for scoring (snapshot or live).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchableIdentity {
  pub process_path: Option<String>,
  pub process_name: String,
  pub class_name: Option<String>,
  pub title: Option<String>,
}

impl From<&SnapshotWindowIdentity> for MatchableIdentity {
  fn from(identity: &SnapshotWindowIdentity) -> Self {
    Self {
      process_path: identity.process_path.clone(),
      process_name: identity.process_name.clone(),
      class_name: identity.class_name.clone(),
      title: identity.title_hint.clone(),
    }
  }
}

impl From<&SnapshotWindow> for MatchableIdentity {
  fn from(window: &SnapshotWindow) -> Self {
    MatchableIdentity::from(&window.identity)
  }
}

/// A live or snapshot window entry for matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchableWindow {
  /// Opaque key (e.g. live container Uuid string, or snapshot local id path).
  pub key: String,
  pub identity: MatchableIdentity,
}

/// Inspectable window identity + workspace for match reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutMatchWindow {
  pub key: String,
  pub process_name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub process_path: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub class_name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub workspace: Option<String>,
}

impl LayoutMatchWindow {
  #[must_use]
  pub fn from_matchable(
    window: &MatchableWindow,
    workspace: Option<String>,
  ) -> Self {
    Self {
      key: window.key.clone(),
      process_name: window.identity.process_name.clone(),
      process_path: window.identity.process_path.clone(),
      class_name: window.identity.class_name.clone(),
      title: window.identity.title.clone(),
      workspace,
    }
  }

  #[must_use]
  pub fn to_matchable(&self) -> MatchableWindow {
    MatchableWindow {
      key: self.key.clone(),
      identity: MatchableIdentity {
        process_path: self.process_path.clone(),
        process_name: self.process_name.clone(),
        class_name: self.class_name.clone(),
        title: self.title.clone(),
      },
    }
  }
}

/// Planned workspace reassignment for a matched pair (dry-run only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedWorkspaceMove {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub from: Option<String>,
  pub to: String,
}

/// One accepted snapshot ↔ live match with score and optional plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutMatchedPair {
  pub snapshot: LayoutMatchWindow,
  pub live: LayoutMatchWindow,
  pub score: u32,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub planned_workspace_move: Option<PlannedWorkspaceMove>,
}

/// Dry-run layout match report (no WM mutation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutMatchReport {
  pub matched: Vec<LayoutMatchedPair>,
  pub unmatched_snapshot: Vec<LayoutMatchWindow>,
  pub unmatched_live: Vec<LayoutMatchWindow>,
}

/// Score how well `candidate` matches `target` (snapshot identity).
///
/// Higher is better. Scoring is additive across independent signals.
#[must_use]
pub fn score_window(
  candidate: &MatchableIdentity,
  target: &MatchableIdentity,
) -> u32 {
  let mut score = 0;

  if let (Some(c_path), Some(t_path)) =
    (&candidate.process_path, &target.process_path)
  {
    if eq_ignore_ascii_case(c_path, t_path) {
      score += SCORE_PROCESS_PATH;
    }
  }

  if eq_ignore_ascii_case(&candidate.process_name, &target.process_name) {
    score += SCORE_PROCESS_NAME;
  }

  if let (Some(c_class), Some(t_class)) =
    (&candidate.class_name, &target.class_name)
  {
    if c_class == t_class {
      score += SCORE_CLASS_NAME;
    }
  }

  score += title_score(
    candidate.title.as_deref(),
    target.title.as_deref(),
  );

  score
}

fn title_score(candidate: Option<&str>, target: Option<&str>) -> u32 {
  let (Some(c), Some(t)) = (candidate, target) else {
    return 0;
  };

  let c_norm = normalize_title(c);
  let t_norm = normalize_title(t);

  if c_norm.is_empty() || t_norm.is_empty() {
    return 0;
  }

  if c_norm == t_norm {
    return SCORE_TITLE_EXACT;
  }

  if c_norm.contains(&t_norm) || t_norm.contains(&c_norm) {
    return SCORE_TITLE_CONTAINS;
  }

  // Simple fuzzy: compare without whitespace / punctuation collapses already
  // handled by normalize; treat near-equal prefixes as weak fuzzy.
  // Use char counts / .chars().take so we never byte-slice mid code point.
  let min_chars = c_norm.chars().count().min(t_norm.chars().count());
  if min_chars >= 4 {
    let prefix_len = min_chars.min(12);
    let c_pref: String = c_norm.chars().take(prefix_len).collect();
    let t_pref: String = t_norm.chars().take(prefix_len).collect();
    if c_pref == t_pref {
      return SCORE_TITLE_FUZZY;
    }
  }

  0
}

fn normalize_title(title: &str) -> String {
  title
    .chars()
    .filter(|c| c.is_alphanumeric() || c.is_whitespace())
    .flat_map(char::to_lowercase)
    .collect::<String>()
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
  a.eq_ignore_ascii_case(b)
}

/// Greedy best-score matching. Each live/snapshot key is assigned at most once.
/// Pairs below [`MATCH_SCORE_THRESHOLD`] are skipped.
#[must_use]
pub fn match_windows(
  snapshot_windows: &[MatchableWindow],
  live_windows: &[MatchableWindow],
) -> Vec<(String, String)> {
  match_windows_with_scores(snapshot_windows, live_windows)
    .into_iter()
    .map(|(s, l, _)| (s, l))
    .collect()
}

/// Like [`match_windows`], but includes the accepted pair score.
#[must_use]
pub fn match_windows_with_scores(
  snapshot_windows: &[MatchableWindow],
  live_windows: &[MatchableWindow],
) -> Vec<(String, String, u32)> {
  let mut pairs: Vec<(u32, usize, usize)> = Vec::new();

  for (s_idx, snap) in snapshot_windows.iter().enumerate() {
    for (l_idx, live) in live_windows.iter().enumerate() {
      let score = score_window(&live.identity, &snap.identity);
      if score >= MATCH_SCORE_THRESHOLD {
        pairs.push((score, s_idx, l_idx));
      }
    }
  }

  pairs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

  let mut used_snap = vec![false; snapshot_windows.len()];
  let mut used_live = vec![false; live_windows.len()];
  let mut result = Vec::new();

  for &(score, s_idx, l_idx) in &pairs {
    if used_snap[s_idx] || used_live[l_idx] {
      continue;
    }
    used_snap[s_idx] = true;
    used_live[l_idx] = true;
    result.push((
      snapshot_windows[s_idx].key.clone(),
      live_windows[l_idx].key.clone(),
      score,
    ));
  }

  result
}

/// Build an inspectable match report (scores, unmatched, planned moves).
///
/// Does not mutate any WM state. `planned_workspace_move` is set when the
/// snapshot workspace name differs from the live window's current workspace.
#[must_use]
pub fn build_layout_match_report(
  snapshot_windows: &[LayoutMatchWindow],
  live_windows: &[LayoutMatchWindow],
) -> LayoutMatchReport {
  let snap_matchables: Vec<_> =
    snapshot_windows.iter().map(LayoutMatchWindow::to_matchable).collect();
  let live_matchables: Vec<_> =
    live_windows.iter().map(LayoutMatchWindow::to_matchable).collect();

  let scored = match_windows_with_scores(&snap_matchables, &live_matchables);

  let snap_by_key: std::collections::HashMap<_, _> = snapshot_windows
    .iter()
    .map(|w| (w.key.clone(), w))
    .collect();
  let live_by_key: std::collections::HashMap<_, _> = live_windows
    .iter()
    .map(|w| (w.key.clone(), w))
    .collect();

  let mut matched = Vec::with_capacity(scored.len());
  let mut used_snap = std::collections::HashSet::new();
  let mut used_live = std::collections::HashSet::new();

  for (snap_key, live_key, score) in scored {
    used_snap.insert(snap_key.clone());
    used_live.insert(live_key.clone());

    let Some(snap) = snap_by_key.get(&snap_key) else {
      continue;
    };
    let Some(live) = live_by_key.get(&live_key) else {
      continue;
    };

    let planned_workspace_move = match (&snap.workspace, &live.workspace) {
      (Some(to), from) if from.as_ref() != Some(to) => {
        Some(PlannedWorkspaceMove {
          from: from.clone(),
          to: to.clone(),
        })
      }
      _ => None,
    };

    matched.push(LayoutMatchedPair {
      snapshot: (*snap).clone(),
      live: (*live).clone(),
      score,
      planned_workspace_move,
    });
  }

  let unmatched_snapshot = snapshot_windows
    .iter()
    .filter(|w| !used_snap.contains(&w.key))
    .cloned()
    .collect();
  let unmatched_live = live_windows
    .iter()
    .filter(|w| !used_live.contains(&w.key))
    .cloned()
    .collect();

  LayoutMatchReport {
    matched,
    unmatched_snapshot,
    unmatched_live,
  }
}

/// Collect snapshot window leaves as [`LayoutMatchWindow`] for dry-run matching.
///
/// Keys match the restore path: `{workspace}::{local_id}::{order}`.
#[must_use]
pub fn collect_snapshot_windows_for_match(
  snapshot: &LayoutSnapshot,
) -> Vec<LayoutMatchWindow> {
  let mut leaves = Vec::new();
  let mut seen_workspace_keys = std::collections::HashSet::new();

  for snap_mon in &snapshot.monitors {
    for workspace in &snap_mon.workspaces {
      let ws_key = format!("{}::{}", snap_mon.device_name, workspace.name);
      if !seen_workspace_keys.insert(ws_key) {
        continue;
      }
      let mut order = 0usize;
      collect_node_match_windows(
        &workspace.root,
        &workspace.name,
        &mut order,
        &mut leaves,
      );
    }
  }

  leaves
}

fn collect_node_match_windows(
  node: &SnapshotNode,
  workspace_name: &str,
  order: &mut usize,
  out: &mut Vec<LayoutMatchWindow>,
) {
  match node.kind {
    SnapshotNodeKind::Window => {
      if let Some(window) = &node.window {
        let identity = MatchableIdentity::from(window);
        out.push(LayoutMatchWindow {
          key: format!("{}::{}::{}", workspace_name, node.local_id, *order),
          process_name: identity.process_name,
          process_path: identity.process_path,
          class_name: identity.class_name,
          title: identity.title,
          workspace: Some(workspace_name.to_string()),
        });
        *order += 1;
      }
    }
    SnapshotNodeKind::Split => {
      if let Some(children) = &node.children {
        for child in children {
          collect_node_match_windows(child, workspace_name, order, out);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ident(
    path: Option<&str>,
    name: &str,
    class: Option<&str>,
    title: Option<&str>,
  ) -> MatchableIdentity {
    MatchableIdentity {
      process_path: path.map(str::to_string),
      process_name: name.to_string(),
      class_name: class.map(str::to_string),
      title: title.map(str::to_string),
    }
  }

  fn win(key: &str, identity: MatchableIdentity) -> MatchableWindow {
    MatchableWindow {
      key: key.to_string(),
      identity,
    }
  }

  fn match_win(
    key: &str,
    name: &str,
    title: Option<&str>,
    workspace: Option<&str>,
  ) -> LayoutMatchWindow {
    LayoutMatchWindow {
      key: key.to_string(),
      process_name: name.to_string(),
      process_path: None,
      class_name: None,
      title: title.map(str::to_string),
      workspace: workspace.map(str::to_string),
    }
  }

  #[test]
  fn perfect_path_match_scores_high() {
    let snap = ident(
      Some(r"C:\Program Files\App\app.exe"),
      "app",
      Some("AppClass"),
      Some("Hello"),
    );
    let live = ident(
      Some(r"c:\program files\app\app.exe"),
      "APP",
      Some("AppClass"),
      Some("Hello"),
    );
    let score = score_window(&live, &snap);
    assert!(score >= SCORE_PROCESS_PATH + SCORE_PROCESS_NAME);
    assert!(score >= MATCH_SCORE_THRESHOLD);
  }

  #[test]
  fn name_and_title_match_without_path() {
    let snap = ident(None, "code", None, Some("main.rs - Visual Studio Code"));
    let live = ident(
      None,
      "Code",
      Some("Chrome_WidgetWin_1"),
      Some("main.rs - Visual Studio Code"),
    );
    let score = score_window(&live, &snap);
    assert!(score >= SCORE_PROCESS_NAME + SCORE_TITLE_EXACT);
    assert!(score >= MATCH_SCORE_THRESHOLD);
  }

  #[test]
  fn ambiguous_same_process_different_titles() {
    let snaps = vec![
      win(
        "snap-a",
        ident(None, "notepad", None, Some("notes.txt - Notepad")),
      ),
      win(
        "snap-b",
        ident(None, "notepad", None, Some("todo.txt - Notepad")),
      ),
    ];
    let lives = vec![
      win(
        "live-todo",
        ident(None, "notepad", None, Some("todo.txt - Notepad")),
      ),
      win(
        "live-notes",
        ident(None, "notepad", None, Some("notes.txt - Notepad")),
      ),
    ];

    let matched = match_windows(&snaps, &lives);
    assert_eq!(matched.len(), 2);
    assert!(matched.contains(&(
      "snap-a".to_string(),
      "live-notes".to_string()
    )));
    assert!(matched.contains(&(
      "snap-b".to_string(),
      "live-todo".to_string()
    )));
  }

  #[test]
  fn no_double_assign_when_scores_collide() {
    let snaps = vec![
      win("snap-1", ident(None, "chrome", None, Some("Tab A"))),
      win("snap-2", ident(None, "chrome", None, Some("Tab B"))),
    ];
    // Only one live chrome window - second snapshot must remain unmatched.
    let lives = vec![win(
      "live-1",
      ident(None, "chrome", None, Some("Tab A")),
    )];

    let matched = match_windows(&snaps, &lives);
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0], ("snap-1".to_string(), "live-1".to_string()));
  }

  #[test]
  fn weak_match_skipped_by_threshold() {
    // Different process names, only weak title overlap → below threshold.
    let snap = ident(None, "alpha", None, Some("Document"));
    let live = ident(None, "beta", None, Some("Document Draft"));
    let score = score_window(&live, &snap);
    assert!(score < MATCH_SCORE_THRESHOLD);

    let matched = match_windows(
      &[win("s", snap)],
      &[win("l", live)],
    );
    assert!(matched.is_empty());
  }

  #[test]
  fn title_contains_scores_medium() {
    let snap = ident(None, "firefox", None, Some("GitHub"));
    let live = ident(
      None,
      "firefox",
      None,
      Some("GitHub - Pull requests"),
    );
    let score = score_window(&live, &snap);
    assert_eq!(
      score,
      SCORE_PROCESS_NAME + SCORE_TITLE_CONTAINS
    );
  }

  #[test]
  fn fuzzy_title_prefix_handles_multibyte_utf8() {
    // 11 ASCII + 2-byte `é` would put a byte index of 12 mid-codepoint;
    // char-based prefix extraction must not panic and should still fuzzy-match.
    let snap = ident(None, "app", None, Some("aaaaaaaaaaaé rest"));
    let live = ident(None, "app", None, Some("aaaaaaaaaaaé other"));
    let score = score_window(&live, &snap);
    assert_eq!(score, SCORE_PROCESS_NAME + SCORE_TITLE_FUZZY);

    // Mixed Japanese + ASCII titles (regression for non-ASCII normalize path).
    let snap_jp = ident(None, "code", None, Some("編集.rs - 窓"));
    let live_jp = ident(None, "code", None, Some("編集.rs - 窓"));
    let score_jp = score_window(&live_jp, &snap_jp);
    assert!(score_jp >= SCORE_PROCESS_NAME + SCORE_TITLE_EXACT);

    // Emoji + ASCII: must not panic even when titles only weakly overlap.
    let snap_emoji = ident(None, "chat", None, Some("🔥hello world"));
    let live_emoji = ident(None, "chat", None, Some("🔥hello there"));
    let score_emoji = score_window(&live_emoji, &snap_emoji);
    assert!(score_emoji >= SCORE_PROCESS_NAME);
  }

  #[test]
  fn match_windows_with_scores_returns_score() {
    let snaps = vec![win(
      "s",
      ident(None, "notepad", None, Some("a.txt - Notepad")),
    )];
    let lives = vec![win(
      "l",
      ident(None, "notepad", None, Some("a.txt - Notepad")),
    )];
    let scored = match_windows_with_scores(&snaps, &lives);
    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].0, "s");
    assert_eq!(scored[0].1, "l");
    assert!(scored[0].2 >= MATCH_SCORE_THRESHOLD);
  }

  #[test]
  fn build_layout_match_report_includes_planned_move_and_unmatched() {
    let snaps = vec![
      match_win("snap-a", "notepad", Some("a.txt"), Some("work")),
      match_win("snap-b", "chrome", Some("Tab"), Some("web")),
    ];
    let lives = vec![
      match_win("live-a", "notepad", Some("a.txt"), Some("home")),
      match_win("live-c", "spotify", Some("Music"), Some("media")),
    ];

    let report = build_layout_match_report(&snaps, &lives);
    assert_eq!(report.matched.len(), 1);
    assert_eq!(report.matched[0].snapshot.key, "snap-a");
    assert_eq!(report.matched[0].live.key, "live-a");
    assert!(report.matched[0].score >= MATCH_SCORE_THRESHOLD);
    let plan = report.matched[0]
      .planned_workspace_move
      .as_ref()
      .expect("planned move");
    assert_eq!(plan.from.as_deref(), Some("home"));
    assert_eq!(plan.to, "work");

    assert_eq!(report.unmatched_snapshot.len(), 1);
    assert_eq!(report.unmatched_snapshot[0].key, "snap-b");
    assert_eq!(report.unmatched_live.len(), 1);
    assert_eq!(report.unmatched_live[0].key, "live-c");
  }

  #[test]
  fn build_layout_match_report_no_move_when_workspace_same() {
    let snaps = vec![match_win(
      "snap-a",
      "notepad",
      Some("a.txt"),
      Some("work"),
    )];
    let lives = vec![match_win(
      "live-a",
      "notepad",
      Some("a.txt"),
      Some("work"),
    )];
    let report = build_layout_match_report(&snaps, &lives);
    assert_eq!(report.matched.len(), 1);
    assert!(report.matched[0].planned_workspace_move.is_none());
    assert!(report.unmatched_snapshot.is_empty());
    assert!(report.unmatched_live.is_empty());
  }
}
