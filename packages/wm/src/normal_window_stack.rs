//! Logical z-order for normal windows.
//!
//! Windows in a workspace are represented by ordered atoms. All tiled
//! windows share one atom; floating and ignored windows each have their
//! own. Focus changes only move an atom when the operation explicitly
//! requests activation. Lifecycle reconciliation only inserts and removes
//! atoms.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;
use wm_platform::WindowId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum StackAtom {
  TiledGroup { workspace_id: Uuid },
  DetachedWindow { window_id: WindowId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackAtomId {
  TiledGroup,
  DetachedWindow(WindowId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Reclassification is exercised through reducer tests;
                    // runtime membership is reconciled atomically.
pub(crate) enum ReclassifyTarget {
  Tiled,
  Detached,
}

/// One reasoned transition of the logical normal-window stack.
///
/// `Reconcile` receives the current live membership and a native
/// top-to-bottom sample. The native sample is used only to position newly
/// discovered atoms (or seed an empty stack); it never reorders existing
/// atoms.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Lifecycle variants keep the reducer API explicit;
                    // runtime sync applies one atomic workspace snapshot.
pub(crate) enum StackOperation {
  InsertDetached(WindowId),
  Remove(WindowId),
  Reclassify {
    window_id: WindowId,
    target: ReclassifyTarget,
  },
  Activate {
    atom: StackAtomId,
    owner_by_window: HashMap<WindowId, WindowId>,
  },
  /// Focus recovery changes the logical focus target but carries no stack
  /// activation intent, so its reducer transition is deliberately a
  /// no-op.
  FocusRecovery,
  Reconcile {
    tiled_window_ids: Vec<WindowId>,
    detached_window_ids: Vec<WindowId>,
    owner_by_window: HashMap<WindowId, WindowId>,
    native_order: Vec<WindowId>,
  },
}

#[derive(Clone, Debug)]
pub(crate) struct NormalWindowStack {
  workspace_id: Uuid,
  atoms: Vec<StackAtom>,
  tiled_window_ids: HashSet<WindowId>,
  /// Verified, live owner links in this workspace/monitor stack. An owner
  /// may be another detached atom or a HWND inside the tiled atom.
  owner_by_window: HashMap<WindowId, WindowId>,
}

impl NormalWindowStack {
  pub(crate) fn new(workspace_id: Uuid) -> Self {
    Self {
      workspace_id,
      atoms: Vec::new(),
      tiled_window_ids: HashSet::new(),
      owner_by_window: HashMap::new(),
    }
  }

  pub(crate) fn atoms(&self) -> &[StackAtom] {
    &self.atoms
  }

  pub(crate) fn activation_group(
    &self,
    window_id: WindowId,
    tiled_window_ids: &[WindowId],
  ) -> Vec<WindowId> {
    let family = self.activation_family(window_id, tiled_window_ids);
    self
      .atoms
      .iter()
      .flat_map(|atom| match atom {
        StackAtom::TiledGroup { workspace_id }
          if *workspace_id == self.workspace_id
            && tiled_window_ids.iter().any(|id| family.contains(id)) =>
        {
          tiled_window_ids.to_vec()
        }
        StackAtom::DetachedWindow { window_id: id }
          if family.contains(id) =>
        {
          vec![*id]
        }
        _ => Vec::new(),
      })
      .collect()
  }

  fn activation_family(
    &self,
    window_id: WindowId,
    tiled_window_ids: &[WindowId],
  ) -> HashSet<WindowId> {
    if tiled_window_ids.contains(&window_id) {
      tiled_window_ids
        .iter()
        .fold(HashSet::new(), |mut family, id| {
          family.extend(owner_family(*id, &self.owner_by_window));
          family
        })
    } else {
      owner_family(window_id, &self.owner_by_window)
    }
  }

  pub(crate) fn apply(&mut self, operation: StackOperation) -> bool {
    match operation {
      StackOperation::InsertDetached(window_id) => {
        self.insert_detached(window_id)
      }
      StackOperation::Remove(window_id) => self.remove_detached(window_id),
      StackOperation::Reclassify { window_id, target } => {
        self.reclassify(window_id, target)
      }
      StackOperation::Activate {
        atom,
        owner_by_window,
      } => self.activate(atom, &owner_by_window),
      StackOperation::FocusRecovery => false,
      StackOperation::Reconcile {
        tiled_window_ids,
        detached_window_ids,
        owner_by_window,
        native_order,
      } => self.reconcile(
        &tiled_window_ids,
        &detached_window_ids,
        owner_by_window,
        &native_order,
      ),
    }
  }

  /// Materializes atoms as native HWNDs. Tiled HWND ordering is internal
  /// to the tile atom and therefore cannot change its external stack
  /// position.
  pub(crate) fn materialize(
    &self,
    tiled_window_ids: &[WindowId],
  ) -> Vec<WindowId> {
    self
      .atoms
      .iter()
      .flat_map(|atom| match atom {
        StackAtom::TiledGroup { workspace_id }
          if *workspace_id == self.workspace_id =>
        {
          tiled_window_ids.to_vec()
        }
        StackAtom::DetachedWindow { window_id } => vec![*window_id],
        StackAtom::TiledGroup { .. } => Vec::new(),
      })
      .collect()
  }

  fn insert_detached(&mut self, window_id: WindowId) -> bool {
    if self.contains_window(window_id) {
      return false;
    }
    self.atoms.push(StackAtom::DetachedWindow { window_id });
    true
  }

  fn remove_detached(&mut self, window_id: WindowId) -> bool {
    let previous_len = self.atoms.len();
    let was_tiled = self.tiled_window_ids.remove(&window_id);
    self.atoms.retain(|atom| {
      !matches!(atom, StackAtom::DetachedWindow { window_id: id } if *id == window_id)
    });
    if was_tiled && self.tiled_window_ids.is_empty() {
      self.atoms.retain(|atom| {
        !matches!(atom, StackAtom::TiledGroup { workspace_id } if *workspace_id == self.workspace_id)
      });
    }
    self.owner_by_window.remove(&window_id);
    self.owner_by_window.retain(|_, owner| *owner != window_id);
    was_tiled || previous_len != self.atoms.len()
  }

  fn reclassify(
    &mut self,
    window_id: WindowId,
    target: ReclassifyTarget,
  ) -> bool {
    let detached_index = self.atom_index(window_id);
    let tile_index = self.tile_group_index();

    match target {
      ReclassifyTarget::Tiled => {
        if self.tiled_window_ids.contains(&window_id) {
          return false;
        }
        if let Some(index) = detached_index {
          self.atoms.remove(index);
        }
        self.tiled_window_ids.insert(window_id);
        if tile_index.is_some() {
          return detached_index.is_some();
        }
        let insertion_index = detached_index
          .unwrap_or(self.atoms.len())
          .min(self.atoms.len());
        self.atoms.insert(
          insertion_index,
          StackAtom::TiledGroup {
            workspace_id: self.workspace_id,
          },
        );
        true
      }
      ReclassifyTarget::Detached => {
        if !self.tiled_window_ids.remove(&window_id) {
          return if detached_index.is_some() {
            false
          } else {
            self.insert_detached(window_id)
          };
        }
        if let Some(index) = detached_index {
          self.atoms.remove(index);
        }
        let Some(tile_index) = tile_index else {
          return self.insert_detached(window_id);
        };
        let tile_index = tile_index.min(self.atoms.len());
        let group_remains = !self.tiled_window_ids.is_empty();
        if !group_remains {
          self.atoms.remove(tile_index);
        }
        let insertion_index = if group_remains {
          tile_index.saturating_add(1).min(self.atoms.len())
        } else {
          tile_index.min(self.atoms.len())
        };
        self.atoms.insert(
          insertion_index,
          StackAtom::DetachedWindow { window_id },
        );
        true
      }
    }
  }

  fn activate(
    &mut self,
    atom_id: StackAtomId,
    owner_by_window: &HashMap<WindowId, WindowId>,
  ) -> bool {
    let before = self.atoms.clone();
    let live_detached: HashSet<WindowId> = self
      .atoms
      .iter()
      .filter_map(|atom| match atom {
        StackAtom::DetachedWindow { window_id } => Some(*window_id),
        StackAtom::TiledGroup { .. } => None,
      })
      .collect();
    self.owner_by_window = owner_by_window
      .iter()
      .filter(|(window_id, owner_id)| {
        live_detached.contains(window_id)
          && (live_detached.contains(owner_id)
            || self.tiled_window_ids.contains(owner_id))
      })
      .map(|(window_id, owner_id)| (*window_id, *owner_id))
      .collect();
    match atom_id {
      StackAtomId::TiledGroup => {
        if self.tile_group_index().is_some() {
          let family = self.tiled_window_ids.iter().fold(
            HashSet::new(),
            |mut family, tiled_window_id| {
              family.extend(owner_family(
                *tiled_window_id,
                &self.owner_by_window,
              ));
              family
            },
          );
          self.promote_owner_family(
            StackAtom::TiledGroup {
              workspace_id: self.workspace_id,
            },
            &family,
          );
        }
      }
      StackAtomId::DetachedWindow(focused) => {
        if self.atom_index(focused).is_none() {
          return false;
        }
        let family = owner_family(focused, owner_by_window);
        self.promote_owner_family(
          StackAtom::DetachedWindow { window_id: focused },
          &family,
        );
      }
    }
    self.apply_owner_constraints();
    before != self.atoms
  }

  fn reconcile(
    &mut self,
    tiled_window_ids: &[WindowId],
    detached_window_ids: &[WindowId],
    owner_by_window: HashMap<WindowId, WindowId>,
    native_order: &[WindowId],
  ) -> bool {
    let before = self.atoms.clone();
    let tiles = deduplicate(tiled_window_ids);
    let detached = deduplicate(detached_window_ids)
      .into_iter()
      .filter(|id| !tiles.contains(id))
      .collect::<Vec<_>>();
    let live_detached: HashSet<WindowId> =
      detached.iter().copied().collect();
    self.tiled_window_ids = tiles.iter().copied().collect();

    self.atoms.retain(|atom| match atom {
      StackAtom::DetachedWindow { window_id } => {
        live_detached.contains(window_id)
      }
      StackAtom::TiledGroup { workspace_id } => {
        *workspace_id == self.workspace_id && !tiles.is_empty()
      }
    });
    deduplicate_atoms(&mut self.atoms);

    let native_rank: HashMap<WindowId, usize> = native_order
      .iter()
      .enumerate()
      .map(|(rank, id)| (*id, rank))
      .collect();

    if !tiles.is_empty() && self.tile_group_index().is_none() {
      let group_rank = tiles
        .iter()
        .filter_map(|id| native_rank.get(id))
        .copied()
        .min()
        .unwrap_or(usize::MAX);
      let insertion_index = self
        .atoms
        .iter()
        .position(|atom| {
          atom_native_rank(atom, &tiles, &native_rank) > group_rank
        })
        .unwrap_or(self.atoms.len());
      self.atoms.insert(
        insertion_index,
        StackAtom::TiledGroup {
          workspace_id: self.workspace_id,
        },
      );
    }

    // Newly discovered windows use their observed native slot. Existing
    // atoms keep their pairwise order regardless of the sample.
    for window_id in detached {
      if self.atom_index(window_id).is_none() {
        let rank =
          native_rank.get(&window_id).copied().unwrap_or(usize::MAX);
        let index = self
          .atoms
          .iter()
          .position(|atom| {
            atom_native_rank(atom, &tiles, &native_rank) > rank
          })
          .unwrap_or(self.atoms.len());
        self
          .atoms
          .insert(index, StackAtom::DetachedWindow { window_id });
      }
    }

    self.owner_by_window = owner_by_window
      .into_iter()
      .filter(|(window_id, owner_id)| {
        live_detached.contains(window_id)
          && (live_detached.contains(owner_id)
            || self.tiled_window_ids.contains(owner_id))
          && window_id != owner_id
      })
      .collect();
    self.apply_owner_constraints();
    before != self.atoms
  }

  fn apply_owner_constraints(&mut self) {
    // Stable topological ordering preserves the current atom order
    // wherever Win32's owned-above-owner constraint does not require
    // movement.
    let rank: HashMap<StackAtom, usize> = self
      .atoms
      .iter()
      .copied()
      .enumerate()
      .map(|(index, atom)| (atom, index))
      .collect();
    let mut indegree: HashMap<StackAtom, usize> =
      self.atoms.iter().copied().map(|atom| (atom, 0)).collect();
    let mut children: HashMap<StackAtom, Vec<StackAtom>> = HashMap::new();
    for (child, owner) in &self.owner_by_window {
      let child = StackAtom::DetachedWindow { window_id: *child };
      let owner = if self.tiled_window_ids.contains(owner) {
        StackAtom::TiledGroup {
          workspace_id: self.workspace_id,
        }
      } else {
        StackAtom::DetachedWindow { window_id: *owner }
      };
      if rank.contains_key(&child) && rank.contains_key(&owner) {
        children.entry(child).or_default().push(owner);
        *indegree.entry(owner).or_default() += 1;
      }
    }

    let mut ordered = Vec::with_capacity(self.atoms.len());
    while ordered.len() < self.atoms.len() {
      let next = self
        .atoms
        .iter()
        .copied()
        .filter(|atom| !ordered.contains(atom))
        .filter(|atom| {
          indegree.get(atom).copied().unwrap_or_default() == 0
        })
        .min_by_key(|atom| rank.get(atom).copied().unwrap_or(usize::MAX));
      let Some(next) = next else {
        // Invalid owner cycles are native data anomalies. Leave the prior
        // order intact instead of oscillating or dropping a live atom.
        return;
      };
      ordered.push(next);
      if let Some(owners) = children.get(&next) {
        for owner in owners {
          if let Some(degree) = indegree.get_mut(owner) {
            *degree = degree.saturating_sub(1);
          }
        }
      }
    }
    self.atoms = ordered;
  }

  fn promote_owner_family(
    &mut self,
    target: StackAtom,
    family: &HashSet<WindowId>,
  ) {
    let in_family = |atom: &StackAtom| match atom {
      StackAtom::TiledGroup { workspace_id } => {
        *workspace_id == self.workspace_id
          && self.tiled_window_ids.iter().any(|id| family.contains(id))
      }
      StackAtom::DetachedWindow { window_id } => {
        family.contains(window_id)
      }
    };
    let mut promoted = vec![target];
    promoted.extend(
      self
        .atoms
        .iter()
        .copied()
        .filter(|atom| *atom != target && in_family(atom)),
    );
    self.atoms.retain(|atom| !in_family(atom));
    self.atoms.splice(0..0, promoted);
  }

  fn contains_window(&self, window_id: WindowId) -> bool {
    self.tiled_window_ids.contains(&window_id)
      || self.atom_index(window_id).is_some()
  }

  fn atom_index(&self, window_id: WindowId) -> Option<usize> {
    self.atoms.iter().position(|atom| match atom {
      StackAtom::TiledGroup { .. } => false,
      StackAtom::DetachedWindow { window_id: id } => *id == window_id,
    })
  }

  fn tile_group_index(&self) -> Option<usize> {
    self.atoms.iter().position(|atom| {
      matches!(atom, StackAtom::TiledGroup { workspace_id } if *workspace_id == self.workspace_id)
    })
  }
}

fn atom_native_rank(
  atom: &StackAtom,
  tiled_window_ids: &[WindowId],
  native_rank: &HashMap<WindowId, usize>,
) -> usize {
  match atom {
    StackAtom::TiledGroup { .. } => native_rank
      .iter()
      .filter(|(window_id, _)| tiled_window_ids.contains(window_id))
      .map(|(_, rank)| *rank)
      .min()
      .unwrap_or(usize::MAX),
    StackAtom::DetachedWindow { window_id } => {
      native_rank.get(window_id).copied().unwrap_or(usize::MAX)
    }
  }
}

fn owner_family(
  focused: WindowId,
  owner_by_window: &HashMap<WindowId, WindowId>,
) -> HashSet<WindowId> {
  let mut family = HashSet::from([focused]);
  let mut cursor = focused;
  while let Some(owner) = owner_by_window.get(&cursor).copied() {
    if !family.insert(owner) {
      break;
    }
    cursor = owner;
  }
  loop {
    let children = owner_by_window
      .iter()
      .filter_map(|(child, owner)| {
        family.contains(owner).then_some(*child)
      })
      .collect::<Vec<_>>();
    let old_len = family.len();
    family.extend(children);
    if family.len() == old_len {
      break;
    }
  }
  family
}

fn deduplicate(window_ids: &[WindowId]) -> Vec<WindowId> {
  let mut seen = HashSet::new();
  window_ids
    .iter()
    .copied()
    .filter(|id| seen.insert(*id))
    .collect()
}

fn deduplicate_atoms(atoms: &mut Vec<StackAtom>) {
  let mut seen = HashSet::new();
  atoms.retain(|atom| seen.insert(*atom));
}

#[cfg(test)]
mod tests {
  use super::*;

  fn wid(id: isize) -> WindowId {
    WindowId(id)
  }

  fn detached(stack: &NormalWindowStack) -> Vec<WindowId> {
    stack
      .atoms()
      .iter()
      .filter_map(|atom| match atom {
        StackAtom::DetachedWindow { window_id } => Some(*window_id),
        StackAtom::TiledGroup { .. } => None,
      })
      .collect()
  }

  fn workspace() -> Uuid {
    Uuid::from_u128(1)
  }

  fn seeded() -> NormalWindowStack {
    let mut stack = NormalWindowStack::new(workspace());
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: vec![wid(10), wid(11)],
      detached_window_ids: vec![wid(1), wid(2), wid(3)],
      owner_by_window: HashMap::new(),
      native_order: vec![wid(1), wid(10), wid(11), wid(2), wid(3)],
    });
    stack
  }

  #[test]
  fn insertion_and_removal_preserve_other_atoms_pairwise() {
    let mut stack = seeded();
    let original_atoms = stack.atoms().to_vec();
    let original_detached = detached(&stack);
    stack.apply(StackOperation::InsertDetached(wid(4)));
    assert_eq!(
      detached(&stack)[..original_detached.len()],
      original_detached
    );
    let after_insert = stack.atoms().to_vec();
    let original_atoms_after_insert = after_insert
      .iter()
      .filter(|atom| {
        **atom != StackAtom::DetachedWindow { window_id: wid(4) }
      })
      .copied()
      .collect::<Vec<_>>();
    assert_eq!(original_atoms_after_insert, original_atoms);
    stack.apply(StackOperation::Remove(wid(2)));
    assert_eq!(detached(&stack), vec![wid(1), wid(3), wid(4)]);
    let surviving_original_atoms = stack
      .atoms()
      .iter()
      .filter(|atom| {
        **atom != StackAtom::DetachedWindow { window_id: wid(4) }
          && **atom != StackAtom::DetachedWindow { window_id: wid(2) }
      })
      .copied()
      .collect::<Vec<_>>();
    let expected_survivors = original_atoms
      .iter()
      .filter(|atom| {
        **atom != StackAtom::DetachedWindow { window_id: wid(2) }
      })
      .copied()
      .collect::<Vec<_>>();
    assert_eq!(surviving_original_atoms, expected_survivors);
    assert!(!stack.apply(StackOperation::Remove(wid(2))));
  }

  #[test]
  fn automatic_reconcile_does_not_promote_a_tiled_fallback() {
    let mut stack = seeded();
    let before = stack.atoms().to_vec();
    assert!(!stack.apply(StackOperation::FocusRecovery));
    assert_eq!(stack.atoms(), before);
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: vec![wid(10), wid(11)],
      detached_window_ids: vec![wid(1), wid(2)],
      owner_by_window: HashMap::new(),
      native_order: vec![wid(10), wid(11), wid(1), wid(2)],
    });
    assert_eq!(stack.atoms(), &[before[0], before[1], before[2]]);
    assert_eq!(
      stack.materialize(&[wid(10), wid(11)]),
      vec![wid(1), wid(10), wid(11), wid(2)]
    );
  }

  #[test]
  fn explicit_activation_moves_only_target_atom_or_owner_family() {
    let mut stack = seeded();
    let before = detached(&stack);
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(3)),
      owner_by_window: HashMap::new(),
    });
    assert_eq!(detached(&stack), vec![wid(3), wid(1), wid(2)]);

    stack = seeded();
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::TiledGroup,
      owner_by_window: HashMap::new(),
    });
    assert!(matches!(stack.atoms()[0], StackAtom::TiledGroup { .. }));
    assert_eq!(detached(&stack), before);
  }

  #[test]
  fn owned_activation_promotes_the_smallest_managed_owner_family() {
    let mut stack = seeded();
    let owners = HashMap::from([(wid(3), wid(2)), (wid(4), wid(2))]);
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: vec![wid(10)],
      detached_window_ids: vec![wid(1), wid(2), wid(3), wid(4)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(1), wid(10), wid(3), wid(4), wid(2)],
    });
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(3)),
      owner_by_window: owners,
    });
    assert_eq!(detached(&stack), vec![wid(3), wid(4), wid(2), wid(1)]);
  }

  #[test]
  fn detached_dialog_owned_by_a_tile_preserves_the_tile_atom_constraint() {
    let mut stack = NormalWindowStack::new(workspace());
    let tiles = [wid(10), wid(11)];
    let owners = HashMap::from([(wid(3), wid(10))]);
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2), wid(3)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(1), wid(3), wid(10), wid(11), wid(2)],
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(1), wid(3), wid(10), wid(11), wid(2)],
      "the owned dialog must stay above its tiled owner"
    );

    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(3)),
      owner_by_window: owners.clone(),
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(3), wid(10), wid(11), wid(1), wid(2)]
    );
    assert_eq!(
      stack.activation_group(wid(3), &tiles),
      vec![wid(3), wid(10), wid(11)]
    );

    stack.apply(StackOperation::Activate {
      atom: StackAtomId::TiledGroup,
      owner_by_window: owners,
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(3), wid(10), wid(11), wid(1), wid(2)],
      "activating an owner promotes its complete tile atom and required owned child"
    );
    assert_eq!(
      stack.activation_group(wid(10), &tiles),
      vec![wid(3), wid(10), wid(11)]
    );
  }

  #[test]
  fn tiled_activation_promotes_owned_popups_with_the_tile_atom() {
    let mut stack = NormalWindowStack::new(workspace());
    let tiles = [wid(10), wid(11)];
    let owners = HashMap::from([(wid(3), wid(10))]);
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2), wid(3)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(1), wid(3), wid(10), wid(11), wid(2)],
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(1), wid(3), wid(10), wid(11), wid(2)]
    );

    stack.apply(StackOperation::Activate {
      atom: StackAtomId::TiledGroup,
      owner_by_window: owners,
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(3), wid(10), wid(11), wid(1), wid(2)],
      "the owned popup follows its owner atom; unrelated detached windows retain their order"
    );
    assert_eq!(
      stack.activation_group(wid(10), &tiles),
      vec![wid(3), wid(10), wid(11)]
    );
  }

  #[test]
  fn reclassification_keeps_the_affected_atom_slot() {
    let mut stack = seeded();
    stack.apply(StackOperation::Reclassify {
      window_id: wid(2),
      target: ReclassifyTarget::Tiled,
    });
    assert_eq!(detached(&stack), vec![wid(1), wid(3)]);
    assert_eq!(stack.atoms().len(), 3);
    assert_eq!(
      stack.materialize(&[wid(10), wid(11), wid(2)]),
      vec![wid(1), wid(10), wid(11), wid(2), wid(3)]
    );

    stack.apply(StackOperation::Reclassify {
      window_id: wid(11),
      target: ReclassifyTarget::Detached,
    });
    assert_eq!(
      stack.materialize(&[wid(10), wid(2)]),
      vec![wid(1), wid(10), wid(2), wid(11), wid(3)]
    );

    stack.apply(StackOperation::Reclassify {
      window_id: wid(10),
      target: ReclassifyTarget::Detached,
    });
    stack.apply(StackOperation::Reclassify {
      window_id: wid(2),
      target: ReclassifyTarget::Detached,
    });
    assert_eq!(
      stack.materialize(&[]),
      vec![wid(1), wid(2), wid(10), wid(11), wid(3)]
    );
  }

  #[test]
  fn reconciliation_is_idempotent_and_repairs_stale_membership() {
    let mut stack = seeded();
    let op = StackOperation::Reconcile {
      tiled_window_ids: vec![wid(10), wid(11), wid(10)],
      detached_window_ids: vec![wid(1), wid(3), wid(3), wid(9)],
      owner_by_window: HashMap::from([(wid(9), wid(88))]),
      native_order: vec![wid(9), wid(1), wid(10), wid(11), wid(3)],
    };
    stack.apply(op.clone());
    let first = stack.atoms().to_vec();
    assert!(!stack.apply(op));
    assert_eq!(stack.atoms(), first);
    assert_eq!(stack.owner_by_window.len(), 0);
    assert_eq!(stack.materialize(&[wid(10), wid(11)]).len(), 5);
  }

  #[test]
  fn nested_ownerless_dialog_close_keeps_independent_atoms_and_tile_slot()
  {
    let mut stack = NormalWindowStack::new(workspace());
    let tiles = [wid(10), wid(11)];
    let mut owners = HashMap::new();
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(1), wid(2), wid(10), wid(11)],
    });

    // An ownerless dialog opens above Settings; an independently opened
    // detached app keeps its slot below Settings.
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2), wid(3)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(3), wid(1), wid(2), wid(10), wid(11)],
    });
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(3)),
      owner_by_window: owners.clone(),
    });

    // A nested owned dialog is a separate atom constrained above its
    // verified owner.
    owners.insert(wid(4), wid(3));
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2), wid(3), wid(4)],
      owner_by_window: owners.clone(),
      native_order: vec![wid(4), wid(3), wid(1), wid(2), wid(10), wid(11)],
    });
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(4)),
      owner_by_window: owners.clone(),
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(4), wid(3), wid(1), wid(2), wid(10), wid(11)]
    );

    owners.remove(&wid(4));
    stack.apply(StackOperation::Remove(wid(4)));
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2), wid(3)],
      owner_by_window: owners.clone(),
      // Native focus recovery temporarily raises a tile. Reconciliation
      // observes it but must preserve the saved logical order.
      native_order: vec![wid(10), wid(11), wid(3), wid(1), wid(2)],
    });
    stack.apply(StackOperation::Remove(wid(3)));
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: tiles.to_vec(),
      detached_window_ids: vec![wid(1), wid(2)],
      owner_by_window: owners,
      native_order: vec![wid(10), wid(11), wid(1), wid(2)],
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(1), wid(2), wid(10), wid(11)]
    );

    stack.apply(StackOperation::Activate {
      atom: StackAtomId::TiledGroup,
      owner_by_window: HashMap::new(),
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(10), wid(11), wid(1), wid(2)]
    );
    stack.apply(StackOperation::Activate {
      atom: StackAtomId::DetachedWindow(wid(1)),
      owner_by_window: HashMap::new(),
    });
    assert_eq!(
      stack.materialize(&tiles),
      vec![wid(1), wid(10), wid(11), wid(2)]
    );
  }

  #[test]
  fn new_atoms_use_observed_slot_without_reordering_existing_atoms() {
    let mut stack = seeded();
    stack.apply(StackOperation::Reconcile {
      tiled_window_ids: vec![wid(10), wid(11)],
      detached_window_ids: vec![wid(1), wid(2), wid(3), wid(4)],
      owner_by_window: HashMap::new(),
      native_order: vec![wid(1), wid(4), wid(10), wid(11), wid(2), wid(3)],
    });
    assert_eq!(detached(&stack), vec![wid(1), wid(4), wid(2), wid(3)]);
  }

  #[test]
  #[allow(clippy::too_many_lines)] // Exercises seeded state-machine
                                   // transitions in one reproducible
                                   // trace.
  fn randomized_lifecycle_sequences_preserve_model_invariants() {
    // Deterministic state-machine coverage without another workspace
    // dependency. A failing sequence is reproducible from its seed.
    for seed in 1..=2_000_u64 {
      let mut rng = seed;
      let mut stack = NormalWindowStack::new(workspace());
      let mut live_detached = HashSet::new();
      let mut live_tiled = vec![wid(10), wid(11)];
      let mut owners = HashMap::new();
      for _ in 0..50 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let candidate = wid(
          isize::try_from((rng >> 16) % 8 + 1)
            .expect("generated test window id fits isize"),
        );
        match rng % 8 {
          0 => {
            stack.apply(StackOperation::InsertDetached(candidate));
            if !live_tiled.contains(&candidate) {
              live_detached.insert(candidate);
            }
          }
          1 => {
            stack.apply(StackOperation::Remove(candidate));
            live_detached.remove(&candidate);
            live_tiled.retain(|window_id| *window_id != candidate);
            owners.retain(|child, owner| {
              *child != candidate && *owner != candidate
            });
          }
          2 => {
            let owner = wid(
              isize::try_from((rng >> 24) % 8 + 1)
                .expect("generated test owner id fits isize"),
            );
            if candidate != owner
              && !owner_family(owner, &owners).contains(&candidate)
            {
              owners.insert(candidate, owner);
            }
          }
          3 => {
            let order = stack.materialize(&live_tiled);
            if live_detached.contains(&candidate) {
              stack.apply(StackOperation::Activate {
                atom: StackAtomId::DetachedWindow(candidate),
                owner_by_window: owners.clone(),
              });
            } else {
              stack.apply(StackOperation::Activate {
                atom: StackAtomId::TiledGroup,
                owner_by_window: owners.clone(),
              });
            }
            assert_eq!(
              order.len(),
              stack.materialize(&live_tiled).len(),
              "seed={seed}"
            );
          }
          4 => {
            if let Some(window_id) = live_tiled.pop() {
              live_detached.insert(window_id);
              stack.apply(StackOperation::Reclassify {
                window_id,
                target: ReclassifyTarget::Detached,
              });
            } else {
              let window_id =
                live_detached.iter().next().copied().unwrap_or(candidate);
              live_detached.remove(&window_id);
              stack.apply(StackOperation::Reclassify {
                window_id,
                target: ReclassifyTarget::Tiled,
              });
              live_tiled.push(window_id);
            }
          }
          5 => {
            // Duplicate create/destroy notifications are idempotent.
            if live_detached.contains(&candidate)
              || live_tiled.contains(&candidate)
            {
              stack.apply(StackOperation::Remove(candidate));
              stack.apply(StackOperation::Remove(candidate));
              live_detached.remove(&candidate);
              live_tiled.retain(|window_id| *window_id != candidate);
              owners.retain(|child, owner| {
                *child != candidate && *owner != candidate
              });
            } else {
              stack.apply(StackOperation::InsertDetached(candidate));
              stack.apply(StackOperation::InsertDetached(candidate));
              live_detached.insert(candidate);
            }
          }
          6 => {
            let before = stack.atoms().to_vec();
            stack.apply(StackOperation::FocusRecovery);
            assert_eq!(stack.atoms(), before, "seed={seed}");
          }
          _ => {}
        }

        let native_order = stack.materialize(&live_tiled);
        stack.apply(StackOperation::Reconcile {
          tiled_window_ids: live_tiled.clone(),
          detached_window_ids: live_detached.iter().copied().collect(),
          owner_by_window: owners.clone(),
          native_order,
        });
        let atoms = stack.atoms();
        assert!(
          atoms
            .iter()
            .filter(|atom| matches!(atom, StackAtom::TiledGroup { .. }))
            .count()
            <= 1,
          "seed={seed}"
        );
        let materialized = stack.materialize(&live_tiled);
        let unique: HashSet<_> = materialized.iter().copied().collect();
        assert_eq!(unique.len(), materialized.len(), "seed={seed}");
        assert_eq!(
          unique.len(),
          live_tiled.len() + live_detached.len(),
          "seed={seed}"
        );
        let position: HashMap<WindowId, usize> = materialized
          .iter()
          .enumerate()
          .map(|(index, window_id)| (*window_id, index))
          .collect();
        for (child, owner) in &stack.owner_by_window {
          assert!(
            position[child] < position[owner],
            "owned child must stay above its owner; seed={seed}, child={child:?}, owner={owner:?}"
          );
        }
        let tile_positions = materialized
          .iter()
          .enumerate()
          .filter_map(|(index, id)| {
            live_tiled.contains(id).then_some(index)
          })
          .collect::<Vec<_>>();
        if !tile_positions.is_empty() {
          assert_eq!(
            tile_positions.last().unwrap()
              - tile_positions.first().unwrap()
              + 1,
            live_tiled.len(),
            "seed={seed}"
          );
        }
        let before = stack.atoms().to_vec();
        stack.apply(StackOperation::Reconcile {
          tiled_window_ids: live_tiled.clone(),
          detached_window_ids: live_detached.iter().copied().collect(),
          owner_by_window: owners.clone(),
          native_order: materialized,
        });
        assert_eq!(stack.atoms(), before, "seed={seed}");
      }
    }
  }
}
