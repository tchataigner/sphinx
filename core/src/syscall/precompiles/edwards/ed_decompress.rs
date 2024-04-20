use core::{
    borrow::{Borrow, BorrowMut},
    mem::size_of,
};
use std::marker::PhantomData;

use curve25519_dalek::edwards::CompressedEdwardsY;
use hybrid_array::{typenum::Unsigned, Array};
use num::{BigUint, One, Zero};
use p3_air::{Air, AirBuilder, BaseAir};
use p3_field::{AbstractField, PrimeField32};
use p3_matrix::{dense::RowMajorMatrix, Matrix};
use serde::{Deserialize, Serialize};
use wp1_derive::AlignedBorrow;

use crate::air::BaseAirBuilder;
use crate::air::MachineAir;
use crate::air::SP1AirBuilder;
use crate::bytes::event::ByteRecord;
use crate::bytes::ByteLookupEvent;
use crate::memory::MemoryReadCols;
use crate::memory::MemoryWriteCols;
use crate::operations::field::field_op::FieldOpCols;
use crate::operations::field::field_op::FieldOperation;
use crate::operations::field::field_sqrt::FieldSqrtCols;
use crate::operations::field::params::FieldParameters;
use crate::operations::field::params::Limbs;
use crate::operations::field::params::BYTES_COMPRESSED_CURVEPOINT;
use crate::operations::field::params::BYTES_FIELD_ELEMENT;
use crate::operations::field::params::WORDS_FIELD_ELEMENT;
use crate::operations::field::range::FieldRangeCols;
use crate::runtime::ExecutionRecord;
use crate::runtime::MemoryReadRecord;
use crate::runtime::MemoryWriteRecord;
use crate::runtime::Program;
use crate::runtime::Syscall;
use crate::runtime::SyscallCode;
use crate::syscall::precompiles::LimbWidth;
use crate::syscall::precompiles::SyscallContext;
use crate::syscall::precompiles::DEFAULT_NUM_LIMBS_T;
use crate::utils::bytes_to_words_le;
use crate::utils::ec::edwards::ed25519::decompress;
use crate::utils::ec::edwards::ed25519::ed25519_sqrt;
use crate::utils::ec::edwards::EdwardsParameters;
use crate::utils::ec::BaseLimbWidth;
use crate::utils::limbs_from_access;
use crate::utils::limbs_from_prev_access;
use crate::utils::pad_vec_rows;
use crate::utils::words_to_bytes_le;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdDecompressEvent<U: LimbWidth = DEFAULT_NUM_LIMBS_T> {
    pub shard: u32,
    pub clk: u32,
    pub ptr: u32,
    pub sign: bool,
    #[serde(with = "crate::utils::array_serde::ArraySerde")]
    pub y_bytes: Array<u8, BYTES_COMPRESSED_CURVEPOINT<U>>,
    #[serde(with = "crate::utils::array_serde::ArraySerde")]
    pub decompressed_x_bytes: Array<u8, BYTES_FIELD_ELEMENT<U>>,
    #[serde(with = "crate::utils::array_serde::ArraySerde")]
    pub x_memory_records: Array<MemoryWriteRecord, WORDS_FIELD_ELEMENT<U>>,
    #[serde(with = "crate::utils::array_serde::ArraySerde")]
    pub y_memory_records: Array<MemoryReadRecord, WORDS_FIELD_ELEMENT<U>>,
}

/// A set of columns to compute `EdDecompress` given a pointer to a 16 word slice formatted as such
/// for a 32-byte base field representation:
///
/// The 31st byte of the slice is the sign bit. The second half of the slice is the 255-bit
/// compressed Y (without sign bit).
///
/// After `EdDecompress`, the first 32 bytes of the slice are overwritten with the decompressed X.
#[derive(Debug, Clone, AlignedBorrow)]
#[repr(C)]
pub struct EdDecompressCols<T, P: FieldParameters> {
    pub is_real: T,
    pub shard: T,
    pub clk: T,
    pub ptr: T,
    pub sign: T,
    pub x_access: Array<MemoryWriteCols<T>, WORDS_FIELD_ELEMENT<P::NB_LIMBS>>,
    pub y_access: Array<MemoryReadCols<T>, WORDS_FIELD_ELEMENT<P::NB_LIMBS>>,
    pub(crate) y_range: FieldRangeCols<T, P>,
    pub(crate) yy: FieldOpCols<T, P>,
    pub(crate) u: FieldOpCols<T, P>,
    pub(crate) dyy: FieldOpCols<T, P>,
    pub(crate) v: FieldOpCols<T, P>,
    pub(crate) u_div_v: FieldOpCols<T, P>,
    pub(crate) x: FieldSqrtCols<T, P>,
    pub(crate) neg_x: FieldOpCols<T, P>,
}

impl<F: PrimeField32, P: FieldParameters> EdDecompressCols<F, P> {
    pub fn populate<E: EdwardsParameters<BaseField = P>>(
        &mut self,
        event: &EdDecompressEvent,
        record: &mut ExecutionRecord,
    ) {
        let mut new_byte_lookup_events = Vec::new();
        self.is_real = F::from_bool(true);
        self.shard = F::from_canonical_u32(event.shard);
        self.clk = F::from_canonical_u32(event.clk);
        self.ptr = F::from_canonical_u32(event.ptr);
        self.sign = F::from_bool(event.sign);
        let nw_field_elt = WORDS_FIELD_ELEMENT::<P::NB_LIMBS>::USIZE;
        for i in 0..nw_field_elt {
            self.x_access[i].populate(event.x_memory_records[i], &mut new_byte_lookup_events);
            self.y_access[i].populate(event.y_memory_records[i], &mut new_byte_lookup_events);
        }

        let y = &BigUint::from_bytes_le(&event.y_bytes);
        self.populate_field_ops::<E>(&mut new_byte_lookup_events, event.shard, y);

        record.add_byte_lookup_events(new_byte_lookup_events);
    }

    fn populate_field_ops<E: EdwardsParameters>(
        &mut self,
        blu_events: &mut Vec<ByteLookupEvent>,
        shard: u32,
        y: &BigUint,
    ) {
        let one = BigUint::one();
        self.y_range.populate(blu_events, shard, y);
        let yy = self
            .yy
            .populate(blu_events, shard, y, y, FieldOperation::Mul);
        let u = self
            .u
            .populate(blu_events, shard, &yy, &one, FieldOperation::Sub);
        let dyy = self
            .dyy
            .populate(blu_events, shard, &E::d_biguint(), &yy, FieldOperation::Mul);
        let v = self
            .v
            .populate(blu_events, shard, &one, &dyy, FieldOperation::Add);
        let u_div_v = self
            .u_div_v
            .populate(blu_events, shard, &u, &v, FieldOperation::Div);
        let x = self.x.populate(blu_events, shard, &u_div_v, ed25519_sqrt);
        self.neg_x
            .populate(blu_events, shard, &BigUint::zero(), &x, FieldOperation::Sub);
    }
}

impl<V: Copy, P: FieldParameters> EdDecompressCols<V, P> {
    pub fn eval<AB: SP1AirBuilder<Var = V>, E: EdwardsParameters<BaseField = P>>(
        &self,
        builder: &mut AB,
    ) where
        V: Into<AB::Expr>,
    {
        builder.assert_bool(self.sign);

        let y: Limbs<V, P::NB_LIMBS> = limbs_from_prev_access(&self.y_access);
        self.y_range.eval(builder, &y, self.shard, self.is_real);
        self.yy.eval(
            builder,
            &y,
            &y,
            FieldOperation::Mul,
            self.shard,
            self.is_real,
        );
        self.u.eval(
            builder,
            &self.yy.result,
            &[AB::Expr::one()].iter(),
            FieldOperation::Sub,
            self.shard,
            self.is_real,
        );
        let d_biguint = E::d_biguint();
        let d_const = E::BaseField::to_limbs_field::<AB::F>(&d_biguint);
        self.dyy.eval(
            builder,
            &d_const,
            &self.yy.result,
            FieldOperation::Mul,
            self.shard,
            self.is_real,
        );
        self.v.eval(
            builder,
            &[AB::Expr::one()].iter(),
            &self.dyy.result,
            FieldOperation::Add,
            self.shard,
            self.is_real,
        );
        self.u_div_v.eval(
            builder,
            &self.u.result,
            &self.v.result,
            FieldOperation::Div,
            self.shard,
            self.is_real,
        );
        self.x.eval(
            builder,
            &self.u_div_v.result,
            AB::F::zero(),
            self.shard,
            self.is_real,
        );
        self.neg_x.eval(
            builder,
            &[AB::Expr::zero()].iter(),
            &self.x.multiplication.result,
            FieldOperation::Sub,
            self.shard,
            self.is_real,
        );

        for i in 0..WORDS_FIELD_ELEMENT::<P::NB_LIMBS>::USIZE {
            builder.eval_memory_access(
                self.shard,
                self.clk,
                self.ptr.into() + AB::F::from_canonical_u32((i as u32) * 4),
                &self.x_access[i],
                self.is_real,
            );
        }
        for i in 0..WORDS_FIELD_ELEMENT::<P::NB_LIMBS>::USIZE {
            builder.eval_memory_access(
                self.shard,
                self.clk,
                self.ptr.into() + AB::F::from_canonical_u32((i as u32) * 4 + 32),
                &self.y_access[i],
                self.is_real,
            );
        }

        // Constrain that the correct result is written into x.
        let x_limbs: Limbs<_, P::NB_LIMBS> = limbs_from_access(&self.x_access);
        builder
            .when(self.is_real)
            .when(self.sign)
            .assert_all_eq(self.neg_x.result.clone(), x_limbs.clone());
        builder
            .when(self.is_real)
            .when_not(self.sign)
            .assert_all_eq(self.x.multiplication.result.clone(), x_limbs);

        builder.receive_syscall(
            self.shard,
            self.clk,
            AB::F::from_canonical_u32(SyscallCode::ED_DECOMPRESS.syscall_id()),
            self.ptr,
            self.sign,
            self.is_real,
        );
    }
}

#[derive(Default)]
pub struct EdDecompressChip<E> {
    _phantom: PhantomData<E>,
}

// TODO(FG): This function is already generic in NB_LIMBS, but the ed_decompress_events record is not
impl<F: FieldParameters<NB_LIMBS = DEFAULT_NUM_LIMBS_T>, E: EdwardsParameters<BaseField = F>>
    Syscall for EdDecompressChip<E>
{
    fn execute(&self, rt: &mut SyscallContext<'_>, arg1: u32, sign: u32) -> Option<u32> {
        let start_clk = rt.clk;
        let slice_ptr = arg1;
        assert!(slice_ptr % 4 == 0, "Pointer must be 4-byte aligned.");
        assert!(sign <= 1, "Sign bit must be 0 or 1.");
        let sign = sign as u8;

        let (y_memory_records_vec, y_vec) = rt.mr_slice(
            slice_ptr + (BYTES_COMPRESSED_CURVEPOINT::<BaseLimbWidth<E>>::USIZE as u32),
            WORDS_FIELD_ELEMENT::<BaseLimbWidth<E>>::USIZE,
        );
        let y_memory_records: Array<MemoryReadRecord, WORDS_FIELD_ELEMENT<BaseLimbWidth<E>>> =
            (&y_memory_records_vec[..]).try_into().unwrap();

        let sign_bool = sign != 0;

        let y_bytes = words_to_bytes_le::<BYTES_COMPRESSED_CURVEPOINT<BaseLimbWidth<E>>>(&y_vec);

        // Copy bytes into another array so we can modify the last byte and make CompressedEdwardsY,
        // which we'll use to compute the expected X.
        // Re-insert sign bit into last bit of Y for CompressedEdwardsY format
        let mut compressed_edwards_y = y_bytes;
        compressed_edwards_y[compressed_edwards_y.len() - 1] &= 0b0111_1111;
        compressed_edwards_y[compressed_edwards_y.len() - 1] |= sign << 7;

        // Compute actual decompressed X
        let compressed_y = CompressedEdwardsY(compressed_edwards_y);
        let decompressed = decompress(&compressed_y);

        let bytes_field_elt = BYTES_FIELD_ELEMENT::<BaseLimbWidth<E>>::USIZE;
        let mut decompressed_x_bytes = decompressed.x.to_bytes_le();
        decompressed_x_bytes.resize(bytes_field_elt, 0u8);

        let decompressed_x_words =
            bytes_to_words_le::<WORDS_FIELD_ELEMENT<BaseLimbWidth<E>>>(&decompressed_x_bytes);

        // Write decompressed X into slice
        let x_memory_records_vec = rt.mw_slice(slice_ptr, &decompressed_x_words);
        let x_memory_records: Array<MemoryWriteRecord, WORDS_FIELD_ELEMENT<BaseLimbWidth<E>>> =
            (&x_memory_records_vec[..]).try_into().unwrap();

        let shard = rt.current_shard();
        rt.record_mut()
            .ed_decompress_events
            .push(EdDecompressEvent {
                shard,
                clk: start_clk,
                ptr: slice_ptr,
                sign: sign_bool,
                y_bytes: y_bytes.into(),
                decompressed_x_bytes: (&decompressed_x_bytes[..]).try_into().unwrap(),
                x_memory_records,
                y_memory_records,
            });
        None
    }

    fn num_extra_cycles(&self) -> u32 {
        0
    }
}

impl<E: EdwardsParameters> EdDecompressChip<E> {
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<F: PrimeField32, E: EdwardsParameters> MachineAir<F> for EdDecompressChip<E> {
    type Record = ExecutionRecord;

    type Program = Program;

    fn name(&self) -> String {
        "EdDecompress".to_string()
    }

    fn generate_trace(
        &self,
        input: &ExecutionRecord,
        output: &mut ExecutionRecord,
    ) -> RowMajorMatrix<F> {
        let mut rows = Vec::new();

        for i in 0..input.ed_decompress_events.len() {
            let event = &input.ed_decompress_events[i];
            let mut row = vec![F::zero(); size_of::<EdDecompressCols<u8, E::BaseField>>()];
            let cols: &mut EdDecompressCols<F, E::BaseField> = row.as_mut_slice().borrow_mut();
            cols.populate::<E>(event, output);

            rows.push(row);
        }

        pad_vec_rows(&mut rows, || {
            let mut row = vec![F::zero(); size_of::<EdDecompressCols<u8, E::BaseField>>()];
            let cols: &mut EdDecompressCols<F, E::BaseField> = row.as_mut_slice().borrow_mut();
            let zero = BigUint::zero();
            cols.populate_field_ops::<E>(&mut vec![], 0, &zero);
            row
        });

        RowMajorMatrix::new(
            rows.into_iter().flatten().collect::<Vec<_>>(),
            size_of::<EdDecompressCols<u8, E::BaseField>>(),
        )
    }

    fn included(&self, shard: &Self::Record) -> bool {
        !shard.ed_decompress_events.is_empty()
    }
}

impl<F, E: EdwardsParameters> BaseAir<F> for EdDecompressChip<E> {
    fn width(&self) -> usize {
        size_of::<EdDecompressCols<u8, E::BaseField>>()
    }
}

impl<AB, E: EdwardsParameters> Air<AB> for EdDecompressChip<E>
where
    AB: SP1AirBuilder,
{
    fn eval(&self, builder: &mut AB) {
        let main = builder.main();
        let row = main.row_slice(0);
        let row: &EdDecompressCols<AB::Var, E::BaseField> = (*row).borrow();
        row.eval::<AB, E>(builder);
    }
}

#[cfg(test)]
pub mod tests {
    use crate::{
        runtime::Program,
        utils::{
            tests::ED_DECOMPRESS_ELF,
            {self},
        },
    };

    #[test]
    fn test_ed_decompress() {
        utils::setup_logger();
        let program = Program::from(ED_DECOMPRESS_ELF);
        utils::run_test(program).unwrap();
    }
}
