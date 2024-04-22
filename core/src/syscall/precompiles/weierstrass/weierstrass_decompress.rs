use core::borrow::{Borrow, BorrowMut};
use core::mem::size_of;
use std::fmt::Debug;

use hybrid_array::typenum::Unsigned;
use hybrid_array::Array;
use num::BigUint;
use num::Zero;
use p3_air::{Air, AirBuilder, BaseAir};
use p3_field::AbstractField;
use p3_field::PrimeField32;
use p3_matrix::dense::RowMajorMatrix;
use p3_matrix::Matrix;
use std::marker::PhantomData;
use wp1_derive::AlignedBorrow;

use crate::air::BaseAirBuilder;
use crate::air::MachineAir;
use crate::air::SP1AirBuilder;
use crate::memory::MemoryReadCols;
use crate::memory::MemoryReadWriteCols;
use crate::operations::field::field_op::FieldOpCols;
use crate::operations::field::field_op::FieldOperation;
use crate::operations::field::field_sqrt::FieldSqrtCols;
use crate::operations::field::params::WORDS_FIELD_ELEMENT;
use crate::operations::field::params::{LimbWidth, Limbs, DEFAULT_NUM_LIMBS_T};
use crate::runtime::ExecutionRecord;
use crate::runtime::Program;
use crate::runtime::SyscallCode;
use crate::utils::bytes_to_words_le_vec;
use crate::utils::ec::field::FieldParameters;
use crate::utils::ec::weierstrass::bls12381::bls12381_sqrt;
use crate::utils::ec::weierstrass::secp256k1::secp256k1_sqrt;
use crate::utils::ec::weierstrass::WeierstrassParameters;
use crate::utils::ec::{BaseLimbWidth, CurveType};
use crate::utils::ec::{EllipticCurve, WithDecompression};
use crate::utils::limbs_from_access;
use crate::utils::limbs_from_prev_access;
use crate::utils::pad_vec_rows;

/// A set of columns to compute `WeierstrassAdd` that add two points on a Weierstrass curve.
///
/// Right now the number of limbs is assumed to be a constant, although this could be macro-ed or
/// made generic in the future.
#[derive(Debug, Clone, AlignedBorrow)]
#[repr(C)]
pub struct WeierstrassDecompressCols<T, U: LimbWidth = DEFAULT_NUM_LIMBS_T> {
    pub is_real: T,
    pub shard: T,
    pub clk: T,
    pub ptr: T,
    pub is_odd: T,
    pub x_access: Array<MemoryReadCols<T>, WORDS_FIELD_ELEMENT<U>>,
    pub y_access: Array<MemoryReadWriteCols<T>, WORDS_FIELD_ELEMENT<U>>,
    pub(crate) x_2: FieldOpCols<T, U>,
    pub(crate) x_3: FieldOpCols<T, U>,
    pub(crate) x_3_plus_b: FieldOpCols<T, U>,
    pub(crate) y: FieldSqrtCols<T, U>,
    pub(crate) neg_y: FieldOpCols<T, U>,
    pub(crate) y_least_bits: [T; 8],
}

#[derive(Default)]
pub struct WeierstrassDecompressChip<E> {
    _marker: PhantomData<E>,
}

impl<E: EllipticCurve + WeierstrassParameters> WeierstrassDecompressChip<E> {
    pub fn new() -> Self {
        Self {
            _marker: PhantomData::<E>,
        }
    }

    fn populate_field_ops<F: PrimeField32>(
        cols: &mut WeierstrassDecompressCols<F, BaseLimbWidth<E>>,
        x: &BigUint,
    ) {
        // Y = sqrt(x^3 + b)
        let x_2 = cols
            .x_2
            .populate::<E::BaseField>(&x.clone(), &x.clone(), FieldOperation::Mul);
        let x_3 = cols
            .x_3
            .populate::<E::BaseField>(&x_2, x, FieldOperation::Mul);
        let b = E::b_int();
        let x_3_plus_b = cols
            .x_3_plus_b
            .populate::<E::BaseField>(&x_3, &b, FieldOperation::Add);

        let sqrt_fn = match E::CURVE_TYPE {
            CurveType::Secp256k1 => secp256k1_sqrt,
            CurveType::Bls12381 => bls12381_sqrt,
            _ => panic!("Unsupported curve"),
        };
        let y = cols.y.populate::<E::BaseField>(&x_3_plus_b, sqrt_fn);

        let zero = BigUint::zero();
        cols.neg_y
            .populate::<E::BaseField>(&zero, &y, FieldOperation::Sub);
        // Decompose bits of least significant Y byte
        let y_bytes = y.to_bytes_le();

        let y_lsb = if y_bytes.is_empty() { 0 } else { y_bytes[0] };
        for i in 0..8 {
            cols.y_least_bits[i] = F::from_canonical_u32(u32::from((y_lsb >> i) & 1));
        }
    }
}

impl<F: PrimeField32, E: EllipticCurve + WeierstrassParameters + WithDecompression> MachineAir<F>
    for WeierstrassDecompressChip<E>
{
    type Record = ExecutionRecord;
    type Program = Program;

    fn name(&self) -> String {
        match E::CURVE_TYPE {
            CurveType::Secp256k1 => "Secp256k1Decompress".to_string(),
            CurveType::Bls12381 => "Bls12381Decompress".to_string(),
            _ => panic!("Unsupported curve"),
        }
    }

    fn generate_trace(
        &self,
        input: &ExecutionRecord,
        output: &mut ExecutionRecord,
    ) -> RowMajorMatrix<F> {
        // collects the events based on the curve type.
        let events = E::decompression_events(input);

        let mut rows = Vec::new();

        let mut new_byte_lookup_events = Vec::new();

        for i in 0..events.len() {
            let event = events[i].clone();
            let mut row =
                vec![F::zero(); size_of::<WeierstrassDecompressCols<u8, BaseLimbWidth<E>>>()];
            let cols: &mut WeierstrassDecompressCols<F, BaseLimbWidth<E>> =
                row.as_mut_slice().borrow_mut();

            cols.is_real = F::from_bool(true);
            cols.shard = F::from_canonical_u32(event.shard);
            cols.clk = F::from_canonical_u32(event.clk);
            cols.ptr = F::from_canonical_u32(event.ptr);
            cols.is_odd = F::from_canonical_u32(u32::from(event.is_odd));

            let x = BigUint::from_bytes_le(&event.x_bytes);
            Self::populate_field_ops(cols, &x);

            for i in 0..cols.x_access.len() {
                cols.x_access[i].populate(event.x_memory_records[i], &mut new_byte_lookup_events);
            }
            for i in 0..cols.y_access.len() {
                cols.y_access[i]
                    .populate_write(event.y_memory_records[i], &mut new_byte_lookup_events);
            }

            rows.push(row);
        }
        output.add_byte_lookup_events(new_byte_lookup_events);

        pad_vec_rows(&mut rows, || {
            let mut row =
                vec![F::zero(); size_of::<WeierstrassDecompressCols<u8, BaseLimbWidth<E>>>()];
            let cols: &mut WeierstrassDecompressCols<F, BaseLimbWidth<E>> =
                row.as_mut_slice().borrow_mut();

            // take X of the generator as a dummy value to make sure Y^2 = X^3 + b holds
            let dummy_value = E::generator().0;
            let dummy_bytes = dummy_value.to_bytes_le();
            let words = bytes_to_words_le_vec(&dummy_bytes);
            for i in 0..cols.x_access.len() {
                cols.x_access[i].access.value = words[i].into();
            }

            Self::populate_field_ops(cols, &dummy_value);
            row
        });

        RowMajorMatrix::new(
            rows.into_iter().flatten().collect::<Vec<_>>(),
            size_of::<WeierstrassDecompressCols<u8, BaseLimbWidth<E>>>(),
        )
    }

    fn included(&self, shard: &Self::Record) -> bool {
        match E::CURVE_TYPE {
            CurveType::Secp256k1 => !shard.secp256k1_decompress_events.is_empty(),
            CurveType::Bls12381 => !shard.bls12381_decompress_events.is_empty(),
            _ => panic!("Unsupported curve"),
        }
    }
}

impl<F, E: EllipticCurve> BaseAir<F> for WeierstrassDecompressChip<E> {
    fn width(&self) -> usize {
        size_of::<WeierstrassDecompressCols<u8, BaseLimbWidth<E>>>()
    }
}

impl<AB, E: EllipticCurve + WeierstrassParameters> Air<AB> for WeierstrassDecompressChip<E>
where
    AB: SP1AirBuilder,
    Limbs<AB::Var, BaseLimbWidth<E>>: Copy,
{
    fn eval(&self, builder: &mut AB) {
        let main = builder.main();
        let row = main.row_slice(0);
        let row: &WeierstrassDecompressCols<AB::Var, BaseLimbWidth<E>> = (*row).borrow();

        let num_limbs = BaseLimbWidth::<E>::USIZE;
        let num_words_field_element = num_limbs / 4;

        builder.assert_bool(row.is_odd);

        let x: Limbs<AB::Var, BaseLimbWidth<E>> = limbs_from_prev_access(&row.x_access);
        row.x_2
            .eval::<AB, E::BaseField, _, _>(builder, &x, &x, FieldOperation::Mul);
        row.x_3
            .eval::<AB, E::BaseField, _, _>(builder, &row.x_2.result, &x, FieldOperation::Mul);
        let b = E::b_int();
        let b_const = E::BaseField::to_limbs_field::<AB::F>(&b);
        row.x_3_plus_b.eval::<AB, E::BaseField, _, _>(
            builder,
            &row.x_3.result,
            &b_const,
            FieldOperation::Add,
        );
        row.y
            .eval::<AB, E::BaseField>(builder, &row.x_3_plus_b.result);
        row.neg_y.eval::<AB, E::BaseField, _, _>(
            builder,
            &[AB::Expr::zero()].iter(),
            &row.y.multiplication.result,
            FieldOperation::Sub,
        );

        // Constrain decomposition of least significant byte of Y into `y_least_bits`
        for i in 0..8 {
            builder.when(row.is_real).assert_bool(row.y_least_bits[i]);
        }
        let y_least_byte = row.y.multiplication.result[0];
        let powers_of_two = [1, 2, 4, 8, 16, 32, 64, 128].map(AB::F::from_canonical_u32);
        let recomputed_byte: AB::Expr = row
            .y_least_bits
            .iter()
            .zip(powers_of_two)
            .map(|(p, b)| (*p).into() * b)
            .sum();
        builder
            .when(row.is_real)
            .assert_eq(recomputed_byte, y_least_byte);

        // Interpret the lowest bit of Y as whether it is odd or not.
        let y_is_odd = row.y_least_bits[0];

        let y_limbs: Limbs<AB::Var, BaseLimbWidth<E>> = limbs_from_access(&row.y_access);
        builder
            .when(row.is_real)
            .when_ne(y_is_odd, AB::Expr::one() - row.is_odd)
            .assert_all_eq(row.y.multiplication.result, y_limbs);

        builder
            .when(row.is_real)
            .when_ne(y_is_odd, row.is_odd)
            .assert_all_eq(row.neg_y.result, y_limbs);

        for i in 0..num_words_field_element {
            builder.eval_memory_access(
                row.shard,
                row.clk,
                row.ptr.into() + AB::F::from_canonical_u32((i as u32) * 4 + num_limbs as u32),
                &row.x_access[i],
                row.is_real,
            );
        }
        for i in 0..num_words_field_element {
            builder.eval_memory_access(
                row.shard,
                row.clk,
                row.ptr.into() + AB::F::from_canonical_u32((i as u32) * 4),
                &row.y_access[i],
                row.is_real,
            );
        }
        let syscall_id = match E::CURVE_TYPE {
            CurveType::Secp256k1 => {
                AB::F::from_canonical_u32(SyscallCode::SECP256K1_DECOMPRESS.syscall_id())
            }
            CurveType::Bls12381 => {
                AB::F::from_canonical_u32(SyscallCode::BLS12381_DECOMPRESS.syscall_id())
            }
            _ => panic!("Unsupported curve"),
        };

        builder.receive_syscall(
            row.shard,
            row.clk,
            syscall_id,
            row.ptr,
            row.is_odd,
            row.is_real,
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::runtime::{Instruction, Opcode, SyscallCode};
    use crate::utils::tests::{BLS_DECOMPRESS_ELF, SECP256K1_DECOMPRESS_ELF};
    use crate::utils::{
        self, bytes_to_words_be_vec, run_test_io, run_test_with_memory_inspection,
        words_to_bytes_le_vec,
    };
    use crate::Program;
    use crate::SP1Stdin;
    use bls12_381::G1Affine;
    use elliptic_curve::group::Curve;
    use elliptic_curve::sec1::ToEncodedPoint;
    use elliptic_curve::Group as _;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn bls_decompress_risc_v_program(w_ptr: u32, compressed_in: &[u8]) -> Program {
        assert_eq!(compressed_in.len(), 48);

        let sign = (compressed_in[0] & 0b_0010_0000) >> 5 == 0;
        let mut compressed = compressed_in.to_owned();
        compressed[0] &= 0b_0001_1111;

        let mut instructions = vec![];

        let mut words =
            bytes_to_words_be_vec([compressed.as_slice(), &[0u8; 48]].concat().as_slice());
        words.reverse();

        for i in 0..words.len() {
            instructions.push(Instruction::new(Opcode::ADD, 29, 0, words[i], false, true));
            instructions.push(Instruction::new(
                Opcode::ADD,
                30,
                0,
                w_ptr + (i as u32) * 4,
                false,
                true,
            ));
            instructions.push(Instruction::new(Opcode::SW, 29, 30, 0, false, true));
        }

        instructions.extend(vec![
            Instruction::new(
                Opcode::ADD,
                5,
                0,
                SyscallCode::BLS12381_DECOMPRESS as u32,
                false,
                true,
            ),
            Instruction::new(Opcode::ADD, 10, 0, w_ptr, false, true),
            Instruction::new(Opcode::ADD, 11, 0, u32::from(sign), false, true),
            Instruction::new(Opcode::ECALL, 5, 10, 11, false, false),
        ]);
        Program::new(instructions, 0, 0)
    }

    const CANDIDATES: [[u8; 48]; 4] = [
        [
            128, 181, 135, 148, 52, 27, 78, 148, 13, 235, 10, 222, 148, 47, 2, 89, 248, 37, 76, 33,
            223, 74, 74, 102, 121, 191, 228, 14, 144, 134, 65, 196, 196, 179, 29, 52, 188, 151,
            130, 217, 19, 140, 56, 237, 23, 143, 187, 17,
        ],
        [
            166, 149, 173, 50, 93, 252, 126, 17, 145, 251, 201, 241, 134, 245, 142, 255, 66, 166,
            52, 2, 151, 49, 177, 131, 128, 255, 137, 191, 66, 196, 100, 164, 44, 184, 202, 85, 178,
            0, 240, 81, 245, 127, 30, 24, 147, 198, 135, 89,
        ],
        [
            179, 44, 55, 73, 219, 90, 162, 144, 118, 142, 170, 188, 197, 226, 44, 223, 102, 32,
            166, 101, 39, 215, 91, 115, 175, 209, 23, 20, 243, 170, 185, 166, 196, 140, 186, 162,
            114, 52, 88, 7, 0, 214, 47, 175, 129, 52, 248, 110,
        ],
        [
            128, 183, 213, 204, 76, 81, 8, 121, 165, 14, 143, 54, 218, 155, 196, 74, 62, 142, 33,
            208, 87, 222, 166, 154, 164, 110, 63, 127, 138, 93, 182, 225, 19, 233, 159, 107, 33,
            26, 109, 200, 54, 243, 158, 202, 205, 126, 190, 5,
        ],
    ];

    // TODO: figure out why at some inputs this test fails
    #[test]
    fn test_weierstrass_bls_decompress_risc_v_program() {
        utils::setup_logger();

        // TODO: make this work on the last points CANDIDATES[2..]
        for compressed_g1 in &CANDIDATES[..2] {
            // use bls12_381 crate to compute expected value
            let mut expected = G1Affine::from_compressed(compressed_g1)
                .unwrap()
                .to_uncompressed();
            expected[0] &= 0b_0001_1111;

            let memory_pointer = 100u32;
            let program = bls_decompress_risc_v_program(memory_pointer, compressed_g1.as_ref());
            let (_, memory) = run_test_with_memory_inspection(program);

            let mut decompressed_g1 = vec![];
            // decompressed G1 occupies 96 bytes or 24 words (8 bytes each): 96 / 8 = 24
            for i in 0..24 {
                decompressed_g1.push(memory.get(&(memory_pointer + i * 4)).unwrap().value);
            }

            let mut decompressed_g1 = words_to_bytes_le_vec(&decompressed_g1);
            decompressed_g1.reverse();

            assert_eq!(
                decompressed_g1,
                expected.to_vec(),
                "Failed on {:?}",
                compressed_g1
            );
        }
    }

    #[test]
    fn test_weierstrass_secp256k1_decompress() {
        utils::setup_logger();

        let mut rng = StdRng::seed_from_u64(2);

        let secret_key = k256::SecretKey::random(&mut rng);
        let public_key = secret_key.public_key();
        let encoded = public_key.to_encoded_point(false);
        let decompressed = encoded.as_bytes();
        let compressed = public_key.to_sec1_bytes();

        let inputs = SP1Stdin::from(&compressed);

        let mut proof = run_test_io(Program::from(SECP256K1_DECOMPRESS_ELF), inputs).unwrap();
        let mut result = [0; 65];
        proof.public_values.read_slice(&mut result);
        assert_eq!(result, decompressed);
    }

    #[test]
    fn test_weierstrass_bls12381_decompress() {
        utils::setup_logger();

        let mut rng = StdRng::seed_from_u64(2);

        let point = bls12_381::G1Projective::random(&mut rng);
        let pt_affine = point.to_affine();
        let pt_compressed = pt_affine.to_compressed();
        let pt_uncompressed = pt_affine.to_uncompressed();

        let inputs = SP1Stdin::from(&pt_compressed[..]);

        let mut proof = run_test_io(Program::from(BLS_DECOMPRESS_ELF), inputs).unwrap();
        let mut result = [0; 96];
        proof.public_values.read_slice(&mut result);
        assert_eq!(result, pt_uncompressed);
    }

    #[test]
    fn test_weierstrass_bls12381_decompress_candidates() {
        utils::setup_logger();

        // TODO: figure out how to make this work on the last points CANDIDATES[2..]
        for candidate in &CANDIDATES[..2] {
            let pt_compressed = candidate;
            let pt_affine = bls12_381::G1Affine::from_compressed(candidate).unwrap();
            let pt_uncompressed = pt_affine.to_uncompressed();

            let inputs = SP1Stdin::from(&pt_compressed[..]);

            let mut proof = run_test_io(Program::from(BLS_DECOMPRESS_ELF), inputs).unwrap();
            let mut result = [0; 96];
            proof.public_values.read_slice(&mut result);
            assert_eq!(result, pt_uncompressed);
        }
    }

    #[test]
    fn test_weierstrass_bls12381_decompress_infinity_point_elf() {
        utils::setup_logger();

        let compressed = G1Affine::identity().to_compressed();
        let expected = G1Affine::from_compressed(&compressed)
            .unwrap()
            .to_uncompressed();

        let mut proof = run_test_io(
            Program::from(BLS_DECOMPRESS_ELF),
            SP1Stdin::from(&compressed),
        )
        .unwrap();
        let mut result = [0; 96];
        proof.public_values.read_slice(&mut result);

        assert_eq!(expected, result);
    }
}