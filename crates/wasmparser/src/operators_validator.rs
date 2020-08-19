/* Copyright 2019 Mozilla Foundation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::limits::MAX_WASM_FUNCTION_LOCALS;
use crate::primitives::{MemoryImmediate, Operator, SIMDLaneIndex, Type, TypeOrFuncType};
use crate::{BinaryReaderError, Result, WasmFeatures, WasmFuncType, WasmModuleResources};
use std::cmp::min;

#[derive(Debug)]
struct BlockState {
    start_types: Vec<Type>,
    return_types: Vec<Type>,
    // Position in `FuncState::stack_types` array where block values
    // start.
    stack_starts_at: usize,
    // True for loop.
    jump_to_top: bool,
    is_else_allowed: bool,
    is_dead_code: bool,
    // Amount of the required polymorphic values at the stack_starts_at
    // position in `FuncState::stack_types` array. These values are
    // fictitious and are not actually present in the stack_types.
    polymorphic_values: Option<usize>,
}

impl BlockState {
    fn is_stack_polymorphic(&self) -> bool {
        self.polymorphic_values.is_some()
    }
}

#[derive(Debug)]
struct FuncState {
    local_types: Vec<Type>,
    blocks: Vec<BlockState>,
    stack_types: Vec<Type>,
    end_function: bool,
}

impl FuncState {
    fn block_at(&self, depth: usize) -> &BlockState {
        assert!(depth < self.blocks.len());
        &self.blocks[self.blocks.len() - 1 - depth]
    }
    fn last_block(&self) -> &BlockState {
        self.blocks.last().unwrap()
    }
    fn stack_type_at(&self, index: usize) -> Option<Type> {
        let stack_starts_at = self.last_block().stack_starts_at;
        if self.last_block().is_stack_polymorphic()
            && stack_starts_at + index >= self.stack_types.len()
        {
            return None;
        }
        assert!(stack_starts_at + index < self.stack_types.len());
        Some(self.stack_types[self.stack_types.len() - 1 - index])
    }
    fn assert_stack_type_at(&self, index: usize, expected: Type) -> bool {
        match self.stack_type_at(index) {
            Some(ty) => ty == expected,
            None => true,
        }
    }
    fn assert_block_stack_len(&self, depth: usize, minimal_len: usize) -> bool {
        assert!(depth < self.blocks.len());
        let blocks_end = self.blocks.len();
        let block_offset = blocks_end - 1 - depth;
        for i in block_offset..blocks_end {
            if self.blocks[i].is_stack_polymorphic() {
                return true;
            }
        }
        let block_starts_at = self.blocks[block_offset].stack_starts_at;
        self.stack_types.len() >= block_starts_at + minimal_len
    }
    fn assert_last_block_stack_len_exact(&self, len: usize) -> bool {
        let block_starts_at = self.last_block().stack_starts_at;
        if self.last_block().is_stack_polymorphic() {
            let polymorphic_values = self.last_block().polymorphic_values.unwrap();
            self.stack_types.len() + polymorphic_values <= block_starts_at + len
        } else {
            self.stack_types.len() == block_starts_at + len
        }
    }
    fn remove_frame_stack_types(&mut self, remove_count: usize) -> OperatorValidatorResult<()> {
        if remove_count == 0 {
            return Ok(());
        }
        let last_block = self.blocks.last_mut().unwrap();
        if last_block.is_stack_polymorphic() {
            let len = self.stack_types.len();
            let remove_non_polymorphic = len
                .checked_sub(last_block.stack_starts_at)
                .ok_or_else(|| OperatorValidatorError::new("invalid block signature"))?
                .min(remove_count);
            self.stack_types.truncate(len - remove_non_polymorphic);
            let polymorphic_values = last_block.polymorphic_values.unwrap();
            let remove_polymorphic = min(remove_count - remove_non_polymorphic, polymorphic_values);
            last_block.polymorphic_values = Some(polymorphic_values - remove_polymorphic);
        } else {
            assert!(self.stack_types.len() >= last_block.stack_starts_at + remove_count);
            let keep = self.stack_types.len() - remove_count;
            self.stack_types.truncate(keep);
        }
        Ok(())
    }
    fn push_block(
        &mut self,
        ty: TypeOrFuncType,
        block_type: BlockType,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<()> {
        let (start_types, return_types) = match ty {
            TypeOrFuncType::Type(Type::EmptyBlockType) => (vec![], vec![]),
            TypeOrFuncType::Type(ty) => (vec![], vec![ty]),
            TypeOrFuncType::FuncType(idx) => {
                let ty = func_type_at(&resources, idx)?;
                (
                    ty.inputs().collect::<Vec<_>>(),
                    ty.outputs().collect::<Vec<_>>(),
                )
            }
        };
        if block_type == BlockType::If {
            // Collect conditional value from the stack_types.
            let last_block = self.blocks.last().unwrap();
            if !last_block.is_stack_polymorphic()
                || self.stack_types.len() > last_block.stack_starts_at
            {
                self.stack_types.pop();
            }
            assert!(self.stack_types.len() >= last_block.stack_starts_at);
        }
        for (i, ty) in start_types.iter().rev().enumerate() {
            if !self.assert_stack_type_at(i, *ty) {
                return Err(OperatorValidatorError::new("stack operand type mismatch"));
            }
        }
        let (stack_starts_at, polymorphic_values) = {
            // When stack for last block is polymorphic, ensure that
            // the polymorphic_values matches, and next block is informed about that.
            let last_block = self.blocks.last_mut().unwrap();
            if !last_block.is_stack_polymorphic()
                || last_block.stack_starts_at + start_types.len() <= self.stack_types.len()
            {
                (self.stack_types.len() - start_types.len(), None)
            } else {
                let unknown_stack_types_len =
                    last_block.stack_starts_at + start_types.len() - self.stack_types.len();
                (last_block.stack_starts_at, Some(unknown_stack_types_len))
            }
        };
        self.blocks.push(BlockState {
            start_types,
            return_types,
            stack_starts_at,
            jump_to_top: block_type == BlockType::Loop,
            is_else_allowed: block_type == BlockType::If,
            is_dead_code: false,
            polymorphic_values,
        });
        Ok(())
    }
    fn pop_block(&mut self) {
        assert!(self.blocks.len() > 1);
        let last_block = self.blocks.pop().unwrap();
        if last_block.is_stack_polymorphic() {
            assert!(
                self.stack_types.len()
                    <= last_block.return_types.len() + last_block.stack_starts_at
            );
        } else {
            assert!(
                self.stack_types.len()
                    == last_block.return_types.len() + last_block.stack_starts_at
            );
        }
        let keep = last_block.stack_starts_at;
        self.stack_types.truncate(keep);
        self.stack_types.extend_from_slice(&last_block.return_types);
    }
    fn reset_block(&mut self) {
        assert!(self.last_block().is_else_allowed);
        let last_block = self.blocks.last_mut().unwrap();
        let keep = last_block.stack_starts_at;
        self.stack_types.truncate(keep);
        self.stack_types
            .extend(last_block.start_types.iter().cloned());
        last_block.is_else_allowed = false;
        last_block.polymorphic_values = None;
    }
    fn change_frame(&mut self, remove_count: usize) -> OperatorValidatorResult<()> {
        self.remove_frame_stack_types(remove_count)
    }
    fn change_frame_with_type(
        &mut self,
        remove_count: usize,
        ty: Type,
    ) -> OperatorValidatorResult<()> {
        self.remove_frame_stack_types(remove_count)?;
        self.stack_types.push(ty);
        Ok(())
    }
    fn change_frame_with_types<I>(
        &mut self,
        remove_count: usize,
        new_items: I,
    ) -> OperatorValidatorResult<()>
    where
        I: Iterator<Item = Type>,
    {
        self.remove_frame_stack_types(remove_count)?;
        self.stack_types.extend(new_items);
        Ok(())
    }
    fn change_frame_to_exact_types_from(&mut self, depth: usize) -> OperatorValidatorResult<()> {
        let types = self.block_at(depth).return_types.clone();
        let last_block = self.blocks.last_mut().unwrap();
        let keep = last_block.stack_starts_at;
        if keep + types.len() <= self.stack_types.len() {
            // Have enough operands on stack, validation is done at `check_jump_from_block`.
            return Ok(());
        }
        let polymorphic_values_used = keep + types.len() - self.stack_types.len();
        self.stack_types.truncate(keep);
        self.stack_types.extend_from_slice(&types);
        // Keep polymorphic stack.
        let polymorphic_values = last_block.polymorphic_values.as_mut().unwrap();
        *polymorphic_values = polymorphic_values.saturating_sub(polymorphic_values_used);
        Ok(())
    }
    fn change_frame_after_select(&mut self, ty: Option<Type>) -> OperatorValidatorResult<()> {
        self.remove_frame_stack_types(3)?;
        if ty.is_none() {
            let last_block = self.blocks.last_mut().unwrap();
            assert!(last_block.is_stack_polymorphic());
            last_block.polymorphic_values = Some(last_block.polymorphic_values.unwrap() + 1);
            return Ok(());
        }
        self.stack_types.push(ty.unwrap());
        Ok(())
    }
    fn start_dead_code(&mut self) {
        let last_block = self.blocks.last_mut().unwrap();
        let keep = last_block.stack_starts_at;
        self.stack_types.truncate(keep);
        last_block.is_dead_code = true;
        last_block.polymorphic_values = Some(0);
    }
    fn end_function(&mut self) {
        self.end_function = true;
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum BlockType {
    Block,
    Loop,
    If,
}

pub enum FunctionEnd {
    No,
    Yes,
}

/// A wrapper around a `BinaryReaderError` where the inner error's offset is a
/// temporary placeholder value. This can be converted into a proper
/// `BinaryReaderError` via the `set_offset` method, which replaces the
/// placeholder offset with an actual offset.
pub(crate) struct OperatorValidatorError(pub(crate) BinaryReaderError);

/// Create an `OperatorValidatorError` with a format string.
macro_rules! format_op_err {
    ( $( $arg:expr ),* $(,)* ) => {
        OperatorValidatorError::new(format!( $( $arg ),* ))
    }
}

/// Early return an `Err(OperatorValidatorError)` with a format string.
macro_rules! bail_op_err {
    ( $( $arg:expr ),* $(,)* ) => {
        return Err(format_op_err!( $( $arg ),* ));
    }
}

impl OperatorValidatorError {
    /// Create a new `OperatorValidatorError` with a placeholder offset.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let offset = std::usize::MAX;
        let e = BinaryReaderError::new(message, offset);
        OperatorValidatorError(e)
    }

    /// Convert this `OperatorValidatorError` into a `BinaryReaderError` by
    /// supplying an actual offset to replace the internal placeholder offset.
    pub(crate) fn set_offset(mut self, offset: usize) -> BinaryReaderError {
        debug_assert_eq!(self.0.inner.offset, std::usize::MAX);
        self.0.inner.offset = offset;
        self.0
    }
}

type OperatorValidatorResult<T> = std::result::Result<T, OperatorValidatorError>;

#[derive(Debug)]
pub(crate) struct OperatorValidator {
    func_state: FuncState,
    features: WasmFeatures,
}

impl OperatorValidator {
    pub fn new(func_type: impl WasmFuncType, features: &WasmFeatures) -> OperatorValidator {
        let local_types = func_type.inputs().collect::<Vec<_>>();
        let mut blocks = Vec::new();
        let last_returns = func_type.outputs().collect::<Vec<_>>();
        blocks.push(BlockState {
            start_types: vec![],
            return_types: last_returns,
            stack_starts_at: 0,
            jump_to_top: false,
            is_else_allowed: false,
            is_dead_code: false,
            polymorphic_values: None,
        });

        OperatorValidator {
            func_state: FuncState {
                local_types,
                blocks,
                stack_types: Vec::new(),
                end_function: false,
            },
            features: *features,
        }
    }
    pub fn define_locals(&mut self, offset: usize, count: u32, ty: Type) -> Result<()> {
        if (MAX_WASM_FUNCTION_LOCALS - self.func_state.local_types.len())
            .checked_sub(count as usize)
            .is_none()
        {
            if count
                .checked_add(self.func_state.local_types.len() as u32)
                .is_none()
            {
                return Err(BinaryReaderError::new("locals overflow", offset));
            } else {
                return Err(BinaryReaderError::new("locals exceed maximum", offset));
            }
        }
        self.features
            .check_value_type(ty)
            .map_err(|e| BinaryReaderError::new(e, offset))?;
        for _ in 0..count {
            self.func_state.local_types.push(ty);
        }
        Ok(())
    }

    fn check_frame_size(&self, require_count: usize) -> OperatorValidatorResult<()> {
        if !self.func_state.assert_block_stack_len(0, require_count) {
            Err(OperatorValidatorError::new(
                "type mismatch: not enough operands",
            ))
        } else {
            Ok(())
        }
    }

    fn check_operands_1(&self, operand: Type) -> OperatorValidatorResult<()> {
        self.check_frame_size(1)?;
        if !self.func_state.assert_stack_type_at(0, operand) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        Ok(())
    }

    fn check_operands_2(&self, operand1: Type, operand2: Type) -> OperatorValidatorResult<()> {
        self.check_frame_size(2)?;
        if !self.func_state.assert_stack_type_at(1, operand1) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        if !self.func_state.assert_stack_type_at(0, operand2) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        Ok(())
    }

    fn check_operands_3(
        &self,
        operand1: Type,
        operand2: Type,
        operand3: Type,
    ) -> OperatorValidatorResult<()> {
        self.check_frame_size(3)?;
        if !self.func_state.assert_stack_type_at(2, operand1) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        if !self.func_state.assert_stack_type_at(1, operand2) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        if !self.func_state.assert_stack_type_at(0, operand3) {
            return Err(OperatorValidatorError::new("stack operand type mismatch"));
        }
        Ok(())
    }

    fn check_operands<I>(&self, expected_types: I) -> OperatorValidatorResult<()>
    where
        I: ExactSizeIterator<Item = Type>,
    {
        let len = expected_types.len();
        self.check_frame_size(len)?;
        for (i, expected_type) in expected_types.enumerate() {
            if !self
                .func_state
                .assert_stack_type_at(len - 1 - i, expected_type)
            {
                return Err(OperatorValidatorError::new("stack operand type mismatch"));
            }
        }
        Ok(())
    }

    fn check_block_return_types(
        &self,
        block: &BlockState,
        reserve_items: usize,
    ) -> OperatorValidatorResult<()> {
        if !self.features.multi_value && block.return_types.len() > 1 {
            return Err(OperatorValidatorError::new(
                "blocks, loops, and ifs may only return at most one \
                 value when multi-value is not enabled",
            ));
        }
        let len = block.return_types.len();
        for i in 0..len {
            if !self
                .func_state
                .assert_stack_type_at(len - 1 - i + reserve_items, block.return_types[i])
            {
                return Err(OperatorValidatorError::new(
                    "type mismatch: stack item type does not match block item type",
                ));
            }
        }
        Ok(())
    }

    fn check_block_return(&self) -> OperatorValidatorResult<()> {
        let len = self.func_state.last_block().return_types.len();
        if !self.func_state.assert_last_block_stack_len_exact(len) {
            return Err(OperatorValidatorError::new(
                "type mismatch: stack size does not match block type",
            ));
        }
        self.check_block_return_types(self.func_state.last_block(), 0)
    }

    fn check_call(
        &mut self,
        function_index: u32,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<()> {
        let ty = match resources.type_of_function(function_index) {
            Some(i) => i,
            None => {
                bail_op_err!(
                    "unknown function {}: function index out of bounds",
                    function_index
                );
            }
        };
        self.check_operands(ty.inputs())?;
        self.func_state
            .change_frame_with_types(ty.len_inputs(), ty.outputs())?;
        Ok(())
    }

    fn check_call_indirect(
        &mut self,
        index: u32,
        table_index: u32,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<()> {
        if resources.table_at(table_index).is_none() {
            return Err(OperatorValidatorError::new(
                "unknown table: table index out of bounds",
            ));
        }
        let ty = func_type_at(&resources, index)?;
        let types = {
            let mut types = Vec::with_capacity(ty.len_inputs() + 1);
            types.extend(ty.inputs());
            types.push(Type::I32);
            types
        };
        self.check_operands(types.into_iter())?;
        self.func_state
            .change_frame_with_types(ty.len_inputs() + 1, ty.outputs())?;
        Ok(())
    }

    fn check_return(&mut self) -> OperatorValidatorResult<()> {
        let depth = (self.func_state.blocks.len() - 1) as u32;
        self.check_jump_from_block(depth, 0)?;
        self.func_state.start_dead_code();
        Ok(())
    }

    fn check_jump_from_block(
        &self,
        relative_depth: u32,
        reserve_items: usize,
    ) -> OperatorValidatorResult<()> {
        if relative_depth as usize >= self.func_state.blocks.len() {
            return Err(OperatorValidatorError::new(
                "unknown label: invalid block depth",
            ));
        }
        let block = self.func_state.block_at(relative_depth as usize);
        if block.jump_to_top {
            let len = block.start_types.len();
            if !self
                .func_state
                .assert_block_stack_len(0, reserve_items + len)
            {
                return Err(OperatorValidatorError::new(
                    "type mismatch: stack size does not match target loop type",
                ));
            }
            for i in 0..len {
                if !self
                    .func_state
                    .assert_stack_type_at(len - 1 - i + reserve_items, block.start_types[i])
                {
                    return Err(OperatorValidatorError::new(
                        "type mismatch: stack item type does not match block param type",
                    ));
                }
            }
            return Ok(());
        }

        let len = block.return_types.len();
        if !self
            .func_state
            .assert_block_stack_len(0, len + reserve_items)
        {
            return Err(OperatorValidatorError::new(
                "type mismatch: stack size does not match target block type",
            ));
        }
        self.check_block_return_types(block, reserve_items)
    }

    fn match_block_return(&self, depth1: u32, depth2: u32) -> OperatorValidatorResult<()> {
        if depth1 as usize >= self.func_state.blocks.len() {
            return Err(OperatorValidatorError::new(
                "unknown label: invalid block depth",
            ));
        }
        if depth2 as usize >= self.func_state.blocks.len() {
            return Err(OperatorValidatorError::new(
                "unknown label: invalid block depth",
            ));
        }
        let block1 = self.func_state.block_at(depth1 as usize);
        let block2 = self.func_state.block_at(depth2 as usize);
        let return_types1 = &block1.return_types;
        let return_types2 = &block2.return_types;
        if block1.jump_to_top || block2.jump_to_top {
            if block1.jump_to_top {
                if !block2.jump_to_top && !return_types2.is_empty() {
                    return Err(OperatorValidatorError::new(
                        "type mismatch: block types do not match",
                    ));
                }
            } else if !return_types1.is_empty() {
                return Err(OperatorValidatorError::new(
                    "type mismatch: block types do not match",
                ));
            }
        } else if *return_types1 != *return_types2 {
            return Err(OperatorValidatorError::new(
                "type mismatch: block types do not match",
            ));
        }
        Ok(())
    }

    fn check_memory_index(
        &self,
        memory_index: u32,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<Type> {
        if memory_index > 0 && !self.features.multi_memory {
            return Err(OperatorValidatorError::new(
                "multi-memory support is not enabled",
            ));
        }
        match resources.memory_at(memory_index) {
            Some(mem) => Ok(mem.index_type()),
            None => bail_op_err!("unknown memory {}", memory_index),
        }
    }

    fn check_memarg(
        &self,
        memarg: MemoryImmediate,
        max_align: u8,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<Type> {
        let index_ty = self.check_memory_index(memarg.memory, resources)?;
        let align = memarg.align;
        if align > max_align {
            return Err(OperatorValidatorError::new(
                "alignment must not be larger than natural",
            ));
        }
        Ok(index_ty)
    }

    #[cfg(feature = "deterministic")]
    fn check_non_deterministic_enabled(&self) -> OperatorValidatorResult<()> {
        if !self.features.deterministic_only {
            return Err(OperatorValidatorError::new(
                "deterministic_only support is not enabled",
            ));
        }
        Ok(())
    }

    #[inline(always)]
    #[cfg(not(feature = "deterministic"))]
    fn check_non_deterministic_enabled(&self) -> OperatorValidatorResult<()> {
        Ok(())
    }

    fn check_threads_enabled(&self) -> OperatorValidatorResult<()> {
        if !self.features.threads {
            return Err(OperatorValidatorError::new(
                "threads support is not enabled",
            ));
        }
        Ok(())
    }

    fn check_reference_types_enabled(&self) -> OperatorValidatorResult<()> {
        if !self.features.reference_types {
            return Err(OperatorValidatorError::new(
                "reference types support is not enabled",
            ));
        }
        Ok(())
    }

    fn check_simd_enabled(&self) -> OperatorValidatorResult<()> {
        if !self.features.simd {
            return Err(OperatorValidatorError::new("SIMD support is not enabled"));
        }
        Ok(())
    }

    fn check_bulk_memory_enabled(&self) -> OperatorValidatorResult<()> {
        if !self.features.bulk_memory {
            return Err(OperatorValidatorError::new(
                "bulk memory support is not enabled",
            ));
        }
        Ok(())
    }

    fn check_shared_memarg_wo_align(
        &self,
        _: MemoryImmediate,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<Type> {
        self.check_memory_index(0, resources)
    }

    fn check_simd_lane_index(&self, index: SIMDLaneIndex, max: u8) -> OperatorValidatorResult<()> {
        if index >= max {
            return Err(OperatorValidatorError::new("SIMD index out of bounds"));
        }
        Ok(())
    }

    fn check_block_type(
        &self,
        ty: TypeOrFuncType,
        resources: impl WasmModuleResources,
    ) -> OperatorValidatorResult<()> {
        match ty {
            TypeOrFuncType::Type(Type::EmptyBlockType)
            | TypeOrFuncType::Type(Type::I32)
            | TypeOrFuncType::Type(Type::I64)
            | TypeOrFuncType::Type(Type::F32)
            | TypeOrFuncType::Type(Type::F64) => Ok(()),
            TypeOrFuncType::Type(Type::ExternRef) | TypeOrFuncType::Type(Type::FuncRef) => {
                self.check_reference_types_enabled()
            }
            TypeOrFuncType::Type(Type::V128) => self.check_simd_enabled(),
            TypeOrFuncType::FuncType(idx) => {
                let ty = func_type_at(&resources, idx)?;
                if !self.features.multi_value {
                    if ty.len_outputs() > 1 {
                        return Err(OperatorValidatorError::new(
                            "blocks, loops, and ifs may only return at most one \
                             value when multi-value is not enabled",
                        ));
                    }
                    if ty.len_inputs() > 0 {
                        return Err(OperatorValidatorError::new(
                            "blocks, loops, and ifs accept no parameters \
                             when multi-value is not enabled",
                        ));
                    }
                }
                Ok(())
            }
            _ => Err(OperatorValidatorError::new("invalid block return type")),
        }
    }

    fn check_block_params(
        &self,
        ty: TypeOrFuncType,
        resources: impl WasmModuleResources,
        skip: usize,
    ) -> OperatorValidatorResult<()> {
        if let TypeOrFuncType::FuncType(idx) = ty {
            let func_ty = func_type_at(&resources, idx)?;
            let len = func_ty.len_inputs();
            self.check_frame_size(len + skip)?;
            for (i, ty) in func_ty.inputs().enumerate() {
                if !self.func_state.assert_stack_type_at(len - 1 - i + skip, ty) {
                    return Err(OperatorValidatorError::new(
                        "stack operand type mismatch for block",
                    ));
                }
            }
        }
        Ok(())
    }

    fn check_select(&self, expected_ty: Option<Type>) -> OperatorValidatorResult<Option<Type>> {
        self.check_frame_size(3)?;
        let func_state = &self.func_state;
        let last_block = func_state.last_block();

        let ty = if last_block.is_stack_polymorphic() {
            match func_state.stack_types.len() - last_block.stack_starts_at {
                0 => return Ok(None),
                1 => {
                    self.check_operands_1(Type::I32)?;
                    return Ok(None);
                }
                2 => {
                    self.check_operands_1(Type::I32)?;
                    func_state.stack_types[func_state.stack_types.len() - 2]
                }
                _ => {
                    let ty = expected_ty
                        .unwrap_or(func_state.stack_types[func_state.stack_types.len() - 3]);
                    self.check_operands_2(ty, Type::I32)?;
                    ty
                }
            }
        } else {
            let ty =
                expected_ty.unwrap_or(func_state.stack_types[func_state.stack_types.len() - 3]);
            self.check_operands_2(ty, Type::I32)?;
            ty
        };

        Ok(Some(ty))
    }

    pub(crate) fn process_operator(
        &mut self,
        operator: &Operator,
        resources: &impl WasmModuleResources,
    ) -> OperatorValidatorResult<FunctionEnd> {
        if self.func_state.end_function {
            return Err(OperatorValidatorError::new("unexpected operator"));
        }
        match *operator {
            Operator::Unreachable => self.func_state.start_dead_code(),
            Operator::Nop => (),
            Operator::Block { ty } => {
                self.check_block_type(ty, resources)?;
                self.check_block_params(ty, resources, 0)?;
                self.func_state
                    .push_block(ty, BlockType::Block, resources)?;
            }
            Operator::Loop { ty } => {
                self.check_block_type(ty, resources)?;
                self.check_block_params(ty, resources, 0)?;
                self.func_state.push_block(ty, BlockType::Loop, resources)?;
            }
            Operator::If { ty } => {
                self.check_block_type(ty, resources)?;
                self.check_operands_1(Type::I32)?;
                self.check_block_params(ty, resources, 1)?;
                self.func_state.push_block(ty, BlockType::If, resources)?;
            }
            Operator::Else => {
                if !self.func_state.last_block().is_else_allowed {
                    return Err(OperatorValidatorError::new(
                        "unexpected else: if block is not started",
                    ));
                }
                self.check_block_return()?;
                self.func_state.reset_block()
            }
            Operator::End => {
                self.check_block_return()?;
                if self.func_state.blocks.len() == 1 {
                    self.func_state.end_function();
                    return Ok(FunctionEnd::Yes);
                }

                let last_block = &self.func_state.last_block();
                if last_block.is_else_allowed && last_block.start_types != last_block.return_types {
                    return Err(OperatorValidatorError::new("type mismatch: else is expected: if block has a type that can't be implemented with a no-op"));
                }
                self.func_state.pop_block()
            }
            Operator::Br { relative_depth } => {
                self.check_jump_from_block(relative_depth, 0)?;
                self.func_state.start_dead_code()
            }
            Operator::BrIf { relative_depth } => {
                self.check_operands_1(Type::I32)?;
                self.check_jump_from_block(relative_depth, 1)?;
                self.func_state.change_frame(1)?;
                if self.func_state.last_block().is_stack_polymorphic() {
                    self.func_state
                        .change_frame_to_exact_types_from(relative_depth as usize)?;
                }
            }
            Operator::BrTable { ref table } => {
                self.check_operands_1(Type::I32)?;
                let mut depth0: Option<u32> = None;
                for element in table.targets() {
                    let (relative_depth, _is_default) = element.map_err(|mut e| {
                        e.inner.offset = usize::max_value();
                        OperatorValidatorError(e)
                    })?;
                    if depth0.is_none() {
                        self.check_jump_from_block(relative_depth, 1)?;
                        depth0 = Some(relative_depth);
                        continue;
                    }
                    self.match_block_return(relative_depth, depth0.unwrap())?;
                }
                self.func_state.start_dead_code()
            }
            Operator::Return => self.check_return()?,
            Operator::Call { function_index } => self.check_call(function_index, resources)?,
            Operator::ReturnCall { function_index } => {
                if !self.features.tail_call {
                    return Err(OperatorValidatorError::new(
                        "tail calls support is not enabled",
                    ));
                }
                self.check_call(function_index, resources)?;
                self.check_return()?;
            }
            Operator::CallIndirect { index, table_index } => {
                self.check_call_indirect(index, table_index, resources)?
            }
            Operator::ReturnCallIndirect { index, table_index } => {
                if !self.features.tail_call {
                    return Err(OperatorValidatorError::new(
                        "tail calls support is not enabled",
                    ));
                }
                self.check_call_indirect(index, table_index, resources)?;
                self.check_return()?;
            }
            Operator::Drop => {
                self.check_frame_size(1)?;
                self.func_state.change_frame(1)?;
            }
            Operator::Select => {
                let ty = self.check_select(None)?;
                match ty {
                    Some(Type::I32) | Some(Type::I64) | Some(Type::F32) | Some(Type::F64) => {}
                    Some(_) => {
                        bail_op_err!("type mismatch: only integer types allowed with bare `select`")
                    }
                    None => {}
                }
                self.func_state.change_frame_after_select(ty)?;
            }
            Operator::TypedSelect { ty } => {
                self.check_select(Some(ty))?;
                self.func_state.change_frame_after_select(Some(ty))?;
            }
            Operator::LocalGet { local_index } => {
                if local_index as usize >= self.func_state.local_types.len() {
                    bail_op_err!("unknown local {}: local index out of bounds", local_index);
                }
                let local_type = self.func_state.local_types[local_index as usize];
                self.func_state.change_frame_with_type(0, local_type)?;
            }
            Operator::LocalSet { local_index } => {
                if local_index as usize >= self.func_state.local_types.len() {
                    bail_op_err!("unknown local {}: local index out of bounds", local_index);
                }
                let local_type = self.func_state.local_types[local_index as usize];
                self.check_operands_1(local_type)?;
                self.func_state.change_frame(1)?;
            }
            Operator::LocalTee { local_index } => {
                if local_index as usize >= self.func_state.local_types.len() {
                    bail_op_err!("unknown local {}: local index out of bounds", local_index);
                }
                let local_type = self.func_state.local_types[local_index as usize];
                self.check_operands_1(local_type)?;
                self.func_state.change_frame_with_type(1, local_type)?;
            }
            Operator::GlobalGet { global_index } => {
                if let Some(ty) = resources.global_at(global_index) {
                    self.func_state.change_frame_with_type(0, ty.content_type)?;
                } else {
                    return Err(OperatorValidatorError::new(
                        "unknown global: global index out of bounds",
                    ));
                };
            }
            Operator::GlobalSet { global_index } => {
                if let Some(ty) = resources.global_at(global_index) {
                    if !ty.mutable {
                        return Err(OperatorValidatorError::new(
                            "global is immutable: cannot modify it with `global.set`",
                        ));
                    }
                    self.check_operands_1(ty.content_type)?;
                    self.func_state.change_frame(1)?;
                } else {
                    return Err(OperatorValidatorError::new(
                        "unknown global: global index out of bounds",
                    ));
                };
            }
            Operator::I32Load { memarg } => {
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64Load { memarg } => {
                let ty = self.check_memarg(memarg, 3, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::F32Load { memarg } => {
                self.check_non_deterministic_enabled()?;
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F64Load { memarg } => {
                self.check_non_deterministic_enabled()?;
                let ty = self.check_memarg(memarg, 3, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::I32Load8S { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32Load8U { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32Load16S { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32Load16U { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64Load8S { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Load8U { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Load16S { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Load16U { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Load32S { memarg } => {
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Load32U { memarg } => {
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I32Store { memarg } => {
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I64Store { memarg } => {
                let ty = self.check_memarg(memarg, 3, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::F32Store { memarg } => {
                self.check_non_deterministic_enabled()?;
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_2(ty, Type::F32)?;
                self.func_state.change_frame(2)?;
            }
            Operator::F64Store { memarg } => {
                self.check_non_deterministic_enabled()?;
                let ty = self.check_memarg(memarg, 3, resources)?;
                self.check_operands_2(ty, Type::F64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I32Store8 { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I32Store16 { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I64Store8 { memarg } => {
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I64Store16 { memarg } => {
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I64Store32 { memarg } => {
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::MemorySize { mem, mem_byte } => {
                if mem_byte != 0 && !self.features.multi_memory {
                    return Err(OperatorValidatorError::new("multi-memory not enabled"));
                }
                let index_ty = self.check_memory_index(mem, resources)?;
                self.func_state.change_frame_with_type(0, index_ty)?;
            }
            Operator::MemoryGrow { mem, mem_byte } => {
                if mem_byte != 0 && !self.features.multi_memory {
                    return Err(OperatorValidatorError::new("multi-memory not enabled"));
                }
                let index_ty = self.check_memory_index(mem, resources)?;
                self.check_operands_1(index_ty)?;
                self.func_state.change_frame_with_type(1, index_ty)?;
            }
            Operator::I32Const { .. } => self.func_state.change_frame_with_type(0, Type::I32)?,
            Operator::I64Const { .. } => self.func_state.change_frame_with_type(0, Type::I64)?,
            Operator::F32Const { .. } => {
                self.check_non_deterministic_enabled()?;
                self.func_state.change_frame_with_type(0, Type::F32)?;
            }
            Operator::F64Const { .. } => {
                self.check_non_deterministic_enabled()?;
                self.func_state.change_frame_with_type(0, Type::F64)?;
            }
            Operator::I32Eqz => {
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32Eq
            | Operator::I32Ne
            | Operator::I32LtS
            | Operator::I32LtU
            | Operator::I32GtS
            | Operator::I32GtU
            | Operator::I32LeS
            | Operator::I32LeU
            | Operator::I32GeS
            | Operator::I32GeU => {
                self.check_operands_2(Type::I32, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::I64Eqz => {
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64Eq
            | Operator::I64Ne
            | Operator::I64LtS
            | Operator::I64LtU
            | Operator::I64GtS
            | Operator::I64GtU
            | Operator::I64LeS
            | Operator::I64LeU
            | Operator::I64GeS
            | Operator::I64GeU => {
                self.check_operands_2(Type::I64, Type::I64)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::F32Eq
            | Operator::F32Ne
            | Operator::F32Lt
            | Operator::F32Gt
            | Operator::F32Le
            | Operator::F32Ge => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_2(Type::F32, Type::F32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::F64Eq
            | Operator::F64Ne
            | Operator::F64Lt
            | Operator::F64Gt
            | Operator::F64Le
            | Operator::F64Ge => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_2(Type::F64, Type::F64)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::I32Clz | Operator::I32Ctz | Operator::I32Popcnt => {
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32Add
            | Operator::I32Sub
            | Operator::I32Mul
            | Operator::I32DivS
            | Operator::I32DivU
            | Operator::I32RemS
            | Operator::I32RemU
            | Operator::I32And
            | Operator::I32Or
            | Operator::I32Xor
            | Operator::I32Shl
            | Operator::I32ShrS
            | Operator::I32ShrU
            | Operator::I32Rotl
            | Operator::I32Rotr => {
                self.check_operands_2(Type::I32, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::I64Clz | Operator::I64Ctz | Operator::I64Popcnt => {
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64Add
            | Operator::I64Sub
            | Operator::I64Mul
            | Operator::I64DivS
            | Operator::I64DivU
            | Operator::I64RemS
            | Operator::I64RemU
            | Operator::I64And
            | Operator::I64Or
            | Operator::I64Xor
            | Operator::I64Shl
            | Operator::I64ShrS
            | Operator::I64ShrU
            | Operator::I64Rotl
            | Operator::I64Rotr => {
                self.check_operands_2(Type::I64, Type::I64)?;
                self.func_state.change_frame_with_type(2, Type::I64)?;
            }
            Operator::F32Abs
            | Operator::F32Neg
            | Operator::F32Ceil
            | Operator::F32Floor
            | Operator::F32Trunc
            | Operator::F32Nearest
            | Operator::F32Sqrt => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F32Add
            | Operator::F32Sub
            | Operator::F32Mul
            | Operator::F32Div
            | Operator::F32Min
            | Operator::F32Max
            | Operator::F32Copysign => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_2(Type::F32, Type::F32)?;
                self.func_state.change_frame_with_type(2, Type::F32)?;
            }
            Operator::F64Abs
            | Operator::F64Neg
            | Operator::F64Ceil
            | Operator::F64Floor
            | Operator::F64Trunc
            | Operator::F64Nearest
            | Operator::F64Sqrt => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::F64Add
            | Operator::F64Sub
            | Operator::F64Mul
            | Operator::F64Div
            | Operator::F64Min
            | Operator::F64Max
            | Operator::F64Copysign => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_2(Type::F64, Type::F64)?;
                self.func_state.change_frame_with_type(2, Type::F64)?;
            }
            Operator::I32WrapI64 => {
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32TruncF32S | Operator::I32TruncF32U => {
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32TruncF64S | Operator::I32TruncF64U => {
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64ExtendI32S | Operator::I64ExtendI32U => {
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64TruncF32S | Operator::I64TruncF32U => {
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64TruncF64S | Operator::I64TruncF64U => {
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::F32ConvertI32S | Operator::F32ConvertI32U => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F32ConvertI64S | Operator::F32ConvertI64U => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F32DemoteF64 => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F64ConvertI32S | Operator::F64ConvertI32U => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::F64ConvertI64S | Operator::F64ConvertI64U => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::F64PromoteF32 => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::I32ReinterpretF32 => {
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64ReinterpretF64 => {
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::F32ReinterpretI32 => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F64ReinterpretI64 => {
                self.check_non_deterministic_enabled()?;
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::I32TruncSatF32S | Operator::I32TruncSatF32U => {
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32TruncSatF64S | Operator::I32TruncSatF64U => {
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64TruncSatF32S | Operator::I64TruncSatF32U => {
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64TruncSatF64S | Operator::I64TruncSatF64U => {
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I32Extend16S | Operator::I32Extend8S => {
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }

            Operator::I64Extend32S | Operator::I64Extend16S | Operator::I64Extend8S => {
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }

            Operator::I32AtomicLoad { memarg }
            | Operator::I32AtomicLoad16U { memarg }
            | Operator::I32AtomicLoad8U { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I64AtomicLoad { memarg }
            | Operator::I64AtomicLoad32U { memarg }
            | Operator::I64AtomicLoad16U { memarg }
            | Operator::I64AtomicLoad8U { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I32AtomicStore { memarg }
            | Operator::I32AtomicStore16 { memarg }
            | Operator::I32AtomicStore8 { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I64AtomicStore { memarg }
            | Operator::I64AtomicStore32 { memarg }
            | Operator::I64AtomicStore16 { memarg }
            | Operator::I64AtomicStore8 { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame(2)?;
            }
            Operator::I32AtomicRmwAdd { memarg }
            | Operator::I32AtomicRmwSub { memarg }
            | Operator::I32AtomicRmwAnd { memarg }
            | Operator::I32AtomicRmwOr { memarg }
            | Operator::I32AtomicRmwXor { memarg }
            | Operator::I32AtomicRmw16AddU { memarg }
            | Operator::I32AtomicRmw16SubU { memarg }
            | Operator::I32AtomicRmw16AndU { memarg }
            | Operator::I32AtomicRmw16OrU { memarg }
            | Operator::I32AtomicRmw16XorU { memarg }
            | Operator::I32AtomicRmw8AddU { memarg }
            | Operator::I32AtomicRmw8SubU { memarg }
            | Operator::I32AtomicRmw8AndU { memarg }
            | Operator::I32AtomicRmw8OrU { memarg }
            | Operator::I32AtomicRmw8XorU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::I64AtomicRmwAdd { memarg }
            | Operator::I64AtomicRmwSub { memarg }
            | Operator::I64AtomicRmwAnd { memarg }
            | Operator::I64AtomicRmwOr { memarg }
            | Operator::I64AtomicRmwXor { memarg }
            | Operator::I64AtomicRmw32AddU { memarg }
            | Operator::I64AtomicRmw32SubU { memarg }
            | Operator::I64AtomicRmw32AndU { memarg }
            | Operator::I64AtomicRmw32OrU { memarg }
            | Operator::I64AtomicRmw32XorU { memarg }
            | Operator::I64AtomicRmw16AddU { memarg }
            | Operator::I64AtomicRmw16SubU { memarg }
            | Operator::I64AtomicRmw16AndU { memarg }
            | Operator::I64AtomicRmw16OrU { memarg }
            | Operator::I64AtomicRmw16XorU { memarg }
            | Operator::I64AtomicRmw8AddU { memarg }
            | Operator::I64AtomicRmw8SubU { memarg }
            | Operator::I64AtomicRmw8AndU { memarg }
            | Operator::I64AtomicRmw8OrU { memarg }
            | Operator::I64AtomicRmw8XorU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame_with_type(2, Type::I64)?;
            }
            Operator::I32AtomicRmwXchg { memarg }
            | Operator::I32AtomicRmw16XchgU { memarg }
            | Operator::I32AtomicRmw8XchgU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::I32AtomicRmwCmpxchg { memarg }
            | Operator::I32AtomicRmw16CmpxchgU { memarg }
            | Operator::I32AtomicRmw8CmpxchgU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_3(ty, Type::I32, Type::I32)?;
                self.func_state.change_frame_with_type(3, Type::I32)?;
            }
            Operator::I64AtomicRmwXchg { memarg }
            | Operator::I64AtomicRmw32XchgU { memarg }
            | Operator::I64AtomicRmw16XchgU { memarg }
            | Operator::I64AtomicRmw8XchgU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I64)?;
                self.func_state.change_frame_with_type(2, Type::I64)?;
            }
            Operator::I64AtomicRmwCmpxchg { memarg }
            | Operator::I64AtomicRmw32CmpxchgU { memarg }
            | Operator::I64AtomicRmw16CmpxchgU { memarg }
            | Operator::I64AtomicRmw8CmpxchgU { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_3(ty, Type::I64, Type::I64)?;
                self.func_state.change_frame_with_type(3, Type::I64)?;
            }
            Operator::MemoryAtomicNotify { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::MemoryAtomicWait32 { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_3(ty, Type::I32, Type::I64)?;
                self.func_state.change_frame_with_type(3, Type::I32)?;
            }
            Operator::MemoryAtomicWait64 { memarg } => {
                self.check_threads_enabled()?;
                let ty = self.check_shared_memarg_wo_align(memarg, resources)?;
                self.check_operands_3(ty, Type::I64, Type::I64)?;
                self.func_state.change_frame_with_type(3, Type::I32)?;
            }
            Operator::AtomicFence { ref flags } => {
                self.check_threads_enabled()?;
                if *flags != 0 {
                    return Err(OperatorValidatorError::new(
                        "non-zero flags for fence not supported yet",
                    ));
                }
            }
            Operator::RefNull { ty } => {
                self.check_reference_types_enabled()?;
                match ty {
                    Type::FuncRef | Type::ExternRef => {}
                    _ => {
                        return Err(OperatorValidatorError::new(
                            "invalid reference type in ref.null",
                        ))
                    }
                }
                self.func_state.change_frame_with_type(0, ty)?;
            }
            Operator::RefIsNull => {
                self.check_reference_types_enabled()?;
                self.check_frame_size(1)?;
                match self.func_state.stack_type_at(0) {
                    None | Some(Type::FuncRef) | Some(Type::ExternRef) => {}
                    _ => {
                        return Err(OperatorValidatorError::new(
                            "type mismatch: invalid reference type in ref.is_null",
                        ))
                    }
                }
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::RefFunc { function_index } => {
                self.check_reference_types_enabled()?;
                if resources.type_of_function(function_index).is_none() {
                    return Err(OperatorValidatorError::new(
                        "unknown function: function index out of bounds",
                    ));
                }
                if !resources.is_function_referenced(function_index) {
                    return Err(OperatorValidatorError::new("undeclared function reference"));
                }
                self.func_state.change_frame_with_type(0, Type::FuncRef)?;
            }
            Operator::V128Load { memarg } => {
                self.check_simd_enabled()?;
                let ty = self.check_memarg(memarg, 4, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V128Store { memarg } => {
                self.check_simd_enabled()?;
                let ty = self.check_memarg(memarg, 4, resources)?;
                self.check_operands_2(ty, Type::V128)?;
                self.func_state.change_frame(2)?;
            }
            Operator::V128Const { .. } => {
                self.check_simd_enabled()?;
                self.func_state.change_frame_with_type(0, Type::V128)?;
            }
            Operator::I8x16Splat | Operator::I16x8Splat | Operator::I32x4Splat => {
                self.check_simd_enabled()?;
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::I64x2Splat => {
                self.check_simd_enabled()?;
                self.check_operands_1(Type::I64)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::F32x4Splat => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_operands_1(Type::F32)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::F64x2Splat => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_operands_1(Type::F64)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::I8x16ExtractLaneS { lane } | Operator::I8x16ExtractLaneU { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 16)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I16x8ExtractLaneS { lane } | Operator::I16x8ExtractLaneU { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 8)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I32x4ExtractLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 4)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I8x16ReplaceLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 16)?;
                self.check_operands_2(Type::V128, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::I16x8ReplaceLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 8)?;
                self.check_operands_2(Type::V128, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::I32x4ReplaceLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 4)?;
                self.check_operands_2(Type::V128, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::I64x2ExtractLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 2)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::I64)?;
            }
            Operator::I64x2ReplaceLane { lane } => {
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 2)?;
                self.check_operands_2(Type::V128, Type::I64)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::F32x4ExtractLane { lane } => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 4)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::F32)?;
            }
            Operator::F32x4ReplaceLane { lane } => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 4)?;
                self.check_operands_2(Type::V128, Type::F32)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::F64x2ExtractLane { lane } => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 2)?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::F64)?;
            }
            Operator::F64x2ReplaceLane { lane } => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_simd_lane_index(lane, 2)?;
                self.check_operands_2(Type::V128, Type::F64)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::F32x4Eq
            | Operator::F32x4Ne
            | Operator::F32x4Lt
            | Operator::F32x4Gt
            | Operator::F32x4Le
            | Operator::F32x4Ge
            | Operator::F64x2Eq
            | Operator::F64x2Ne
            | Operator::F64x2Lt
            | Operator::F64x2Gt
            | Operator::F64x2Le
            | Operator::F64x2Ge
            | Operator::F32x4Add
            | Operator::F32x4Sub
            | Operator::F32x4Mul
            | Operator::F32x4Div
            | Operator::F32x4Min
            | Operator::F32x4Max
            | Operator::F64x2Add
            | Operator::F64x2Sub
            | Operator::F64x2Mul
            | Operator::F64x2Div
            | Operator::F64x2Min
            | Operator::F64x2Max => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_operands_2(Type::V128, Type::V128)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::I8x16Eq
            | Operator::I8x16Ne
            | Operator::I8x16LtS
            | Operator::I8x16LtU
            | Operator::I8x16GtS
            | Operator::I8x16GtU
            | Operator::I8x16LeS
            | Operator::I8x16LeU
            | Operator::I8x16GeS
            | Operator::I8x16GeU
            | Operator::I16x8Eq
            | Operator::I16x8Ne
            | Operator::I16x8LtS
            | Operator::I16x8LtU
            | Operator::I16x8GtS
            | Operator::I16x8GtU
            | Operator::I16x8LeS
            | Operator::I16x8LeU
            | Operator::I16x8GeS
            | Operator::I16x8GeU
            | Operator::I32x4Eq
            | Operator::I32x4Ne
            | Operator::I32x4LtS
            | Operator::I32x4LtU
            | Operator::I32x4GtS
            | Operator::I32x4GtU
            | Operator::I32x4LeS
            | Operator::I32x4LeU
            | Operator::I32x4GeS
            | Operator::I32x4GeU
            | Operator::V128And
            | Operator::V128AndNot
            | Operator::V128Or
            | Operator::V128Xor
            | Operator::I8x16Add
            | Operator::I8x16AddSaturateS
            | Operator::I8x16AddSaturateU
            | Operator::I8x16Sub
            | Operator::I8x16SubSaturateS
            | Operator::I8x16SubSaturateU
            | Operator::I8x16MinS
            | Operator::I8x16MinU
            | Operator::I8x16MaxS
            | Operator::I8x16MaxU
            | Operator::I16x8Add
            | Operator::I16x8AddSaturateS
            | Operator::I16x8AddSaturateU
            | Operator::I16x8Sub
            | Operator::I16x8SubSaturateS
            | Operator::I16x8SubSaturateU
            | Operator::I16x8Mul
            | Operator::I16x8MinS
            | Operator::I16x8MinU
            | Operator::I16x8MaxS
            | Operator::I16x8MaxU
            | Operator::I32x4Add
            | Operator::I32x4Sub
            | Operator::I32x4Mul
            | Operator::I32x4MinS
            | Operator::I32x4MinU
            | Operator::I32x4MaxS
            | Operator::I32x4MaxU
            | Operator::I64x2Add
            | Operator::I64x2Sub
            | Operator::I64x2Mul
            | Operator::I8x16RoundingAverageU
            | Operator::I16x8RoundingAverageU
            | Operator::I8x16NarrowI16x8S
            | Operator::I8x16NarrowI16x8U
            | Operator::I16x8NarrowI32x4S
            | Operator::I16x8NarrowI32x4U => {
                self.check_simd_enabled()?;
                self.check_operands_2(Type::V128, Type::V128)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::F32x4Abs
            | Operator::F32x4Neg
            | Operator::F32x4Sqrt
            | Operator::F64x2Abs
            | Operator::F64x2Neg
            | Operator::F64x2Sqrt
            | Operator::F32x4ConvertI32x4S
            | Operator::F32x4ConvertI32x4U => {
                self.check_non_deterministic_enabled()?;
                self.check_simd_enabled()?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V128Not
            | Operator::I8x16Abs
            | Operator::I8x16Neg
            | Operator::I16x8Abs
            | Operator::I16x8Neg
            | Operator::I32x4Abs
            | Operator::I32x4Neg
            | Operator::I64x2Neg
            | Operator::I32x4TruncSatF32x4S
            | Operator::I32x4TruncSatF32x4U
            | Operator::I16x8WidenLowI8x16S
            | Operator::I16x8WidenHighI8x16S
            | Operator::I16x8WidenLowI8x16U
            | Operator::I16x8WidenHighI8x16U
            | Operator::I32x4WidenLowI16x8S
            | Operator::I32x4WidenHighI16x8S
            | Operator::I32x4WidenLowI16x8U
            | Operator::I32x4WidenHighI16x8U => {
                self.check_simd_enabled()?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V128Bitselect => {
                self.check_simd_enabled()?;
                self.check_operands_3(Type::V128, Type::V128, Type::V128)?;
                self.func_state.change_frame_with_type(3, Type::V128)?;
            }
            Operator::I8x16AnyTrue
            | Operator::I8x16AllTrue
            | Operator::I8x16Bitmask
            | Operator::I16x8AnyTrue
            | Operator::I16x8AllTrue
            | Operator::I16x8Bitmask
            | Operator::I32x4AnyTrue
            | Operator::I32x4AllTrue
            | Operator::I32x4Bitmask => {
                self.check_simd_enabled()?;
                self.check_operands_1(Type::V128)?;
                self.func_state.change_frame_with_type(1, Type::I32)?;
            }
            Operator::I8x16Shl
            | Operator::I8x16ShrS
            | Operator::I8x16ShrU
            | Operator::I16x8Shl
            | Operator::I16x8ShrS
            | Operator::I16x8ShrU
            | Operator::I32x4Shl
            | Operator::I32x4ShrS
            | Operator::I32x4ShrU
            | Operator::I64x2Shl
            | Operator::I64x2ShrS
            | Operator::I64x2ShrU => {
                self.check_simd_enabled()?;
                self.check_operands_2(Type::V128, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::V8x16Swizzle => {
                self.check_simd_enabled()?;
                self.check_operands_2(Type::V128, Type::V128)?;
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::V8x16Shuffle { ref lanes } => {
                self.check_simd_enabled()?;
                self.check_operands_2(Type::V128, Type::V128)?;
                for i in lanes {
                    self.check_simd_lane_index(*i, 32)?;
                }
                self.func_state.change_frame_with_type(2, Type::V128)?;
            }
            Operator::V8x16LoadSplat { memarg } => {
                self.check_simd_enabled()?;
                let ty = self.check_memarg(memarg, 0, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V16x8LoadSplat { memarg } => {
                self.check_simd_enabled()?;
                let ty = self.check_memarg(memarg, 1, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V32x4LoadSplat { memarg } => {
                self.check_simd_enabled()?;
                let ty = self.check_memarg(memarg, 2, resources)?;
                self.check_operands_1(ty)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }
            Operator::V64x2LoadSplat { memarg }
            | Operator::I16x8Load8x8S { memarg }
            | Operator::I16x8Load8x8U { memarg }
            | Operator::I32x4Load16x4S { memarg }
            | Operator::I32x4Load16x4U { memarg }
            | Operator::I64x2Load32x2S { memarg }
            | Operator::I64x2Load32x2U { memarg } => {
                self.check_simd_enabled()?;
                let idx = self.check_memarg(memarg, 3, resources)?;
                self.check_operands_1(idx)?;
                self.func_state.change_frame_with_type(1, Type::V128)?;
            }

            Operator::MemoryInit { mem, segment } => {
                self.check_bulk_memory_enabled()?;
                let ty = self.check_memory_index(mem, resources)?;
                if segment >= resources.data_count() {
                    bail_op_err!("unknown data segment {}", segment);
                }
                self.check_operands_3(ty, Type::I32, Type::I32)?;
                self.func_state.change_frame(3)?;
            }
            Operator::DataDrop { segment } => {
                self.check_bulk_memory_enabled()?;
                if segment >= resources.data_count() {
                    bail_op_err!("unknown data segment {}", segment);
                }
            }
            Operator::MemoryCopy { src, dst } => {
                self.check_bulk_memory_enabled()?;
                let src_ty = self.check_memory_index(src, resources)?;
                let dst_ty = self.check_memory_index(dst, resources)?;
                self.check_operands_3(
                    dst_ty,
                    src_ty,
                    match src_ty {
                        Type::I32 => Type::I32,
                        _ => dst_ty,
                    },
                )?;
                self.func_state.change_frame(3)?;
            }
            Operator::MemoryFill { mem } => {
                self.check_bulk_memory_enabled()?;
                let ty = self.check_memory_index(mem, resources)?;
                self.check_operands_3(ty, Type::I32, ty)?;
                self.func_state.change_frame(3)?;
            }
            Operator::TableInit { segment, table } => {
                self.check_bulk_memory_enabled()?;
                if table > 0 {
                    self.check_reference_types_enabled()?;
                }
                let table = match resources.table_at(table) {
                    Some(table) => table,
                    None => bail_op_err!("unknown table {}: table index out of bounds", table),
                };
                let segment_ty = match resources.element_type_at(segment) {
                    Some(ty) => ty,
                    None => bail_op_err!(
                        "unknown elem segment {}: segment index out of bounds",
                        segment
                    ),
                };
                if segment_ty != table.element_type {
                    return Err(OperatorValidatorError::new("type mismatch"));
                }
                self.check_operands_3(Type::I32, Type::I32, Type::I32)?;
                self.func_state.change_frame(3)?;
            }
            Operator::ElemDrop { segment } => {
                self.check_bulk_memory_enabled()?;
                if segment >= resources.element_count() {
                    bail_op_err!(
                        "unknown elem segment {}: segment index out of bounds",
                        segment
                    );
                }
            }
            Operator::TableCopy {
                src_table,
                dst_table,
            } => {
                self.check_bulk_memory_enabled()?;
                if src_table > 0 || dst_table > 0 {
                    self.check_reference_types_enabled()?;
                }
                let (src, dst) =
                    match (resources.table_at(src_table), resources.table_at(dst_table)) {
                        (Some(a), Some(b)) => (a, b),
                        _ => return Err(OperatorValidatorError::new("table index out of bounds")),
                    };
                if src.element_type != dst.element_type {
                    return Err(OperatorValidatorError::new("type mismatch"));
                }
                self.check_operands_3(Type::I32, Type::I32, Type::I32)?;
                self.func_state.change_frame(3)?;
            }
            Operator::TableGet { table } => {
                self.check_reference_types_enabled()?;
                let ty = match resources.table_at(table) {
                    Some(ty) => ty.element_type,
                    None => return Err(OperatorValidatorError::new("table index out of bounds")),
                };
                self.check_operands_1(Type::I32)?;
                self.func_state.change_frame_with_type(1, ty)?;
            }
            Operator::TableSet { table } => {
                self.check_reference_types_enabled()?;
                let ty = match resources.table_at(table) {
                    Some(ty) => ty.element_type,
                    None => return Err(OperatorValidatorError::new("table index out of bounds")),
                };
                self.check_operands_2(Type::I32, ty)?;
                self.func_state.change_frame(2)?;
            }
            Operator::TableGrow { table } => {
                self.check_reference_types_enabled()?;
                let ty = match resources.table_at(table) {
                    Some(ty) => ty.element_type,
                    None => return Err(OperatorValidatorError::new("table index out of bounds")),
                };
                self.check_operands_2(ty, Type::I32)?;
                self.func_state.change_frame_with_type(2, Type::I32)?;
            }
            Operator::TableSize { table } => {
                self.check_reference_types_enabled()?;
                if resources.table_at(table).is_none() {
                    return Err(OperatorValidatorError::new("table index out of bounds"));
                }
                self.func_state.change_frame_with_type(0, Type::I32)?;
            }
            Operator::TableFill { table } => {
                self.check_bulk_memory_enabled()?;
                let ty = match resources.table_at(table) {
                    Some(ty) => ty.element_type,
                    None => return Err(OperatorValidatorError::new("table index out of bounds")),
                };
                self.check_operands_3(Type::I32, ty, Type::I32)?;
                self.func_state.change_frame(3)?;
            }
        }
        Ok(FunctionEnd::No)
    }
}

fn func_type_at<T: WasmModuleResources>(
    resources: &T,
    at: u32,
) -> OperatorValidatorResult<&T::FuncType> {
    resources
        .func_type_at(at)
        .ok_or_else(|| OperatorValidatorError::new("unknown type: type index out of bounds"))
}
