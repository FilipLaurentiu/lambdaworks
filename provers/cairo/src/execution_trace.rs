use super::{
    cairo_mem::CairoMemory,
    decode::{
        instruction_flags::{
            aux_get_last_nim_of_field_element, ApUpdate, CairoInstructionFlags, CairoOpcode,
            DstReg, Op0Reg, Op1Src, PcUpdate, ResLogic,
        },
        instruction_offsets::InstructionOffsets,
    },
    register_states::RegisterStates,
};
use crate::layouts::plain::air::{
    CairoAIR, PublicInputs, EXTRA_ADDR, FRAME_DST_ADDR, FRAME_OP0_ADDR, FRAME_OP1_ADDR, FRAME_PC,
    OFF_DST, OFF_OP0, OFF_OP1, RC_HOLES,
};
use lambdaworks_math::{
    field::fields::fft_friendly::stark_252_prime_field::Stark252PrimeField,
    unsigned_integer::element::UnsignedInteger,
};
use stark_platinum_prover::{trace::TraceTable, Felt252};

type CairoTraceTable = TraceTable<Stark252PrimeField>;
// NOTE: This should be deleted and use CairoAIR::STEP_SIZE once it is set to 16
const CAIRO_STEP: usize = 16;

const PLAIN_LAYOUT_NUM_COLUMNS: usize = 8;

/// Builds the Cairo main trace (i.e. the trace without the auxiliary columns).
/// Builds the execution trace, fills the offset range-check holes and memory holes, adds
/// public memory dummy accesses (See section 9.8 of the Cairo whitepaper) and pads the result
/// so that it has a trace length equal to the closest power of two.
pub fn build_main_trace(
    register_states: &RegisterStates,
    memory: &CairoMemory,
    public_input: &mut PublicInputs,
) -> CairoTraceTable {
    let mut main_trace = build_cairo_execution_trace(register_states, memory);

    let mut address_cols =
        main_trace.merge_columns(&[FRAME_PC, FRAME_DST_ADDR, FRAME_OP0_ADDR, FRAME_OP1_ADDR]);

    address_cols.sort_by_key(|x| x.representative());

    let (rc_holes, rc_min, rc_max) = get_rc_holes(&main_trace, &[OFF_DST, OFF_OP0, OFF_OP1]);
    public_input.range_check_min = Some(rc_min);
    public_input.range_check_max = Some(rc_max);
    fill_rc_holes(&mut main_trace, &rc_holes);

    let memory_holes = get_memory_holes(&address_cols, public_input.codelen);

    if !memory_holes.is_empty() {
        fill_memory_holes(&mut main_trace, &memory_holes);
    }

    add_pub_memory_dummy_accesses(
        &mut main_trace,
        public_input.public_memory.len(),
        memory_holes.len(),
    );

    let trace_len_next_power_of_two = main_trace.n_rows().next_power_of_two();
    let padding_len = trace_len_next_power_of_two - main_trace.n_rows();
    main_trace.pad_with_last_row(padding_len);

    main_trace
}

/// Artificial `(0, 0)` dummy memory accesses must be added for the public memory.
/// See section 9.8 of the Cairo whitepaper.
fn add_pub_memory_dummy_accesses(
    main_trace: &mut CairoTraceTable,
    pub_memory_len: usize,
    last_memory_hole_idx: usize,
) {
    for i in 0..pub_memory_len {
        main_trace.set_or_extend(last_memory_hole_idx + i, EXTRA_ADDR, &Felt252::zero());
    }
}

/// Gets holes from the range-checked columns. These holes must be filled for the
/// permutation range-checks, as can be read in section 9.9 of the Cairo whitepaper.
/// Receives the trace and the indexes of the range-checked columns.
/// Outputs the holes that must be filled to make the range continuous and the extreme
/// values rc_min and rc_max, corresponding to the minimum and maximum values of the range.
/// NOTE: These extreme values should be received as public inputs in the future and not
/// calculated here.
fn get_rc_holes(trace: &CairoTraceTable, columns_indices: &[usize]) -> (Vec<Felt252>, u16, u16) {
    let offset_columns = trace.merge_columns(columns_indices);

    let mut sorted_offset_representatives: Vec<u16> = offset_columns
        .iter()
        .map(|x| x.representative().into())
        .collect();
    sorted_offset_representatives.sort();

    let mut all_missing_values: Vec<Felt252> = Vec::new();

    for window in sorted_offset_representatives.windows(2) {
        if window[1] != window[0] {
            let mut missing_range: Vec<_> = ((window[0] + 1)..window[1])
                .map(|x| Felt252::from(x as u64))
                .collect();
            all_missing_values.append(&mut missing_range);
        }
    }

    let multiple_of_three_padding =
        ((all_missing_values.len() + 2) / 3) * 3 - all_missing_values.len();
    let padding_element = Felt252::from(*sorted_offset_representatives.last().unwrap() as u64);
    all_missing_values.append(&mut vec![padding_element; multiple_of_three_padding]);

    (
        all_missing_values,
        sorted_offset_representatives[0],
        sorted_offset_representatives.last().cloned().unwrap(),
    )
}

/// Fills holes found in the range-checked columns.
fn fill_rc_holes(trace: &mut CairoTraceTable, holes: &[Felt252]) {
    holes.iter().enumerate().for_each(|(i, hole)| {
        trace.set_or_extend(i, RC_HOLES, hole);
    });

    // Fill the rest of the RC_HOLES column to avoid inexistent zeros
    let mut offsets = trace.merge_columns(&[OFF_DST, OFF_OP0, OFF_OP1, RC_HOLES]);

    offsets.sort_by_key(|x| x.representative());
    let greatest_offset = offsets.last().unwrap();
    (holes.len()..trace.n_rows()).for_each(|i| {
        trace.set_or_extend(i, RC_HOLES, greatest_offset);
    });
}

/// Get memory holes from accessed addresses. These memory holes appear
/// as a consequence of interaction with builtins.
/// Returns a vector of addresses that were not present in the input vector (holes)
///
/// # Arguments
///
/// * `sorted_addrs` - Vector of sorted memory addresses.
/// * `codelen` - the length of the Cairo program instructions.
fn get_memory_holes(sorted_addrs: &[Felt252], codelen: usize) -> Vec<Felt252> {
    let mut memory_holes = Vec::new();
    let mut prev_addr = &sorted_addrs[0];

    for addr in sorted_addrs.iter() {
        let addr_diff = addr - prev_addr;

        // If the candidate memory hole has an address belonging to the program segment (public
        // memory), that is not accounted here since public memory is added in a posterior step of
        // the protocol.
        if addr_diff != Felt252::one()
            && addr_diff != Felt252::zero()
            && addr.representative() > (codelen as u64).into()
        {
            let mut hole_addr = prev_addr + Felt252::one();

            while hole_addr.representative() < addr.representative() {
                if hole_addr.representative() > (codelen as u64).into() {
                    memory_holes.push(hole_addr);
                }
                hole_addr += Felt252::one();
            }
        }
        prev_addr = addr;
    }

    memory_holes
}

/// Fill memory holes in the extra address column of the trace with the missing addresses.
fn fill_memory_holes(trace: &mut CairoTraceTable, memory_holes: &[Felt252]) {
    memory_holes.iter().enumerate().for_each(|(i, hole)| {
        trace.set_or_extend(i, EXTRA_ADDR, hole);
    });
}

/// Receives the raw Cairo trace and memory as outputted from the Cairo VM and returns
/// the trace table used to Felt252ed the Cairo STARK prover.
/// The constraints of the Cairo AIR are defined over this trace rather than the raw trace
/// obtained from the Cairo VM, this is why this function is needed.
pub fn build_cairo_execution_trace(
    register_states: &RegisterStates,
    memory: &CairoMemory,
) -> CairoTraceTable {
    let num_steps = register_states.steps();

    // Instruction flags and offsets are decoded from the raw instructions and represented
    // by the CairoInstructionFlags and InstructionOffsets as an intermediate representation
    let (flags, biased_offsets): (Vec<CairoInstructionFlags>, Vec<InstructionOffsets>) =
        register_states
            .flags_and_offsets(memory)
            .unwrap()
            .into_iter()
            .unzip();

    // dst, op0, op1 and res are computed from flags and offsets
    let (dst_addrs, mut dsts): (Vec<Felt252>, Vec<Felt252>) =
        compute_dst(&flags, &biased_offsets, register_states, memory);
    let (op0_addrs, mut op0s): (Vec<Felt252>, Vec<Felt252>) =
        compute_op0(&flags, &biased_offsets, register_states, memory);
    let (op1_addrs, op1s): (Vec<Felt252>, Vec<Felt252>) =
        compute_op1(&flags, &biased_offsets, register_states, memory, &op0s);
    let mut res = compute_res(&flags, &op0s, &op1s, &dsts);

    // In some cases op0, dst or res may need to be updated from the already calculated values
    update_values(&flags, register_states, &mut op0s, &mut dsts, &mut res);

    // Flags and offsets are transformed to a bit representation. This is needed since
    // the flag constraints of the Cairo AIR are defined over bit representations of these
    let bit_prefix_flags: Vec<[Felt252; 16]> = flags
        .iter()
        .map(CairoInstructionFlags::to_trace_representation)
        .collect();
    let unbiased_offsets: Vec<(Felt252, Felt252, Felt252)> = biased_offsets
        .iter()
        .map(InstructionOffsets::to_trace_representation)
        .collect();

    // ap, fp, pc and instruction columns are computed
    let aps: Vec<Felt252> = register_states
        .rows
        .iter()
        .map(|t| Felt252::from(t.ap))
        .collect();

    let fps: Vec<Felt252> = register_states
        .rows
        .iter()
        .map(|t| Felt252::from(t.fp))
        .collect();

    let pcs: Vec<Felt252> = register_states
        .rows
        .iter()
        .map(|t| Felt252::from(t.pc))
        .collect();

    let instructions: Vec<Felt252> = register_states
        .rows
        .iter()
        .map(|t| *memory.get(&t.pc).unwrap())
        .collect();

    // t0, t1 and mul derived values are constructed. For details reFelt252r to
    // section 9.1 of the Cairo whitepaper
    let two = Felt252::from(2);
    let t0: Vec<Felt252> = bit_prefix_flags
        .iter()
        .zip(&dsts)
        .map(|(repr_flags, dst)| (repr_flags[9] - two * repr_flags[10]) * dst)
        .collect();
    let t1: Vec<Felt252> = t0.iter().zip(&res).map(|(t, r)| t * r).collect();
    let mul: Vec<Felt252> = op0s.iter().zip(&op1s).map(|(op0, op1)| op0 * op1).collect();

    let mut trace: CairoTraceTable =
        TraceTable::allocate_with_zeros(num_steps, PLAIN_LAYOUT_NUM_COLUMNS, CAIRO_STEP);

    set_offsets(&mut trace, unbiased_offsets);
    set_bit_prefix_flags(&mut trace, bit_prefix_flags);
    set_mem_pool(
        &mut trace,
        pcs,
        instructions,
        op0_addrs,
        op0s,
        dst_addrs,
        dsts,
        op1_addrs,
        op1s,
    );
    set_update_pc(&mut trace, aps, t0, t1, mul, fps, res);

    trace
}

/// Returns the vector of res values.
fn compute_res(
    flags: &[CairoInstructionFlags],
    op0s: &[Felt252],
    op1s: &[Felt252],
    dsts: &[Felt252],
) -> Vec<Felt252> {
    /*
    Cairo whitepaper, page 33 - https://eprint.iacr.org/2021/1063.pdf
    # Compute res.
    if pc_update == 4:
        if res_logic == 0 && opcode == 0 && ap_update != 1:
            res = Unused
        else:
            Undefined Behavior
    else if pc_update = 0, 1 or 2:
        switch res_logic:
            case 0: res = op1
            case 1: res = op0 + op1
            case 2: res = op0 * op1
            default: Undefined Behavior
    else: Undefined Behavior
    */
    flags
        .iter()
        .zip(op0s)
        .zip(op1s)
        .zip(dsts)
        .map(|(((f, op0), op1), dst)| {
            match f.pc_update {
                PcUpdate::Jnz => {
                    match (&f.res_logic, &f.opcode, &f.ap_update) {
                        (
                            ResLogic::Op1,
                            CairoOpcode::NOp,
                            ApUpdate::Regular | ApUpdate::Add1 | ApUpdate::Add2,
                        ) => {
                            // In a `jnz` instruction, res is not used, so it is used
                            // to hold the value v = dst^(-1) as an optimization.
                            // This is important for the calculation of the `t1` virtual column
                            // values later on.
                            // See section 9.5 of the Cairo whitepaper, page 53.
                            if dst == &Felt252::zero() {
                                *dst
                            } else {
                                dst.inv().unwrap()
                            }
                        }
                        _ => {
                            panic!("Undefined Behavior");
                        }
                    }
                }
                PcUpdate::Regular | PcUpdate::Jump | PcUpdate::JumpRel => match f.res_logic {
                    ResLogic::Op1 => *op1,
                    ResLogic::Add => op0 + op1,
                    ResLogic::Mul => op0 * op1,
                    ResLogic::Unconstrained => {
                        panic!("Undefined Behavior");
                    }
                },
            }
        })
        .collect()
}

/// Returns the vector of:
/// - dst_addrs
/// - dsts
fn compute_dst(
    flags: &[CairoInstructionFlags],
    offsets: &[InstructionOffsets],
    register_states: &RegisterStates,
    memory: &CairoMemory,
) -> (Vec<Felt252>, Vec<Felt252>) {
    /* Cairo whitepaper, page 33 - https://eprint.iacr.org/2021/1063.pdf

    # Compute dst
    if dst_reg == 0:
        dst = m(ap + offdst)
    else:
        dst = m(fp + offdst)
    */
    flags
        .iter()
        .zip(offsets)
        .zip(register_states.rows.iter())
        .map(|((f, o), t)| match f.dst_reg {
            DstReg::AP => {
                let addr = t.ap.checked_add_signed(o.off_dst.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
            DstReg::FP => {
                let addr = t.fp.checked_add_signed(o.off_dst.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
        })
        .unzip()
}

/// Returns the vector of:
/// - op0_addrs
/// - op0s
fn compute_op0(
    flags: &[CairoInstructionFlags],
    offsets: &[InstructionOffsets],
    register_states: &RegisterStates,
    memory: &CairoMemory,
) -> (Vec<Felt252>, Vec<Felt252>) {
    /* Cairo whitepaper, page 33 - https://eprint.iacr.org/2021/1063.pdf

    # Compute op0.
    if op0_reg == 0:
        op0 = m(ap + offop0)
    else:
        op0 = m(fp + offop0)
    */
    flags
        .iter()
        .zip(offsets)
        .zip(register_states.rows.iter())
        .map(|((f, o), t)| match f.op0_reg {
            Op0Reg::AP => {
                let addr = t.ap.checked_add_signed(o.off_op0.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
            Op0Reg::FP => {
                let addr = t.fp.checked_add_signed(o.off_op0.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
        })
        .unzip()
}

/// Returns the vector of:
/// - op1_addrs
/// - op1s
fn compute_op1(
    flags: &[CairoInstructionFlags],
    offsets: &[InstructionOffsets],
    register_states: &RegisterStates,
    memory: &CairoMemory,
    op0s: &[Felt252],
) -> (Vec<Felt252>, Vec<Felt252>) {
    /* Cairo whitepaper, page 33 - https://eprint.iacr.org/2021/1063.pdf
    # Compute op1 and instruction_size.
    switch op1_src:
        case 0:
            instruction_size = 1
            op1 = m(op0 + offop1)
        case 1:
            instruction_size = 2
            op1 = m(pc + offop1)
            # If offop1 = 1, we have op1 = immediate_value.
        case 2:
            instruction_size = 1
            op1 = m(fp + offop1)
        case 4:
            instruction_size = 1
            op1 = m(ap + offop1)
        default:
            Undefined Behavior
    */
    flags
        .iter()
        .zip(offsets)
        .zip(op0s)
        .zip(register_states.rows.iter())
        .map(|(((flag, offset), op0), trace_state)| match flag.op1_src {
            Op1Src::Op0 => {
                let addr = aux_get_last_nim_of_field_element(op0)
                    .checked_add_signed(offset.off_op1.into())
                    .unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
            Op1Src::Imm => {
                let pc = trace_state.pc;
                let addr = pc.checked_add_signed(offset.off_op1.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
            Op1Src::AP => {
                let ap = trace_state.ap;
                let addr = ap.checked_add_signed(offset.off_op1.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
            Op1Src::FP => {
                let fp = trace_state.fp;
                let addr = fp.checked_add_signed(offset.off_op1.into()).unwrap();
                (Felt252::from(addr), *memory.get(&addr).unwrap())
            }
        })
        .unzip()
}

/// Depending on the instruction opcodes, some values should be updated.
/// This function updates op0s, dst, res in place when the conditions hold.
fn update_values(
    flags: &[CairoInstructionFlags],
    register_states: &RegisterStates,
    op0s: &mut [Felt252],
    dst: &mut [Felt252],
    res: &mut [Felt252],
) {
    for (i, f) in flags.iter().enumerate() {
        if f.opcode == CairoOpcode::Call {
            let instruction_size = if flags[i].op1_src == Op1Src::Imm {
                2
            } else {
                1
            };
            op0s[i] = (register_states.rows[i].pc + instruction_size).into();
            dst[i] = register_states.rows[i].fp.into();
        } else if f.opcode == CairoOpcode::AssertEq {
            res[i] = dst[i];
        }
    }
}

// NOTE: Leaving this function despite not being used anywhere. It could be useful once
// we implement layouts with the range-check builtin.
#[allow(dead_code)]
fn decompose_rc_values_into_trace_columns(rc_values: &[&Felt252]) -> [Vec<Felt252>; 8] {
    let mask = UnsignedInteger::from_hex("FFFF").unwrap();
    let mut rc_base_types: Vec<UnsignedInteger<4>> =
        rc_values.iter().map(|x| x.representative()).collect();

    let mut decomposition_columns: Vec<Vec<Felt252>> = Vec::new();

    for _ in 0..8 {
        decomposition_columns.push(
            rc_base_types
                .iter()
                .map(|&x| Felt252::from(&(x & mask)))
                .collect(),
        );

        rc_base_types = rc_base_types.iter().map(|&x| x >> 16).collect();
    }

    // This can't fail since we have 8 pushes
    decomposition_columns.try_into().unwrap()
}

fn set_bit_prefix_flags(trace: &mut CairoTraceTable, bit_prefix_flags: Vec<[Felt252; 16]>) {
    for (step_idx, flags) in bit_prefix_flags.into_iter().enumerate() {
        for (flag_idx, flag) in flags.into_iter().enumerate() {
            trace.set(flag_idx + CAIRO_STEP * step_idx, 1, flag);
        }
    }
}

fn set_offsets(trace: &mut CairoTraceTable, offsets: Vec<(Felt252, Felt252, Felt252)>) {
    // NOTE: We should check that these offsets correspond to the off0, off1 and off2.
    const OFF_DST_OFFSET: usize = 0;
    const OFF_OP0_OFFSET: usize = 8;
    const OFF_OP1_OFFSET: usize = 4;

    for (step_idx, (off_dst, off_op0, off_op1)) in offsets.into_iter().enumerate() {
        trace.set(OFF_DST_OFFSET + CAIRO_STEP * step_idx, 0, off_dst);
        trace.set(OFF_OP0_OFFSET + CAIRO_STEP * step_idx, 0, off_op0);
        trace.set(OFF_OP1_OFFSET + CAIRO_STEP * step_idx, 0, off_op1);
    }
}

// Column 3
fn set_mem_pool(
    trace: &mut CairoTraceTable,
    pcs: Vec<Felt252>,
    instructions: Vec<Felt252>,
    op0_addrs: Vec<Felt252>,
    op0_vals: Vec<Felt252>,
    dst_addrs: Vec<Felt252>,
    dst_vals: Vec<Felt252>,
    op1_addrs: Vec<Felt252>,
    op1_vals: Vec<Felt252>,
) {
    const PC_OFFSET: usize = 0;
    const INST_OFFSET: usize = 1;
    const OP0_ADDR_OFFSET: usize = 4;
    const OP0_VAL_OFFSET: usize = 5;
    const DST_ADDR_OFFSET: usize = 8;
    const DST_VAL_OFFSET: usize = 9;
    const OP1_ADDR_OFFSET: usize = 12;
    const OP1_VAL_OFFSET: usize = 13;

    for (step_idx, (pc, inst, op0_addr, op0_val, dst_addr, dst_val, op1_addr, op1_val)) in
        itertools::izip!(
            pcs,
            instructions,
            op0_addrs,
            op0_vals,
            dst_addrs,
            dst_vals,
            op1_addrs,
            op1_vals
        )
        .enumerate()
    {
        trace.set(PC_OFFSET + CAIRO_STEP * step_idx, 3, pc);
        trace.set(INST_OFFSET + CAIRO_STEP * step_idx, 3, inst);
        trace.set(OP0_ADDR_OFFSET + CAIRO_STEP * step_idx, 3, op0_addr);
        trace.set(OP0_VAL_OFFSET + CAIRO_STEP * step_idx, 3, op0_val);
        trace.set(DST_ADDR_OFFSET + CAIRO_STEP * step_idx, 3, dst_addr);
        trace.set(DST_VAL_OFFSET + CAIRO_STEP * step_idx, 3, dst_val);
        trace.set(OP1_ADDR_OFFSET + CAIRO_STEP * step_idx, 3, op1_addr);
        trace.set(OP1_VAL_OFFSET + CAIRO_STEP * step_idx, 3, op1_val);
    }
}

fn set_update_pc(
    trace: &mut CairoTraceTable,
    aps: Vec<Felt252>,
    t0s: Vec<Felt252>,
    t1s: Vec<Felt252>,
    mul: Vec<Felt252>,
    fps: Vec<Felt252>,
    res: Vec<Felt252>,
) {
    const AP_OFFSET: usize = 0;
    const TMP0_OFFSET: usize = 2;
    const OPS_MUL_OFFSET: usize = 4;
    const FP_OFFSET: usize = 8;
    const TMP1_OFFSET: usize = 10;
    const RES_OFFSET: usize = 12;

    for (step_idx, (ap, tmp0, m, fp, tmp1, res)) in
        itertools::izip!(aps, t0s, mul, fps, t1s, res).enumerate()
    {
        trace.set(AP_OFFSET + CAIRO_STEP * step_idx, 5, ap);
        trace.set(TMP0_OFFSET + CAIRO_STEP * step_idx, 5, tmp0);
        trace.set(OPS_MUL_OFFSET + CAIRO_STEP * step_idx, 5, m);
        trace.set(FP_OFFSET + CAIRO_STEP * step_idx, 5, fp);
        trace.set(TMP1_OFFSET + CAIRO_STEP * step_idx, 5, tmp1);
        trace.set(RES_OFFSET + CAIRO_STEP * step_idx, 5, res);
    }
}

#[cfg(test)]
mod test {
    use crate::{
        cairo_layout::CairoLayout, layouts::plain::air::EXTRA_VAL, runner::run::run_program,
        tests::utils::cairo0_program_path,
    };

    use super::*;
    use lambdaworks_math::field::element::FieldElement;
    use stark_platinum_prover::table::Table;

    #[test]
    fn test_rc_decompose() {
        let fifteen = Felt252::from_hex("000F000F000F000F000F000F000F000F").unwrap();
        let sixteen = Felt252::from_hex("00100010001000100010001000100010").unwrap();
        let one_two_three = Felt252::from_hex("00010002000300040005000600070008").unwrap();

        let decomposition_columns =
            decompose_rc_values_into_trace_columns(&[&fifteen, &sixteen, &one_two_three]);

        for row in &decomposition_columns {
            assert_eq!(row[0], Felt252::from_hex("F").unwrap());
            assert_eq!(row[1], Felt252::from_hex("10").unwrap());
        }

        assert_eq!(decomposition_columns[0][2], Felt252::from_hex("8").unwrap());
        assert_eq!(decomposition_columns[1][2], Felt252::from_hex("7").unwrap());
        assert_eq!(decomposition_columns[2][2], Felt252::from_hex("6").unwrap());
        assert_eq!(decomposition_columns[3][2], Felt252::from_hex("5").unwrap());
        assert_eq!(decomposition_columns[4][2], Felt252::from_hex("4").unwrap());
        assert_eq!(decomposition_columns[5][2], Felt252::from_hex("3").unwrap());
        assert_eq!(decomposition_columns[6][2], Felt252::from_hex("2").unwrap());
        assert_eq!(decomposition_columns[7][2], Felt252::from_hex("1").unwrap());
    }

    #[test]
    fn test_fill_range_check_values() {
        let columns = vec![
            vec![FieldElement::from(1); 3],
            vec![FieldElement::from(4); 3],
            vec![FieldElement::from(7); 3],
        ];
        let expected_col = vec![
            FieldElement::from(2),
            FieldElement::from(3),
            FieldElement::from(5),
            FieldElement::from(6),
            FieldElement::from(7),
            FieldElement::from(7),
        ];
        let table = TraceTable::<Stark252PrimeField>::from_columns(columns, 1);

        let (col, rc_min, rc_max) = get_rc_holes(&table, &[0, 1, 2]);
        assert_eq!(col, expected_col);
        assert_eq!(rc_min, 1);
        assert_eq!(rc_max, 7);
    }

    #[test]
    fn test_add_missing_values_to_rc_holes_column() {
        let mut row = vec![Felt252::from(5); 36];
        row[35] = Felt252::zero();
        let data = row.repeat(8);
        let table = Table::new(data, 36);

        let mut main_trace = TraceTable::<Stark252PrimeField> {
            table,
            step_size: 1,
        };

        let rc_holes = vec![
            Felt252::from(1),
            Felt252::from(2),
            Felt252::from(3),
            Felt252::from(4),
            Felt252::from(5),
            Felt252::from(6),
        ];

        fill_rc_holes(&mut main_trace, &rc_holes);

        let expected_rc_holes_column = vec![
            Felt252::from(1),
            Felt252::from(2),
            Felt252::from(3),
            Felt252::from(4),
            Felt252::from(5),
            Felt252::from(6),
            Felt252::from(6),
            Felt252::from(6),
        ];

        let rc_holes_column = main_trace.columns()[35].clone();

        assert_eq!(expected_rc_holes_column, rc_holes_column);
    }

    #[test]
    fn test_get_memory_holes_no_codelen() {
        // We construct a sorted addresses list [1, 2, 3, 6, 7, 8, 9, 13, 14, 15], and
        // set codelen = 0. With this value of codelen, any holes present between
        // the min and max addresses should be returned by the function.
        let mut addrs: Vec<Felt252> = (1..4).map(Felt252::from).collect();
        let addrs_extension: Vec<Felt252> = (6..10).map(Felt252::from).collect();
        addrs.extend_from_slice(&addrs_extension);
        let addrs_extension: Vec<Felt252> = (13..16).map(Felt252::from).collect();
        addrs.extend_from_slice(&addrs_extension);
        let codelen = 0;

        let expected_memory_holes = vec![
            Felt252::from(4),
            Felt252::from(5),
            Felt252::from(10),
            Felt252::from(11),
            Felt252::from(12),
        ];
        let calculated_memory_holes = get_memory_holes(&addrs, codelen);

        assert_eq!(expected_memory_holes, calculated_memory_holes);
    }

    #[test]
    fn test_get_memory_holes_inside_program_section() {
        // We construct a sorted addresses list [1, 2, 3, 8, 9] and we
        // set a codelen of 9. Since all the holes will be inside the
        // program segment (meaning from addresses 1 to 9), the function
        // should not return any of them.
        let mut addrs: Vec<Felt252> = (1..4).map(Felt252::from).collect();
        let addrs_extension: Vec<Felt252> = (8..10).map(Felt252::from).collect();
        addrs.extend_from_slice(&addrs_extension);
        let codelen = 9;

        let calculated_memory_holes = get_memory_holes(&addrs, codelen);
        let expected_memory_holes: Vec<Felt252> = Vec::new();

        assert_eq!(expected_memory_holes, calculated_memory_holes);
    }

    #[test]
    fn test_get_memory_holes_outside_program_section() {
        // We construct a sorted addresses list [1, 2, 3, 8, 9] and we
        // set a codelen of 6. The holes found inside the program section,
        // i.e. in the address range between 1 to 6, should not be returned.
        // So addresses 4, 5 and 6 will no be returned, only address 7.
        let mut addrs: Vec<Felt252> = (1..4).map(Felt252::from).collect();
        let addrs_extension: Vec<Felt252> = (8..10).map(Felt252::from).collect();
        addrs.extend_from_slice(&addrs_extension);
        let codelen = 6;

        let calculated_memory_holes = get_memory_holes(&addrs, codelen);
        let expected_memory_holes = vec![Felt252::from(7)];

        assert_eq!(expected_memory_holes, calculated_memory_holes);
    }

    #[test]
    fn test_fill_memory_holes() {
        const TRACE_COL_LEN: usize = 2;
        const NUM_TRACE_COLS: usize = EXTRA_VAL + 1;

        let mut trace_cols = vec![vec![Felt252::zero(); TRACE_COL_LEN]; NUM_TRACE_COLS];
        trace_cols[FRAME_PC][0] = Felt252::one();
        trace_cols[FRAME_DST_ADDR][0] = Felt252::from(2);
        trace_cols[FRAME_OP0_ADDR][0] = Felt252::from(3);
        trace_cols[FRAME_OP1_ADDR][0] = Felt252::from(5);
        trace_cols[FRAME_PC][1] = Felt252::from(6);
        trace_cols[FRAME_DST_ADDR][1] = Felt252::from(9);
        trace_cols[FRAME_OP0_ADDR][1] = Felt252::from(10);
        trace_cols[FRAME_OP1_ADDR][1] = Felt252::from(11);
        let mut trace = TraceTable::from_columns(trace_cols, 1);

        let memory_holes = vec![Felt252::from(4), Felt252::from(7), Felt252::from(8)];
        fill_memory_holes(&mut trace, &memory_holes);

        let extra_addr = &trace.columns()[EXTRA_ADDR];
        assert_eq!(extra_addr, &memory_holes)
    }

    #[test]
    fn set_offsets_works() {
        let program_content = std::fs::read(cairo0_program_path("fibonacci_stone.json")).unwrap();
        let mut trace: CairoTraceTable = TraceTable::allocate_with_zeros(128, 8, 16);
        let (register_states, memory, _) =
            run_program(None, CairoLayout::Plain, &program_content).unwrap();

        let (_, biased_offsets): (Vec<CairoInstructionFlags>, Vec<InstructionOffsets>) =
            register_states
                .flags_and_offsets(&memory)
                .unwrap()
                .into_iter()
                .unzip();

        let unbiased_offsets: Vec<(Felt252, Felt252, Felt252)> = biased_offsets
            .iter()
            .map(InstructionOffsets::to_trace_representation)
            .collect();

        set_offsets(&mut trace, unbiased_offsets);

        trace.table.columns()[0][0..50]
            .iter()
            .for_each(|v| println!("VAL: {}", v));
    }

    #[test]
    fn set_update_pc_works() {
        let program_content = std::fs::read(cairo0_program_path("fibonacci_stone.json")).unwrap();
        let mut trace: CairoTraceTable = TraceTable::allocate_with_zeros(128, 8, 16);
        let (register_states, memory, _) =
            run_program(None, CairoLayout::Plain, &program_content).unwrap();

        let (flags, biased_offsets): (Vec<CairoInstructionFlags>, Vec<InstructionOffsets>) =
            register_states
                .flags_and_offsets(&memory)
                .unwrap()
                .into_iter()
                .unzip();

        // dst, op0, op1 and res are computed from flags and offsets
        let (_dst_addrs, mut dsts): (Vec<Felt252>, Vec<Felt252>) =
            compute_dst(&flags, &biased_offsets, &register_states, &memory);
        let (_op0_addrs, mut op0s): (Vec<Felt252>, Vec<Felt252>) =
            compute_op0(&flags, &biased_offsets, &register_states, &memory);
        let (_op1_addrs, op1s): (Vec<Felt252>, Vec<Felt252>) =
            compute_op1(&flags, &biased_offsets, &register_states, &memory, &op0s);
        let mut res = compute_res(&flags, &op0s, &op1s, &dsts);

        update_values(&flags, &register_states, &mut op0s, &mut dsts, &mut res);

        let aps: Vec<Felt252> = register_states
            .rows
            .iter()
            .map(|t| Felt252::from(t.ap))
            .collect();
        let fps: Vec<Felt252> = register_states
            .rows
            .iter()
            .map(|t| Felt252::from(t.fp))
            .collect();

        let trace_repr_flags: Vec<[Felt252; 16]> = flags
            .iter()
            .map(CairoInstructionFlags::to_trace_representation)
            .collect();

        let two = Felt252::from(2);
        let t0: Vec<Felt252> = trace_repr_flags
            .iter()
            .zip(&dsts)
            .map(|(repr_flags, dst)| (repr_flags[9] - two * repr_flags[10]) * dst)
            .collect();
        let t1: Vec<Felt252> = t0.iter().zip(&res).map(|(t, r)| t * r).collect();
        let mul: Vec<Felt252> = op0s.iter().zip(&op1s).map(|(op0, op1)| op0 * op1).collect();

        set_update_pc(&mut trace, aps, t0, t1, mul, fps, res);

        trace.table.columns()[5][0..50]
            .iter()
            .enumerate()
            .for_each(|(i, v)| println!("ROW {} - VALUE: {}", i, v));
    }

    #[test]
    fn set_mem_pool_works() {
        let program_content = std::fs::read(cairo0_program_path("fibonacci_stone.json")).unwrap();
        let mut trace: CairoTraceTable = TraceTable::allocate_with_zeros(128, 8, 16);
        let (register_states, memory, _) =
            run_program(None, CairoLayout::Plain, &program_content).unwrap();

        let (flags, biased_offsets): (Vec<CairoInstructionFlags>, Vec<InstructionOffsets>) =
            register_states
                .flags_and_offsets(&memory)
                .unwrap()
                .into_iter()
                .unzip();

        // dst, op0, op1 and res are computed from flags and offsets
        let (dst_addrs, mut dsts): (Vec<Felt252>, Vec<Felt252>) =
            compute_dst(&flags, &biased_offsets, &register_states, &memory);
        let (op0_addrs, mut op0s): (Vec<Felt252>, Vec<Felt252>) =
            compute_op0(&flags, &biased_offsets, &register_states, &memory);
        let (op1_addrs, op1s): (Vec<Felt252>, Vec<Felt252>) =
            compute_op1(&flags, &biased_offsets, &register_states, &memory, &op0s);
        let mut res = compute_res(&flags, &op0s, &op1s, &dsts);

        update_values(&flags, &register_states, &mut op0s, &mut dsts, &mut res);

        let pcs: Vec<Felt252> = register_states
            .rows
            .iter()
            .map(|t| Felt252::from(t.pc))
            .collect();
        let instructions: Vec<Felt252> = register_states
            .rows
            .iter()
            .map(|t| *memory.get(&t.pc).unwrap())
            .collect();

        set_mem_pool(
            &mut trace,
            pcs,
            instructions,
            op0_addrs,
            op0s,
            dst_addrs,
            dsts,
            op1_addrs,
            op1s,
        );

        trace.table.columns()[3][0..50]
            .iter()
            .enumerate()
            .for_each(|(i, v)| println!("ROW {} - VALUE: {}", i, v));
    }

    #[test]
    fn set_bit_prefix_flags_works() {
        let program_content = std::fs::read(cairo0_program_path("fibonacci_stone.json")).unwrap();
        let mut trace: CairoTraceTable = TraceTable::allocate_with_zeros(128, 8, 16);
        let (register_states, memory, _) =
            run_program(None, CairoLayout::Plain, &program_content).unwrap();

        let (flags, _biased_offsets): (Vec<CairoInstructionFlags>, Vec<InstructionOffsets>) =
            register_states
                .flags_and_offsets(&memory)
                .unwrap()
                .into_iter()
                .unzip();

        let bit_prefix_flags: Vec<[Felt252; 16]> = flags
            .iter()
            .map(CairoInstructionFlags::to_trace_representation)
            .collect();

        set_bit_prefix_flags(&mut trace, bit_prefix_flags);

        trace.table.columns()[1][0..50]
            .iter()
            .enumerate()
            .for_each(|(i, v)| println!("ROW {} - VALUE: {}", i, v));
    }
}
