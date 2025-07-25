// © 2023, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![feature(rustc_private)]
#![feature(box_patterns)]
#![feature(if_let_guard, let_chains)]
#![feature(never_type)]
#![feature(proc_macro_hygiene)]
#![feature(anonymous_lifetime_in_impl_trait)]
#![feature(stmt_expr_attributes)]
#![feature(allocator_api)]
pub mod action;
pub mod borrow_checker;
pub mod borrow_pcg;
pub mod coupling;
pub mod free_pcs;
pub mod r#loop;
pub mod pcg;
pub mod rustc_interface;
pub mod utils;
pub mod visualization;

use action::PcgActions;
use borrow_checker::BorrowCheckerInterface;
use borrow_pcg::{graph::borrows_imgcat_debug, latest::Latest};
use free_pcs::{CapabilityKind, PcgLocation};
use pcg::{EvalStmtPhase, PcgEngine, PcgSuccessor};
use rustc_interface::{
    borrowck::{self, BorrowSet, LocationTable, PoloniusInput, RegionInferenceContext},
    dataflow::{compute_fixpoint, AnalysisEngine},
    middle::{mir::Body, ty::TyCtxt},
};
use serde_json::json;
use utils::{
    display::{DebugLines, DisplayWithCompilerCtxt},
    validity::HasValidityCheck,
    CompilerCtxt, Place, VALIDITY_CHECKS, VALIDITY_CHECKS_WARN_ONLY,
};
use visualization::mir_graph::generate_json_from_mir;

use utils::json::ToJsonWithCompilerCtxt;

pub type PcgOutput<'mir, 'tcx, A> = free_pcs::PcgAnalysis<'mir, 'tcx, A>;
/// Instructs that the current capability to the place (first [`CapabilityKind`]) should
/// be weakened to the second given capability. We guarantee that `_.1 > _.2`.
/// If `_.2` is `None`, the capability is removed.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Weaken<'tcx> {
    pub(crate) place: Place<'tcx>,
    pub(crate) from: CapabilityKind,
    pub(crate) to: Option<CapabilityKind>,
}

impl<'tcx> Weaken<'tcx> {
    pub(crate) fn debug_line<BC: Copy>(&self, ctxt: CompilerCtxt<'_, 'tcx, BC>) -> String {
        let to_str = match self.to {
            Some(to) => format!("{to:?}"),
            None => "None".to_string(),
        };
        format!(
            "Weaken {} from {:?} to {}",
            self.place.to_short_string(ctxt),
            self.from,
            to_str
        )
    }

    pub(crate) fn new(
        place: Place<'tcx>,
        from: CapabilityKind,
        to: Option<CapabilityKind>,
    ) -> Self {
        // TODO
        // if let Some(to) = to {
        //     pcg_validity_assert!(
        //         from > to,
        //         "FROM capability ({:?}) is not greater than TO capability ({:?})",
        //         from,
        //         to
        //     );
        // }
        Self { place, from, to }
    }

    pub fn place(&self) -> Place<'tcx> {
        self.place
    }

    pub fn from_cap(&self) -> CapabilityKind {
        self.from
    }

    pub fn to_cap(&self) -> Option<CapabilityKind> {
        self.to
    }
}

/// Instructs that the current capability to the place should be restored to the given capability, e.g.
/// a lent exclusive capability should be restored to an exclusive capability.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RestoreCapability<'tcx> {
    place: Place<'tcx>,
    capability: CapabilityKind,
}

impl<'tcx, BC: Copy> ToJsonWithCompilerCtxt<'tcx, BC> for RestoreCapability<'tcx> {
    fn to_json(&self, ctxt: CompilerCtxt<'_, 'tcx, BC>) -> serde_json::Value {
        json!({
            "place": self.place.to_json(ctxt),
            "capability": format!("{:?}", self.capability),
        })
    }
}
impl<'tcx> RestoreCapability<'tcx> {
    pub(crate) fn debug_line<BC: Copy>(&self, repacker: CompilerCtxt<'_, 'tcx, BC>) -> String {
        format!(
            "Restore {} to {:?}",
            self.place.to_short_string(repacker),
            self.capability,
        )
    }

    pub(crate) fn new(place: Place<'tcx>, capability: CapabilityKind) -> Self {
        Self { place, capability }
    }

    pub fn place(&self) -> Place<'tcx> {
        self.place
    }

    pub fn capability(&self) -> CapabilityKind {
        self.capability
    }
}

impl<'tcx, BC: Copy> ToJsonWithCompilerCtxt<'tcx, BC> for Weaken<'tcx> {
    fn to_json(&self, repacker: CompilerCtxt<'_, 'tcx, BC>) -> serde_json::Value {
        json!({
            "place": self.place.to_json(repacker),
            "old": format!("{:?}", self.from),
            "new": format!("{:?}", self.to),
        })
    }
}

impl<'tcx> DebugLines<CompilerCtxt<'_, 'tcx>> for BorrowPcgActions<'tcx> {
    fn debug_lines(&self, repacker: CompilerCtxt<'_, 'tcx>) -> Vec<String> {
        self.0
            .iter()
            .map(|action| action.debug_line(repacker))
            .collect()
    }
}

use borrow_pcg::action::actions::BorrowPcgActions;
use std::{alloc::Allocator, sync::Mutex};
use utils::eval_stmt_data::EvalStmtData;

lazy_static::lazy_static! {
    /// Whether to record PCG information for each block. This is used for
    /// debugging only. This is set to true when the PCG is initially
    /// constructed, and then disabled after its construction. The reason for
    /// using a global variable is that debugging information is written during
    /// the dataflow operations of the PCG, which are also used when examining
    /// PCG results. We don't want to write the debugging information to disk
    /// during examination, of course.
    static ref RECORD_PCG: Mutex<bool> = Mutex::new(false);
}

struct PCGStmtVisualizationData<'a, 'tcx> {
    /// The value of the "latest" map at the end of the statement.
    latest: &'a Latest<'tcx>,
    actions: &'a EvalStmtData<PcgActions<'tcx>>,
}

struct PcgSuccessorVisualizationData<'a, 'tcx> {
    actions: &'a PcgActions<'tcx>,
}

impl<'tcx, 'a> From<&'a PcgSuccessor<'tcx>> for PcgSuccessorVisualizationData<'a, 'tcx> {
    fn from(successor: &'a PcgSuccessor<'tcx>) -> Self {
        Self {
            actions: &successor.actions,
        }
    }
}

impl<'tcx, 'a> ToJsonWithCompilerCtxt<'tcx, &'a dyn BorrowCheckerInterface<'tcx>> for PcgSuccessorVisualizationData<'a, 'tcx> {
    fn to_json(&self, repacker: CompilerCtxt<'_, 'tcx>) -> serde_json::Value {
        json!({
            "actions": self.actions.iter().map(|a| a.to_json(repacker)).collect::<Vec<_>>(),
        })
    }
}

impl<'tcx, 'a> ToJsonWithCompilerCtxt<'tcx, &'a dyn BorrowCheckerInterface<'tcx>> for PCGStmtVisualizationData<'a, 'tcx> {
    fn to_json(&self, repacker: CompilerCtxt<'_, 'tcx, &'a dyn BorrowCheckerInterface<'tcx>>) -> serde_json::Value {
        json!({
            "latest": self.latest.to_json(repacker),
            "actions": self.actions.to_json(repacker),
        })
    }
}

impl<'a, 'tcx> PCGStmtVisualizationData<'a, 'tcx> {
    fn new<'mir>(location: &'a PcgLocation<'tcx>) -> Self
    where
        'tcx: 'mir,
    {
        Self {
            latest: &location.states[EvalStmtPhase::PostMain].borrow.latest,
            actions: &location.actions,
        }
    }
}

pub trait BodyAndBorrows<'tcx> {
    fn body(&self) -> &Body<'tcx>;
    fn borrow_set(&self) -> &BorrowSet<'tcx>;
    fn region_inference_context(&self) -> &RegionInferenceContext<'tcx>;
    fn location_table(&self) -> &LocationTable;
    fn input_facts(&self) -> &PoloniusInput;
}

impl<'tcx> BodyAndBorrows<'tcx> for borrowck::BodyWithBorrowckFacts<'tcx> {
    fn body(&self) -> &Body<'tcx> {
        &self.body
    }
    fn borrow_set(&self) -> &BorrowSet<'tcx> {
        &self.borrow_set
    }
    fn region_inference_context(&self) -> &RegionInferenceContext<'tcx> {
        &self.region_inference_context
    }

    fn location_table(&self) -> &LocationTable {
        self.location_table.as_ref().unwrap()
    }

    fn input_facts(&self) -> &PoloniusInput {
        self.input_facts.as_ref().unwrap()
    }
}

pub fn run_pcg<
    'a,
    'tcx: 'a,
    A: Allocator + Copy + std::fmt::Debug,
    BC: BorrowCheckerInterface<'tcx> + ?Sized,
>(
    body: &'a Body<'tcx>,
    tcx: TyCtxt<'tcx>,
    bc: &'a BC,
    arena: A,
    visualization_output_path: Option<&str>,
) -> PcgOutput<'a, 'tcx, A> {
    let ctxt: CompilerCtxt<'a, 'tcx> = CompilerCtxt::new(body, tcx, bc.as_dyn());
    let engine = PcgEngine::new(ctxt, arena, visualization_output_path);
    {
        let mut record_pcg = RECORD_PCG.lock().unwrap();
        *record_pcg = true;
    }
    let analysis = compute_fixpoint(AnalysisEngine(engine), tcx, body);
    {
        let mut record_pcg = RECORD_PCG.lock().unwrap();
        *record_pcg = false;
    }
    if let Some(dir_path) = &visualization_output_path {
        for block in body.basic_blocks.indices() {
            let state = analysis.entry_set_for_block(block);
            assert!(state.block() == block);
            let block_iterations_json_file =
                format!("{}/block_{}_iterations.json", dir_path, block.index());
            state
                .dot_graphs()
                .unwrap()
                .borrow()
                .write_json_file(&block_iterations_json_file);
        }
    }
    let mut fpcs_analysis = free_pcs::PcgAnalysis::new(analysis.into_results_cursor(body));

    if let Some(dir_path) = visualization_output_path {
        let edge_legend_file_path = format!("{dir_path}/edge_legend.dot");
        let edge_legend_graph = crate::visualization::legend::generate_edge_legend().unwrap();
        std::fs::write(&edge_legend_file_path, edge_legend_graph)
            .expect("Failed to write edge legend");

        let node_legend_file_path = format!("{dir_path}/node_legend.dot");
        let node_legend_graph = crate::visualization::legend::generate_node_legend().unwrap();
        std::fs::write(&node_legend_file_path, node_legend_graph)
            .expect("Failed to write node legend");
        generate_json_from_mir(&format!("{dir_path}/mir.json"), ctxt)
            .expect("Failed to generate JSON from MIR");

        // Iterate over each statement in the MIR
        for (block, _data) in body.basic_blocks.iter_enumerated() {
            let pcs_block_option = if let Ok(opt) = fpcs_analysis.get_all_for_bb(block) {
                opt
            } else {
                continue;
            };
            if pcs_block_option.is_none() {
                continue;
            }
            let pcs_block = pcs_block_option.unwrap();
            for (statement_index, statement) in pcs_block.statements.iter().enumerate() {
                if validity_checks_enabled() {
                    statement.assert_validity(ctxt);
                }
                let data = PCGStmtVisualizationData::new(statement);
                let pcg_data_file_path = format!(
                    "{}/block_{}_stmt_{}_pcg_data.json",
                    &dir_path,
                    block.index(),
                    statement_index
                );
                let pcg_data_json = data.to_json(ctxt);
                std::fs::write(&pcg_data_file_path, pcg_data_json.to_string())
                    .expect("Failed to write pcg data to JSON file");
            }
            for succ in pcs_block.terminator.succs {
                let data = PcgSuccessorVisualizationData::from(&succ);
                let pcg_data_file_path = format!(
                    "{}/block_{}_term_block_{}_pcg_data.json",
                    &dir_path,
                    block.index(),
                    succ.block().index()
                );
                let pcg_data_json = data.to_json(ctxt);
                std::fs::write(&pcg_data_file_path, pcg_data_json.to_string())
                    .expect("Failed to write pcg data to JSON file");
            }
        }
    }

    fpcs_analysis
}

#[macro_export]
macro_rules! pcg_validity_assert {
    ($cond:expr) => {
        if $crate::validity_checks_enabled() {
            if $crate::validity_checks_warn_only() {
                #[allow(clippy::neg_cmp_op_on_partial_ord)]
                if !$cond {
                    tracing::error!("assertion failed: {}", stringify!($cond));
                }
            } else {
                if !$cond {
                    tracing::error!("assertion failed: {}", stringify!($cond));
                }
                assert!($cond);
            }
        }
    };
    ($cond:expr, $($arg:tt)*) => {
        if $crate::validity_checks_enabled() {
            if $crate::validity_checks_warn_only() {
                #[allow(clippy::neg_cmp_op_on_partial_ord)]
                if !$cond {
                    tracing::error!($($arg)*);
                }
            } else {
                if !$cond {
                    tracing::error!($($arg)*);
                }
                assert!($cond, $($arg)*);
            }
        }
    };
}

#[macro_export]
macro_rules! pcg_validity_warn {
    ($cond:expr, $($arg:tt)*) => {
        if $crate::validity_checks_enabled() {
            if !$cond {
                tracing::warn!($($arg)*);
            }
        }
    };
}

pub(crate) fn validity_checks_enabled() -> bool {
    *VALIDITY_CHECKS
}

pub(crate) fn validity_checks_warn_only() -> bool {
    *VALIDITY_CHECKS_WARN_ONLY
}
