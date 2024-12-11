// © 2023, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use rustc_interface::dataflow::JoinSemiLattice;

use crate::{
    free_pcs::{
        CapabilityKind, CapabilityLocal, CapabilityProjections, CapabilitySummary,
        FreePlaceCapabilitySummary,
    },
    rustc_interface,
    utils::{PlaceOrdering, PlaceRepacker},
};

impl JoinSemiLattice for FreePlaceCapabilitySummary<'_, '_> {
    fn join(&mut self, other: &Self) -> bool {
        self.post_main.join(&other.post_main, self.repacker)
    }
}

pub trait RepackingJoinSemiLattice<'tcx> {
    fn join(&mut self, other: &Self, repacker: PlaceRepacker<'_, 'tcx>) -> bool;
}
impl<'tcx> RepackingJoinSemiLattice<'tcx> for CapabilitySummary<'tcx> {
    fn join(&mut self, other: &Self, repacker: PlaceRepacker<'_, 'tcx>) -> bool {
        let mut changed = false;
        for (l, to) in self.iter_enumerated_mut() {
            let local_changed = to.join(&other[l], repacker);
            changed = changed || local_changed;
        }
        changed
    }
}

impl<'tcx> RepackingJoinSemiLattice<'tcx> for CapabilityLocal<'tcx> {
    fn join(&mut self, other: &Self, repacker: PlaceRepacker<'_, 'tcx>) -> bool {
        match (&mut *self, other) {
            (CapabilityLocal::Unallocated, CapabilityLocal::Unallocated) => false,
            (CapabilityLocal::Allocated(to_places), CapabilityLocal::Allocated(from_places)) => {
                to_places.join(from_places, repacker)
            }
            (CapabilityLocal::Allocated(..), CapabilityLocal::Unallocated) => {
                *self = CapabilityLocal::Unallocated;
                true
            }
            // Can jump to a `is_cleanup` block with some paths being alloc and other not
            (CapabilityLocal::Unallocated, CapabilityLocal::Allocated(..)) => false,
        }
    }
}

impl<'tcx> RepackingJoinSemiLattice<'tcx> for CapabilityProjections<'tcx> {
    fn join(&mut self, other: &Self, repacker: PlaceRepacker<'_, 'tcx>) -> bool {
        if self.is_empty() {
            // Handle the bottom case
            *self = other.clone();
            return true;
        }
        let mut changed = false;
        for (&place, &kind) in &**other {
            let related = self.find_all_related(place, None);
            for (from_place, _) in (*related).iter().copied() {
                let final_place = match from_place.partial_cmp(place).unwrap() {
                    PlaceOrdering::Prefix => {
                        let from = related.get_only_place();
                        let joinable_place = if self[&from] != CapabilityKind::Exclusive {
                            // One cannot expand a `Write` or a `ShallowInit` capability
                            from
                        } else {
                            from.joinable_to(place)
                        };
                        assert!(from.is_prefix(joinable_place));
                        if joinable_place != from {
                            changed = true;
                            self.expand(from, joinable_place, repacker);
                        }
                        Some(joinable_place)
                    }
                    PlaceOrdering::Equal => Some(place),
                    PlaceOrdering::Suffix => {
                        // Downgrade the permission if needed
                        for &(p, k) in &*related {
                            // Might not contain key if `p.projects_ptr(repacker)`
                            // returned `Some` in a previous iteration.
                            if !self.contains_key(&p) {
                                continue;
                            }
                            let collapse_to = if kind != CapabilityKind::Exclusive {
                                place
                            } else {
                                place.joinable_to(p)
                            };
                            if collapse_to != p {
                                changed = true;
                                let mut from = related.get_places();
                                from.retain(|&from| collapse_to.is_prefix(from));
                                self.collapse(from, collapse_to, repacker);
                            }
                            if k > kind {
                                changed = true;
                                self.update_cap(collapse_to, kind);
                            }
                        }
                        None
                    }
                    PlaceOrdering::Both => {
                        changed = true;

                        let cp = related.common_prefix(place);
                        // todo!("Collapse {place:?} to {:?}", cp);
                        self.collapse(related.get_places(), cp, repacker);
                        Some(cp)
                    }
                };
                if let Some(place) = final_place {
                    // Downgrade the permission if needed
                    if self[&place] > kind {
                        changed = true;
                        self.update_cap(place, kind);
                    }
                }
            }
        }
        changed
    }
}
