//! Geometry-based directional focus selection.

use wm_platform::{Direction, Point, Rect};

/// Selects the candidate rectangle nearest to the focused rectangle's
/// directional edge midpoint.
///
/// Candidates are considered only when they are wholly in the requested
/// direction. A candidate containing the edge midpoint therefore wins with
/// a distance of zero. This makes the rule easy to reason about for nested
/// layouts: focus right means "the window that contains, or is closest to,
/// the middle of my right edge".
///
/// The returned value is the index of the selected candidate. Ties are
/// broken first by distance along the requested axis, then by distance
/// along the perpendicular axis, and finally by input order.
#[must_use]
pub fn geometric_focus_target_index(
  origin: &Rect,
  direction: &Direction,
  candidates: &[Rect],
) -> Option<usize> {
  let anchor = directional_edge_midpoint(origin, direction);

  candidates
    .iter()
    .enumerate()
    .filter_map(|(index, candidate)| {
      let (primary_distance, secondary_distance) =
        directional_distances(origin, candidate, direction)?;
      let distance = squared_distance_to_rect(&anchor, candidate);

      Some((distance, primary_distance, secondary_distance, index))
    })
    .min_by_key(|(distance, primary, secondary, index)| {
      (*distance, *primary, *secondary, *index)
    })
    .map(|(_, _, _, index)| index)
}

fn directional_edge_midpoint(
  origin: &Rect,
  direction: &Direction,
) -> Point {
  match direction {
    Direction::Up => Point {
      x: origin.left + origin.width() / 2,
      y: origin.top,
    },
    Direction::Down => Point {
      x: origin.left + origin.width() / 2,
      y: origin.bottom,
    },
    Direction::Left => Point {
      x: origin.left,
      y: origin.top + origin.height() / 2,
    },
    Direction::Right => Point {
      x: origin.right,
      y: origin.top + origin.height() / 2,
    },
  }
}

/// Returns distance along the requested axis and perpendicular axis when
/// the candidate is in the requested direction.
fn directional_distances(
  origin: &Rect,
  candidate: &Rect,
  direction: &Direction,
) -> Option<(i64, i64)> {
  let candidate_center = candidate.center_point();
  let origin_center = origin.center_point();

  let (primary_distance, secondary_distance, is_directional) =
    match direction {
      Direction::Up => (
        i64::from(origin.top) - i64::from(candidate.bottom),
        (i64::from(candidate_center.x) - i64::from(origin_center.x)).abs(),
        candidate.bottom <= origin.top,
      ),
      Direction::Down => (
        i64::from(candidate.top) - i64::from(origin.bottom),
        (i64::from(candidate_center.x) - i64::from(origin_center.x)).abs(),
        candidate.top >= origin.bottom,
      ),
      Direction::Left => (
        i64::from(origin.left) - i64::from(candidate.right),
        (i64::from(candidate_center.y) - i64::from(origin_center.y)).abs(),
        candidate.right <= origin.left,
      ),
      Direction::Right => (
        i64::from(candidate.left) - i64::from(origin.right),
        (i64::from(candidate_center.y) - i64::from(origin_center.y)).abs(),
        candidate.left >= origin.right,
      ),
    };

  is_directional.then_some((primary_distance, secondary_distance))
}

fn squared_distance_to_rect(point: &Point, rect: &Rect) -> i64 {
  let dx = axis_distance(i64::from(point.x), rect.left, rect.right);
  let dy = axis_distance(i64::from(point.y), rect.top, rect.bottom);
  dx * dx + dy * dy
}

fn axis_distance(point: i64, min: i32, max: i32) -> i64 {
  if point < i64::from(min) {
    i64::from(min) - point
  } else if point > i64::from(max) {
    point - i64::from(max)
  } else {
    0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn right_selects_window_containing_middle_right() {
    // Visual layout:
    //
    // 124
    // 125
    // 136
    //
    // Window 1 spans the left side. Its right-edge midpoint is inside 2.
    let candidates = [
      Rect::from_xy(100, 0, 100, 200),
      Rect::from_xy(100, 200, 100, 100),
      Rect::from_xy(200, 0, 100, 100),
      Rect::from_xy(200, 100, 100, 100),
      Rect::from_xy(200, 200, 100, 100),
    ];

    assert_eq!(
      geometric_focus_target_index(
        &Rect::from_xy(0, 0, 100, 300),
        &Direction::Right,
        &candidates,
      ),
      Some(0),
    );
  }

  #[test]
  fn each_arrow_uses_the_matching_edge_midpoint() {
    let origin = Rect::from_xy(100, 100, 100, 100);
    let candidates = [
      Rect::from_xy(100, 0, 100, 100),
      Rect::from_xy(100, 200, 100, 100),
      Rect::from_xy(0, 100, 100, 100),
      Rect::from_xy(200, 100, 100, 100),
    ];

    assert_eq!(
      geometric_focus_target_index(&origin, &Direction::Up, &candidates),
      Some(0),
    );
    assert_eq!(
      geometric_focus_target_index(&origin, &Direction::Down, &candidates),
      Some(1),
    );
    assert_eq!(
      geometric_focus_target_index(&origin, &Direction::Left, &candidates),
      Some(2),
    );
    assert_eq!(
      geometric_focus_target_index(
        &origin,
        &Direction::Right,
        &candidates
      ),
      Some(3),
    );
  }

  #[test]
  fn ignores_candidates_not_in_requested_direction() {
    let candidates = [
      Rect::from_xy(-100, 0, 100, 100),
      Rect::from_xy(200, 100, 100, 100),
    ];

    assert_eq!(
      geometric_focus_target_index(
        &Rect::from_xy(0, 100, 100, 100),
        &Direction::Right,
        &candidates,
      ),
      Some(1),
    );
  }

  #[test]
  fn input_order_breaks_exact_ties() {
    let candidates = [
      Rect::from_xy(100, 0, 100, 50),
      Rect::from_xy(100, 150, 100, 50),
    ];

    assert_eq!(
      geometric_focus_target_index(
        &Rect::from_xy(0, 50, 100, 100),
        &Direction::Right,
        &candidates,
      ),
      Some(0),
    );
  }
}
