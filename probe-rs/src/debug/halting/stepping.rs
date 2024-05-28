use super::{
    super::{debug_info::DebugInfo, DebugError},
    instruction::Instruction,
    line_sequence_for_address,
    sequence::Sequence,
    VerifiedBreakpoint,
};
use crate::{
    architecture::{
        arm::ArmError, riscv::communication_interface::RiscvError,
        xtensa::communication_interface::XtensaError,
    },
    core::ExceptionInfo,
    debug::{ColumnType, DebugRegisters},
    exception_handler_for_core, CoreInterface, CoreStatus, HaltReason,
};
use probe_rs_target::InstructionSet;
use std::{ops::ControlFlow, time::Duration};

/// Implement the various stepping actions available during debugging.
/// The stepping actions are based on available requests and step granularities
/// defined in the [DAP protocol](https://microsoft.github.io/debug-adapter-protocol/specification#Types_SteppingGranularity).
/// Currently we have support for 'instruction' level stepping, as well
/// as 'step-over' 'step-into', and 'step-out' at 'statement' level.
/// Note: Because of the way the DWARF debug information is currently generated by the Rust compiler,
/// (no explicity 'basic_block' information is generated), the the 'step over' functionality
/// at 'statement' level will be rare, and most often, we will have to fall back to
/// behave more like 'step over' at 'line' level.
#[derive(Clone, Debug)]
pub enum Stepping {
    /// Advance one machine instruction at a time.
    StepInstruction,
    /// Step Over the current statement, and halt at the start of the next statement.
    OverStatement,
    /// Use best efforts to determine the location of any function calls in this statement, and step into them.
    IntoStatement,
    /// Step to the calling statement, immediately after the current function returns.
    OutOfStatement,
}

impl Stepping {
    /// Determine the program counter location where the SteppingMode is aimed, and step to it.
    /// Return the new CoreStatus and program_counter value.
    /// ### Implementation Notes for stepping multiple instructions at a time:
    /// - This implementation builds on the [VerifiedBreakpoint] implementation, with additional heurstics
    ///   available based on the halted target's stackframe information, to determine suitable target locations
    ///   for stepping.
    /// - If a hardware breakpoint is available, we will set it at the desired location, run to it, and release it.
    /// - If no hardware breakpoints are available, we will do repeated instruction steps until we reach the desired location.
    /// ### Usage Note:
    /// - Currently, no special provision is made for the effect of interrupts that get triggered
    ///   during stepping. The user must ensure that interrupts are disabled during stepping, or
    ///   accept that stepping may be diverted by the interrupt processing on the core.
    pub fn step(
        &self,
        core: &mut impl CoreInterface,
        debug_info: &DebugInfo,
    ) -> Result<(CoreStatus, u64), DebugError> {
        let mut core_status = core
            .status()
            .map_err(|error| DebugError::Other(anyhow::anyhow!(error)))?;
        let mut program_counter = match core_status {
            CoreStatus::Halted(_) => core
                .read_core_reg(core.program_counter().id())?
                .try_into()?,
            _ => {
                return Err(DebugError::Other(anyhow::anyhow!(
                    "Core must be halted before stepping."
                )))
            }
        };
        let origin_program_counter = program_counter;
        let target_breakpoint = match self {
            Stepping::StepInstruction => {
                // First deal with the the fast/easy case.
                program_counter = core.step()?.pc;
                core_status = core.status()?;
                return Ok((core_status, program_counter));
            }
            Stepping::IntoStatement => get_step_into_location(debug_info, core),
            Stepping::OutOfStatement => get_step_out_location(debug_info, core, program_counter),
            Stepping::OverStatement => get_step_over_location(debug_info, core, program_counter),
        }
        .map_err(|step_error| {
            tracing::warn!("Error during step ({:?}): {}", self, &step_error);
            step_error
        })?;
        tracing::debug!(
            "Step ({:20?}):\n\tFrom: {:?}\n\t  To: {:?}\n\t@ {:#010X}",
            self,
            debug_info
                .get_source_location(origin_program_counter)
                .unwrap_or_default(),
            target_breakpoint.source_location,
            target_breakpoint.address
        );

        run_to_address(target_breakpoint.address, core, debug_info)
    }
}

/// Stepping into a line is a bit tricky case because the current RUST generated DWARF,
/// does not store the DW_TAG_call_site information described in the DWARF 5 standard.
/// It is not a mandatory attribute, so it is not clear if we can ever expect it.
/// #### To find if any functions are called from the current program counter:
/// -  We single step the target core, until:
///    - We are on a new line in the same sequence (we can get the next haltpoint), or
///    - We are in a new sequence. This means we have stepped into a non-inlined function call.
///      Inlined function call instructions would already have processed by the target,
///      conceptually it is impossible to 'step into' them.
fn get_step_into_location(
    debug_info: &DebugInfo,
    core: &mut impl CoreInterface,
) -> Result<VerifiedBreakpoint, DebugError> {
    while let Ok(core_information) = &core.step() {
        let new_sequence = Sequence::from_address(debug_info, core_information.pc)?;

        // Once we have reached a new valid haltpoint, we are either at the start of a non-inlined function,
        // or on a new line in the same sequence (stepped over, because there was nothing to step into).
        if let Some(new_halt_location) = new_sequence.haltpoint_for_address(core_information.pc) {
            return VerifiedBreakpoint::for_address(debug_info, new_halt_location.address);
        }

        if let ControlFlow::Break(debug_error) = validate_core_status_after_step(core, debug_info) {
            return Err(debug_error);
        }
    }
    let message = "Could not step into the current statement.".to_string();
    Err(DebugError::WarnAndContinue { message })
}

/// Step out of the current function, and halt at the first available location after the return address.
/// For inlined functions, this is the first available breakpoint address after the last statement in the inline function.
/// For non-inlined functions, this is the first available breakpoint address after the return address.
fn get_step_out_location(
    debug_info: &DebugInfo,
    core: &mut impl CoreInterface,
    program_counter: u64,
) -> Result<VerifiedBreakpoint, DebugError> {
    // Get the function DIE for the current program counter, and there are inlined functions,
    // use the innermost of those.
    let program_unit = debug_info.compile_unit_info(program_counter)?;
    let function = program_unit
        .get_function_dies(debug_info, program_counter)?
        .pop()
        .ok_or(DebugError::WarnAndContinue {
            // Without a valid function DIE, we don't have enough information to proceed intelligently.
            message: format!("Unable to identify the function at {program_counter:#010x}"),
        })?;
    tracing::trace!(
        "Step Out target: Evaluating {}function {:?}, low_pc={:?}, high_pc={:?}",
        if function.is_inline() { "inlined " } else { "" },
        function.function_name(debug_info),
        function.low_pc(),
        function.high_pc()
    );

    if function
        .attribute(debug_info, gimli::DW_AT_noreturn)
        .is_some()
    {
        let message = format!(
            "Function {:?} is marked as `noreturn`. Cannot step out of this function.",
            function
                .function_name(debug_info)
                .as_deref()
                .unwrap_or("<unknown>")
        );
        return Err(DebugError::WarnAndContinue { message });
    }

    if function.is_inline() {
        function
            .inline_call_location(debug_info)
            .and_then(|call_site| {
                // Step_out_address for inlined functions, is the first available breakpoint address for the call site.
                // This has been tested to work with nested inline functions also.
                tracing::debug!(
                    "Step Out target: inline function, stepping over call-site: {call_site:?}"
                );
                call_site.combined_typed_path().as_ref().map(|path| {
                    VerifiedBreakpoint::for_source_location(
                        debug_info,
                        path,
                        call_site.line.unwrap_or(0),
                        call_site.column.map(|column| match column {
                            ColumnType::LeftEdge => 0_u64,
                            ColumnType::Column(c) => c,
                        }),
                    )
                })
            })
            .unwrap_or_else(|| {
                // This should never happen, but if it does, it is probably more useful to just step over
                // the current statement, than to give an error.
                tracing::warn!(
                    "Unable to identify the call-site for the inlined function {:?}",
                    function.function_name(debug_info)
                );
                get_step_over_location(debug_info, core, program_counter)
            })
    } else {
        let return_address = get_return_address(core)?;
        tracing::debug!(
            "Step Out target: non-inline function, stepping over return address: {return_address:#010x}"
        );
        // Step-out address for non-inlined functions is the first available breakpoint address after the return address.
        if let Ok(target_location_at_return) =
            VerifiedBreakpoint::for_address(debug_info, return_address)
        {
            Ok(target_location_at_return)
        } else {
            // It is possible that the return address is not a valid halt location,
            // in which case we have to find the next valid halt_location,
            // in the calling function.
            // In this case, we have to do the following:
            // 1. Run (set a breakpoint) to the last valid halt location in the current sequence.
            // 2. Single-step the core until we get to the next valid halt location.

            // Find the last valid halt location in the current sequence.
            tracing::debug!("Looking for last halt instruction in the sequence containing address={program_counter:#010x}");

            let sequence = Sequence::from_address(debug_info, program_counter)?;

            let Some(last_sequence_haltpoint) = sequence.last_halt_instruction else {
                let message = format!("No valid halt location found in the sequence for the return address: {return_address:#010x}.");
                return Err(DebugError::WarnAndContinue { message });
            };

            // Run to the last valid halt location in the current sequence.
            run_to_address(last_sequence_haltpoint, core, debug_info)?;
            // Now single-step until we find a valid halt location.
            while let Ok(step_result) = core.step() {
                if let ControlFlow::Break(debug_error) =
                    validate_core_status_after_step(core, debug_info)
                {
                    return Err(debug_error);
                }

                if let Ok(target_location) =
                    VerifiedBreakpoint::for_address(debug_info, step_result.pc)
                {
                    return Ok(target_location);
                }
            }
            let message = format!(
                "Unexpected halt while stepping past the return address: {return_address:#010x}."
            );
            Err(DebugError::WarnAndContinue { message })
        }
    }
}

/// The `step over` operation will try to optimize, by first identifying the current halt location, and then
/// applying that filter to the available source locations, to find the next available position.
/// - It is reasonable to expect that most stepping operations from within an IDE like VSCode, will initiate
/// at a known source location, and so it is reasonable to start the search with a limited scope.
/// - If the current source location is part of a [`Block`] with a link to another block,
///   we step to the first instruction in the next block, a.k.a. 'statement' level 'step-over'
/// - If not, then we step to the first instruction in the next line in the same sequence,
///   a.k.a. 'line' level 'step-over'
fn get_step_over_location(
    debug_info: &DebugInfo,
    core: &mut impl CoreInterface,
    program_counter: u64,
) -> Result<VerifiedBreakpoint, DebugError> {
    let current_halt_location = VerifiedBreakpoint::for_address(debug_info, program_counter)?;

    let mut candidate_haltpoints: Vec<Instruction> = Vec::new();
    let Some(sequence) =
        // When we filter by address, we expect to get a single sequence, or none.
        line_sequence_for_address(debug_info, current_halt_location.address)
    else {
        let message = format!(
            "No available line program sequences for address {:?}",
            current_halt_location.address
        );
        return Err(DebugError::WarnAndContinue { message });
    };
    // First we try to find the next haltpoint, in the next linked block in the current sequence.
    if let Some(breakpoint) = sequence.haltpoint_for_next_block(current_halt_location.address) {
        return Ok(breakpoint);
    }
    // If we don't find a next block, we try to find the next haltpoint in the same sequence.
    candidate_haltpoints.extend(sequence.blocks.iter().flat_map(|block| {
        block.instructions.iter().filter(|instruction| {
            instruction.role.is_halt_location()
                && instruction.address > current_halt_location.address
        })
    }));
    // Ensure we limit the stepping range to something sensible.
    let return_address = get_return_address(core)?;
    let terminating_address = sequence.last_halt_instruction.unwrap_or(return_address);

    if candidate_haltpoints.is_empty() {
        // We've run out of valid lines in the current sequence, so can just step to the last statement in the sequence.
        let candidate_haltpoint = VerifiedBreakpoint::for_address(debug_info, terminating_address)?;
        if program_counter == candidate_haltpoint.address {
            // We are already at the last statement in the sequence, so we have to attempt a step to the next sequence.
            sequence
                .haltpoint_for_next_block(program_counter)
                .ok_or_else(|| DebugError::WarnAndContinue {
                    message: "No valid halt location found in the current sequence.".to_string(),
                })
        } else {
            Ok(candidate_haltpoint)
        }
    } else {
        // Now step the target until we hit one of the candidate haltpoints, or some eror occurs.
        let (_, next_line_address) =
            step_to_next_line(&candidate_haltpoints, core, debug_info, terminating_address)?;
        VerifiedBreakpoint::for_address(debug_info, next_line_address)
    }
}

// TODO: Normalizing the return address is a common operation, and should probably be implemented in the `CoreInterface` trait.
/// NOTE: [ARMv7-M Architecture Reference Manual](https://developer.arm.com/documentation/ddi0403/ee), Section A5.1.2:
/// We have to clear the last bit to ensure the PC is half-word aligned. (on ARM architecture,
/// when in Thumb state for certain instruction types will set the LSB to 1)
fn get_return_address(core: &mut impl CoreInterface) -> Result<u64, DebugError> {
    let return_register_value: u64 = core.read_core_reg(core.return_address().id())?.try_into()?;
    let return_address = if core.instruction_set().ok() == Some(InstructionSet::Thumb2) {
        return_register_value & !0b1
    } else {
        return_register_value
    };
    Ok(return_address)
}

/// Run the target to the desired address. If available, we will use a breakpoint, otherwise we will use single step.
/// Returns the program counter at the end of the step, when any of the following conditions are met:
/// - We reach the `target_address_range.end()` (inclusive)
/// - We reach some other legitimate halt point (e.g. the user tries to step past a series of statements,
///   but there is another breakpoint active in that "gap")
/// - We encounter an error (e.g. the core locks up, or the USB cable is unplugged, etc.)
/// - It turns out this step will be long-running, and we do not have to wait any longer for the request to complete.
fn run_to_address(
    target_address: u64,
    core: &mut impl CoreInterface,
    debug_info: &DebugInfo,
) -> Result<(CoreStatus, u64), DebugError> {
    let mut program_counter = core
        .read_core_reg(core.program_counter().id())?
        .try_into()?;

    if target_address == program_counter {
        // No need to step further. Some stepping operations will already have stepped the target to the desired location.
        return Ok((core.status()?, program_counter));
    }

    if let Ok((breakpoint_index, is_new_breakpoint)) =
        confirm_or_set_hw_breakpoint(core, target_address)
    {
        core.run()?;
        // It is possible that we are stepping over long running instructions.
        // We have to wait for the outcome, because we have to 'undo' the temporary breakpoints we
        // set to get here.
        match core.wait_for_core_halted(Duration::from_millis(1000)) {
            Ok(()) => {
                // The core halted as expected, althoughit is conceivable that the core has halted,
                // but we have not yet stepped to the target address.
                // For example, if the user tries to step out of a function, but there is another breakpoint active
                // before the end of the function. This is a legitimate situation, so we clear the breakpoint
                // at the target address, and pass control back to the user
                if is_new_breakpoint {
                    core.clear_hw_breakpoint(breakpoint_index)?;
                }
                Ok((
                    core.status()?,
                    core.read_core_reg(core.program_counter().id())?
                        .try_into()?,
                ))
            }
            Err(error) => {
                program_counter = core
                    .halt(Duration::from_millis(500))
                    .map_err(|error| DebugError::WarnAndContinue {
                        message: error.to_string(),
                    })?
                    .pc;
                if is_new_breakpoint {
                    core.clear_hw_breakpoint(breakpoint_index)?;
                }
                if matches!(
                    error,
                    crate::Error::Arm(ArmError::Timeout)
                        | crate::Error::Riscv(RiscvError::Timeout)
                        | crate::Error::Xtensa(XtensaError::Timeout)
                ) {
                    // This is not a quick step and halt operation.
                    // Notify the user that we are not going to wait any longer, and then return
                    // the current program counter so that the debugger can show the user where the
                    tracing::error!(
                        "The core did not halt after stepping to {:#010X}. Forced a halt at {:#010X}. Long running operations between debug steps are not currently supported.",
                        target_address,
                        program_counter
                    );
                    Ok((core.status()?, program_counter))
                } else {
                    // Something else is wrong.
                    Err(DebugError::Other(anyhow::anyhow!(
                        "Unexpected error while waiting for the core to halt after stepping to {:#010X}. Forced a halt at {:#010X}. {:?}.",
                        program_counter,
                        target_address,
                        error
                    )))
                }
            }
        }
    } else {
        // If we don't have breakpoints to use, we have to rely on single stepping.
        step_to_address(target_address, core, debug_info)
    }
}

/// In some cases, we need to single-step the core, until ONE of the following conditions are met:
/// - We reach the `target_address`.
/// - We reach some other legitimate halt point (e.g. the user tries to step past a series of statements,
///   but there is another breakpoint active in that "gap").
/// - We encounter an error (e.g. the core locks up).
// TODO: The ideal would be to implement and use software breakpoints, in stead of single stepping the core.
fn step_to_address(
    target_address: u64,
    core: &mut impl CoreInterface,
    debug_info: &DebugInfo,
) -> Result<(CoreStatus, u64), DebugError> {
    let mut program_counter = core
        .read_core_reg(core.program_counter().id())?
        .try_into()?;
    while target_address != program_counter {
        // Single step the core until we get to the target_address;
        program_counter = core.step()?.pc;
        if let ControlFlow::Break(debug_error) = validate_core_status_after_step(core, debug_info) {
            return Err(debug_error);
        }
    }
    Ok((core.status()?, program_counter))
}

/// Single step the core, until we reach the next valid halt location on the next line in the source file.
// TODO: The ideal would be to implement and use software breakpoints, in stead of single stepping the core.
fn step_to_next_line(
    available_source_locations: &[Instruction],
    core: &mut impl CoreInterface,
    debug_info: &DebugInfo,
    terminating_address: u64,
) -> Result<(CoreStatus, u64), DebugError> {
    let mut program_counter = core
        .read_core_reg(core.program_counter().id())?
        .try_into()?;

    while program_counter <= terminating_address {
        if available_source_locations
            .iter()
            .any(|instruction| instruction.address == program_counter)
        {
            break;
        }
        // Single step the core until we get to the target_address;
        program_counter = core.step()?.pc;
        if let ControlFlow::Break(debug_error) = validate_core_status_after_step(core, debug_info) {
            return Err(debug_error);
        }
    }
    Ok((core.status()?, program_counter))
}

/// After stepping, ensure that the core didn't halt for some other reason.
fn validate_core_status_after_step(
    core: &mut impl CoreInterface,
    debug_info: &DebugInfo,
) -> ControlFlow<DebugError, ()> {
    if let Ok(Some(exception_info)) = check_for_exception(core, debug_info) {
        let message = format!(
            "Exception encountered while stepping to the next line: {:?}",
            exception_info.description
        );
        ControlFlow::Break(DebugError::WarnAndContinue { message })
    } else {
        match core.status() {
            Ok(CoreStatus::Halted(halt_reason)) => match halt_reason {
                HaltReason::Step | HaltReason::Request => ControlFlow::Continue(()),
                // This is a recoverable error, and can be reported to the user higher up in the call stack.
                other_halt_reason => {
                    let message = format!("Target halted unexpectedly before we reached the destination address of a step operation. Reason: {other_halt_reason:?}");
                    ControlFlow::Break(DebugError::WarnAndContinue { message })
                }
            },
            // This is not a recoverable error, and will result in the debug session ending
            // we have no predicatable way of successfully continuing the session)
            Ok(other_status) => ControlFlow::Break(DebugError::Other(anyhow::anyhow!(
                "Target failed to reach the destination address of a step operation: {:?}",
                other_status
            ))),
            Err(error) => ControlFlow::Break(error.into()),
        }
    }
}

// TODO: This functionality probably belongs in the `CoreInterface` trait, and should be implemented for all cores.
/// Confirm if a breakpoint is already set for this address, and return the breakpoint comparator index.
/// This funciton will set a hardware breakpoint at the specified address,
/// provided a hw_breakpoint is available, or confirm if one is already set.
/// If successful it will return the index of the breakpoint comparator that was used,
/// and a flag on whether this was pre-existing or newly set.
fn confirm_or_set_hw_breakpoint(
    core: &mut impl CoreInterface,
    address: u64,
) -> Result<(usize, bool), DebugError> {
    for (index, bp) in core.hw_breakpoints()?.iter().enumerate() {
        if bp.is_none() {
            core.set_hw_breakpoint(index, address)?;
            return Ok((index, true));
        } else if *bp == Some(address) {
            return Ok((index, false));
        }
    }
    Err(DebugError::Other(anyhow::anyhow!(
        "No available hardware breakpoints"
    )))
}

/// Check if an exception is currently active on the core, and return the exception details if found.
fn check_for_exception(
    core: &mut impl CoreInterface,
    debug_info: &DebugInfo,
) -> Result<Option<ExceptionInfo>, DebugError> {
    let debug_registers = DebugRegisters::from_core(core);
    let exception_interface = exception_handler_for_core(core.core_type());
    match exception_interface.exception_details(core, &debug_registers, debug_info)? {
        Some(exception_info) => {
            tracing::trace!("Found exception context: {}", exception_info.description);
            Ok(Some(exception_info))
        }
        None => {
            tracing::trace!("No exception context found, proceeeding.");
            Ok(None)
        }
    }
}
