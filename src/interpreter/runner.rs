use std::mem;
use std::ops;
use std::{u32, usize};
use std::fmt::{self, Display};
use std::iter::repeat;
use std::collections::{HashMap, VecDeque};
use elements::{Opcode, BlockType, Local};
use interpreter::Error;
use interpreter::store::{Store, FuncId, ModuleId, FuncInstance};
use interpreter::module::{CallerContext, FunctionSignature};
use interpreter::value::{
	RuntimeValue, TryInto, WrapInto, TryTruncateInto, ExtendInto,
	ArithmeticOps, Integer, Float, LittleEndianConvert, TransmuteInto,
};
use interpreter::variable::VariableInstance;
use common::{DEFAULT_MEMORY_INDEX, DEFAULT_TABLE_INDEX, BlockFrame, BlockFrameType};
use common::stack::StackWithLimit;

/// Function interpreter.
pub struct Interpreter<'store> {
	store: &'store mut Store,
}

/// Function execution context.
pub struct FunctionContext {
	/// Is context initialized.
	pub is_initialized: bool,
	/// Internal function reference.
	pub function: FuncId,
	pub module: ModuleId,
	/// Function return type.
	pub return_type: BlockType,
	/// Local variables.
	pub locals: Vec<VariableInstance>,
	/// Values stack.
	pub value_stack: StackWithLimit<RuntimeValue>,
	/// Blocks frames stack.
	pub frame_stack: StackWithLimit<BlockFrame>,
	/// Current instruction position.
	pub position: usize,
}

/// Interpreter action to execute after executing instruction.
#[derive(Debug)]
pub enum InstructionOutcome {
	/// Continue with next instruction.
	RunNextInstruction,
	/// Branch to given frame.
	Branch(usize),
	/// Execute function call.
	ExecuteCall(FuncId),
	/// End current frame.
	End,
	/// Return from current function block.
	Return,
}

/// Function run result.
enum RunResult {
	/// Function has returned (optional) value.
	Return(Option<RuntimeValue>),
	/// Function is calling other function.
	NestedCall(FunctionContext),
}

impl<'store> Interpreter<'store> {
	pub fn new(store: &mut Store) -> Interpreter {
		Interpreter {
			store
		}
	}

	pub fn run_function(&mut self, function_context: FunctionContext) -> Result<Option<RuntimeValue>, Error> {
		let mut function_stack = VecDeque::new();
		function_stack.push_back(function_context);

		loop {
			let mut function_context = function_stack.pop_back().expect("on loop entry - not empty; on loop continue - checking for emptiness; qed");
			let function_ref = function_context.function;
			let function_return = {
				let function_body = function_ref.resolve(self.store).body();

				match function_body {
					Some(function_body) => {
						if !function_context.is_initialized() {
							let return_type = function_context.return_type;
							function_context.initialize(&function_body.locals)?;
							function_context.push_frame(&function_body.labels, BlockFrameType::Function, return_type)?;
						}

						self.do_run_function(&mut function_context, function_body.opcodes.elements(), &function_body.labels)?
					},
					None => {
						// move locals back to the stack
						let locals_to_move: Vec<_> = function_context.locals.drain(..).collect();
						for local in locals_to_move {
							function_context.value_stack_mut().push(local.get())?;
						}
						let nested_context = CallerContext::nested(&mut function_context);

						// TODO: Call host functions
						// let result = function_ref.module.call_internal_function(nested_context, function_ref.internal_index)?;
						// RunResult::Return(result)

						panic!()
					},
				}
			};

			match function_return {
				RunResult::Return(return_value) => {
					match function_stack.back_mut() {
						Some(caller_context) => if let Some(return_value) = return_value {
							caller_context.value_stack_mut().push(return_value)?;
						},
						None => return Ok(return_value),
					}
				},
				RunResult::NestedCall(nested_context) => {
					function_stack.push_back(function_context);
					function_stack.push_back(nested_context);
				},
			}
		}
	}

	fn do_run_function<'a>(&mut self, function_context: &mut FunctionContext, function_body: &[Opcode], function_labels: &HashMap<usize, usize>) -> Result<RunResult, Error> {
		loop {
			let instruction = &function_body[function_context.position];

			debug!(target: "interpreter", "running {:?}", instruction);
			match self.run_instruction(function_context, function_labels, instruction)? {
				InstructionOutcome::RunNextInstruction => function_context.position += 1,
				InstructionOutcome::Branch(mut index) => {
					// discard index - 1 blocks
					while index >= 1 {
						function_context.discard_frame()?;
						index -= 1;
					}

					function_context.pop_frame(true)?;
					if function_context.frame_stack().is_empty() {
						break;
					}
				},
				InstructionOutcome::ExecuteCall(func_ref) => {
					function_context.position += 1;
					return Ok(RunResult::NestedCall(function_context.nested(self.store, func_ref)?));
				},
				InstructionOutcome::End => {
					if function_context.frame_stack().is_empty() {
						break;
					}
				},
				InstructionOutcome::Return => break,
			}
		}

		Ok(RunResult::Return(match function_context.return_type {
			BlockType::Value(_) => Some(function_context.value_stack_mut().pop()?),
			BlockType::NoResult => None,
		}))
	}

	fn run_instruction<'a>(&mut self, context: &mut FunctionContext, labels: &HashMap<usize, usize>, opcode: &Opcode) -> Result<InstructionOutcome, Error> {
		match opcode {
			&Opcode::Unreachable => self.run_unreachable(context),
			&Opcode::Nop => self.run_nop(context),
			&Opcode::Block(block_type) => self.run_block(context, labels, block_type),
			&Opcode::Loop(block_type) => self.run_loop(context, labels, block_type),
			&Opcode::If(block_type) => self.run_if(context, labels, block_type),
			&Opcode::Else => self.run_else(context, labels),
			&Opcode::End => self.run_end(context),
			&Opcode::Br(idx) => self.run_br(context, idx),
			&Opcode::BrIf(idx) => self.run_br_if(context, idx),
			&Opcode::BrTable(ref table, default) => self.run_br_table(context, table, default),
			&Opcode::Return => self.run_return(context),

			&Opcode::Call(index) => self.run_call(context, index),
			&Opcode::CallIndirect(index, _reserved) => self.run_call_indirect(context, index),

			&Opcode::Drop => self.run_drop(context),
			&Opcode::Select => self.run_select(context),

			&Opcode::GetLocal(index) => self.run_get_local(context, index),
			&Opcode::SetLocal(index) => self.run_set_local(context, index),
			&Opcode::TeeLocal(index) => self.run_tee_local(context, index),
			&Opcode::GetGlobal(index) => self.run_get_global(context, index),
			&Opcode::SetGlobal(index) => self.run_set_global(context, index),

			&Opcode::I32Load(align, offset) => self.run_load::<i32>(context, align, offset),
			&Opcode::I64Load(align, offset) => self.run_load::<i64>(context, align, offset),
			&Opcode::F32Load(align, offset) => self.run_load::<f32>(context, align, offset),
			&Opcode::F64Load(align, offset) => self.run_load::<f64>(context, align, offset),
			&Opcode::I32Load8S(align, offset) => self.run_load_extend::<i8, i32>(context, align, offset),
			&Opcode::I32Load8U(align, offset) => self.run_load_extend::<u8, i32>(context, align, offset),
			&Opcode::I32Load16S(align, offset) => self.run_load_extend::<i16, i32>(context, align, offset),
			&Opcode::I32Load16U(align, offset) => self.run_load_extend::<u16, i32>(context, align, offset),
			&Opcode::I64Load8S(align, offset) => self.run_load_extend::<i8, i64>(context, align, offset),
			&Opcode::I64Load8U(align, offset) => self.run_load_extend::<u8, i64>(context, align, offset),
			&Opcode::I64Load16S(align, offset) => self.run_load_extend::<i16, i64>(context, align, offset),
			&Opcode::I64Load16U(align, offset) => self.run_load_extend::<u16, i64>(context, align, offset),
			&Opcode::I64Load32S(align, offset) => self.run_load_extend::<i32, i64>(context, align, offset),
			&Opcode::I64Load32U(align, offset) => self.run_load_extend::<u32, i64>(context, align, offset),

			&Opcode::I32Store(align, offset) => self.run_store::<i32>(context, align, offset),
			&Opcode::I64Store(align, offset) => self.run_store::<i64>(context, align, offset),
			&Opcode::F32Store(align, offset) => self.run_store::<f32>(context, align, offset),
			&Opcode::F64Store(align, offset) => self.run_store::<f64>(context, align, offset),
			&Opcode::I32Store8(align, offset) => self.run_store_wrap::<i32, i8>(context, align, offset),
			&Opcode::I32Store16(align, offset) => self.run_store_wrap::<i32, i16>(context, align, offset),
			&Opcode::I64Store8(align, offset) => self.run_store_wrap::<i64, i8>(context, align, offset),
			&Opcode::I64Store16(align, offset) => self.run_store_wrap::<i64, i16>(context, align, offset),
			&Opcode::I64Store32(align, offset) => self.run_store_wrap::<i64, i32>(context, align, offset),

			&Opcode::CurrentMemory(_) => self.run_current_memory(context),
			&Opcode::GrowMemory(_) => self.run_grow_memory(context),

			&Opcode::I32Const(val) => self.run_const(context, val.into()),
			&Opcode::I64Const(val) => self.run_const(context, val.into()),
			&Opcode::F32Const(val) => self.run_const(context, RuntimeValue::decode_f32(val)),
			&Opcode::F64Const(val) => self.run_const(context, RuntimeValue::decode_f64(val)),

			&Opcode::I32Eqz => self.run_eqz::<i32>(context),
			&Opcode::I32Eq => self.run_eq::<i32>(context),
			&Opcode::I32Ne => self.run_ne::<i32>(context),
			&Opcode::I32LtS => self.run_lt::<i32>(context),
			&Opcode::I32LtU => self.run_lt::<u32>(context),
			&Opcode::I32GtS => self.run_gt::<i32>(context),
			&Opcode::I32GtU => self.run_gt::<u32>(context),
			&Opcode::I32LeS => self.run_lte::<i32>(context),
			&Opcode::I32LeU => self.run_lte::<u32>(context),
			&Opcode::I32GeS => self.run_gte::<i32>(context),
			&Opcode::I32GeU => self.run_gte::<u32>(context),

			&Opcode::I64Eqz => self.run_eqz::<i64>(context),
			&Opcode::I64Eq => self.run_eq::<i64>(context),
			&Opcode::I64Ne => self.run_ne::<i64>(context),
			&Opcode::I64LtS => self.run_lt::<i64>(context),
			&Opcode::I64LtU => self.run_lt::<u64>(context),
			&Opcode::I64GtS => self.run_gt::<i64>(context),
			&Opcode::I64GtU => self.run_gt::<u64>(context),
			&Opcode::I64LeS => self.run_lte::<i64>(context),
			&Opcode::I64LeU => self.run_lte::<u64>(context),
			&Opcode::I64GeS => self.run_gte::<i64>(context),
			&Opcode::I64GeU => self.run_gte::<u64>(context),

			&Opcode::F32Eq => self.run_eq::<f32>(context),
			&Opcode::F32Ne => self.run_ne::<f32>(context),
			&Opcode::F32Lt => self.run_lt::<f32>(context),
			&Opcode::F32Gt => self.run_gt::<f32>(context),
			&Opcode::F32Le => self.run_lte::<f32>(context),
			&Opcode::F32Ge => self.run_gte::<f32>(context),

			&Opcode::F64Eq => self.run_eq::<f64>(context),
			&Opcode::F64Ne => self.run_ne::<f64>(context),
			&Opcode::F64Lt => self.run_lt::<f64>(context),
			&Opcode::F64Gt => self.run_gt::<f64>(context),
			&Opcode::F64Le => self.run_lte::<f64>(context),
			&Opcode::F64Ge => self.run_gte::<f64>(context),

			&Opcode::I32Clz => self.run_clz::<i32>(context),
			&Opcode::I32Ctz => self.run_ctz::<i32>(context),
			&Opcode::I32Popcnt => self.run_popcnt::<i32>(context),
			&Opcode::I32Add => self.run_add::<i32>(context),
			&Opcode::I32Sub => self.run_sub::<i32>(context),
			&Opcode::I32Mul => self.run_mul::<i32>(context),
			&Opcode::I32DivS => self.run_div::<i32, i32>(context),
			&Opcode::I32DivU => self.run_div::<i32, u32>(context),
			&Opcode::I32RemS => self.run_rem::<i32, i32>(context),
			&Opcode::I32RemU => self.run_rem::<i32, u32>(context),
			&Opcode::I32And => self.run_and::<i32>(context),
			&Opcode::I32Or => self.run_or::<i32>(context),
			&Opcode::I32Xor => self.run_xor::<i32>(context),
			&Opcode::I32Shl => self.run_shl::<i32>(context, 0x1F),
			&Opcode::I32ShrS => self.run_shr::<i32, i32>(context, 0x1F),
			&Opcode::I32ShrU => self.run_shr::<i32, u32>(context, 0x1F),
			&Opcode::I32Rotl => self.run_rotl::<i32>(context),
			&Opcode::I32Rotr => self.run_rotr::<i32>(context),

			&Opcode::I64Clz => self.run_clz::<i64>(context),
			&Opcode::I64Ctz => self.run_ctz::<i64>(context),
			&Opcode::I64Popcnt => self.run_popcnt::<i64>(context),
			&Opcode::I64Add => self.run_add::<i64>(context),
			&Opcode::I64Sub => self.run_sub::<i64>(context),
			&Opcode::I64Mul => self.run_mul::<i64>(context),
			&Opcode::I64DivS => self.run_div::<i64, i64>(context),
			&Opcode::I64DivU => self.run_div::<i64, u64>(context),
			&Opcode::I64RemS => self.run_rem::<i64, i64>(context),
			&Opcode::I64RemU => self.run_rem::<i64, u64>(context),
			&Opcode::I64And => self.run_and::<i64>(context),
			&Opcode::I64Or => self.run_or::<i64>(context),
			&Opcode::I64Xor => self.run_xor::<i64>(context),
			&Opcode::I64Shl => self.run_shl::<i64>(context, 0x3F),
			&Opcode::I64ShrS => self.run_shr::<i64, i64>(context, 0x3F),
			&Opcode::I64ShrU => self.run_shr::<i64, u64>(context, 0x3F),
			&Opcode::I64Rotl => self.run_rotl::<i64>(context),
			&Opcode::I64Rotr => self.run_rotr::<i64>(context),

			&Opcode::F32Abs => self.run_abs::<f32>(context),
			&Opcode::F32Neg => self.run_neg::<f32>(context),
			&Opcode::F32Ceil => self.run_ceil::<f32>(context),
			&Opcode::F32Floor => self.run_floor::<f32>(context),
			&Opcode::F32Trunc => self.run_trunc::<f32>(context),
			&Opcode::F32Nearest => self.run_nearest::<f32>(context),
			&Opcode::F32Sqrt => self.run_sqrt::<f32>(context),
			&Opcode::F32Add => self.run_add::<f32>(context),
			&Opcode::F32Sub => self.run_sub::<f32>(context),
			&Opcode::F32Mul => self.run_mul::<f32>(context),
			&Opcode::F32Div => self.run_div::<f32, f32>(context),
			&Opcode::F32Min => self.run_min::<f32>(context),
			&Opcode::F32Max => self.run_max::<f32>(context),
			&Opcode::F32Copysign => self.run_copysign::<f32>(context),

			&Opcode::F64Abs => self.run_abs::<f64>(context),
			&Opcode::F64Neg => self.run_neg::<f64>(context),
			&Opcode::F64Ceil => self.run_ceil::<f64>(context),
			&Opcode::F64Floor => self.run_floor::<f64>(context),
			&Opcode::F64Trunc => self.run_trunc::<f64>(context),
			&Opcode::F64Nearest => self.run_nearest::<f64>(context),
			&Opcode::F64Sqrt => self.run_sqrt::<f64>(context),
			&Opcode::F64Add => self.run_add::<f64>(context),
			&Opcode::F64Sub => self.run_sub::<f64>(context),
			&Opcode::F64Mul => self.run_mul::<f64>(context),
			&Opcode::F64Div => self.run_div::<f64, f64>(context),
			&Opcode::F64Min => self.run_min::<f64>(context),
			&Opcode::F64Max => self.run_max::<f64>(context),
			&Opcode::F64Copysign => self.run_copysign::<f64>(context),

			&Opcode::I32WarpI64 => self.run_wrap::<i64, i32>(context),
			&Opcode::I32TruncSF32 => self.run_trunc_to_int::<f32, i32, i32>(context),
			&Opcode::I32TruncUF32 => self.run_trunc_to_int::<f32, u32, i32>(context),
			&Opcode::I32TruncSF64 => self.run_trunc_to_int::<f64, i32, i32>(context),
			&Opcode::I32TruncUF64 => self.run_trunc_to_int::<f64, u32, i32>(context),
			&Opcode::I64ExtendSI32 => self.run_extend::<i32, i64, i64>(context),
			&Opcode::I64ExtendUI32 => self.run_extend::<u32, u64, i64>(context),
			&Opcode::I64TruncSF32 => self.run_trunc_to_int::<f32, i64, i64>(context),
			&Opcode::I64TruncUF32 => self.run_trunc_to_int::<f32, u64, i64>(context),
			&Opcode::I64TruncSF64 => self.run_trunc_to_int::<f64, i64, i64>(context),
			&Opcode::I64TruncUF64 => self.run_trunc_to_int::<f64, u64, i64>(context),
			&Opcode::F32ConvertSI32 => self.run_extend::<i32, f32, f32>(context),
			&Opcode::F32ConvertUI32 => self.run_extend::<u32, f32, f32>(context),
			&Opcode::F32ConvertSI64 => self.run_wrap::<i64, f32>(context),
			&Opcode::F32ConvertUI64 => self.run_wrap::<u64, f32>(context),
			&Opcode::F32DemoteF64 => self.run_wrap::<f64, f32>(context),
			&Opcode::F64ConvertSI32 => self.run_extend::<i32, f64, f64>(context),
			&Opcode::F64ConvertUI32 => self.run_extend::<u32, f64, f64>(context),
			&Opcode::F64ConvertSI64 => self.run_extend::<i64, f64, f64>(context),
			&Opcode::F64ConvertUI64 => self.run_extend::<u64, f64, f64>(context),
			&Opcode::F64PromoteF32 => self.run_extend::<f32, f64, f64>(context),

			&Opcode::I32ReinterpretF32 => self.run_reinterpret::<f32, i32>(context),
			&Opcode::I64ReinterpretF64 => self.run_reinterpret::<f64, i64>(context),
			&Opcode::F32ReinterpretI32 => self.run_reinterpret::<i32, f32>(context),
			&Opcode::F64ReinterpretI64 => self.run_reinterpret::<i64, f64>(context),
		}
	}

	fn run_unreachable<'a>(&mut self, _context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		Err(Error::Trap("programmatic".into()))
	}

	fn run_nop<'a>(&mut self, _context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_block<'a>(&mut self, context: &mut FunctionContext, labels: &HashMap<usize, usize>, block_type: BlockType) -> Result<InstructionOutcome, Error> {
		context.push_frame(labels, BlockFrameType::Block, block_type)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_loop<'a>(&mut self, context: &mut FunctionContext, labels: &HashMap<usize, usize>, block_type: BlockType) -> Result<InstructionOutcome, Error> {
		context.push_frame(labels, BlockFrameType::Loop, block_type)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_if<'a>(&mut self, context: &mut FunctionContext, labels: &HashMap<usize, usize>, block_type: BlockType) -> Result<InstructionOutcome, Error> {
		let branch = context.value_stack_mut().pop_as()?;
		let block_frame_type = if branch { BlockFrameType::IfTrue } else {
			let else_pos = labels[&context.position];
			if !labels.contains_key(&else_pos) {
				context.position = else_pos;
				return Ok(InstructionOutcome::RunNextInstruction);
			}

			context.position = else_pos;
			BlockFrameType::IfFalse
		};
		context.push_frame(labels, block_frame_type, block_type).map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_else<'a>(&mut self, context: &mut FunctionContext, labels: &HashMap<usize, usize>) -> Result<InstructionOutcome, Error> {
		let end_pos = labels[&context.position];
		context.pop_frame(false)?;
		context.position = end_pos;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_end<'a>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		context.pop_frame(false)?;
		Ok(InstructionOutcome::End)
	}

	fn run_br<'a>(&mut self, _context: &mut FunctionContext, label_idx: u32) -> Result<InstructionOutcome, Error> {
		Ok(InstructionOutcome::Branch(label_idx as usize))
	}

	fn run_br_if<'a>(&mut self, context: &mut FunctionContext, label_idx: u32) -> Result<InstructionOutcome, Error> {
		if context.value_stack_mut().pop_as()? {
			Ok(InstructionOutcome::Branch(label_idx as usize))
		} else {
			Ok(InstructionOutcome::RunNextInstruction)
		}
	}

	fn run_br_table<'a>(&mut self, context: &mut FunctionContext, table: &Vec<u32>, default: u32) -> Result<InstructionOutcome, Error> {
		let index: u32 = context.value_stack_mut().pop_as()?;
		Ok(InstructionOutcome::Branch(table.get(index as usize).cloned().unwrap_or(default) as usize))
	}

	fn run_return<'a>(&mut self, _context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		Ok(InstructionOutcome::Return)
	}

	fn run_call<'a>(&mut self, context: &mut FunctionContext, func_idx: u32) -> Result<InstructionOutcome, Error> {
		let func = context.module().resolve_func(self.store, func_idx);
		Ok(InstructionOutcome::ExecuteCall(func))
	}

	fn run_call_indirect<'a>(&mut self, context: &mut FunctionContext, type_idx: u32) -> Result<InstructionOutcome, Error> {
		let table_func_idx: u32 = context.value_stack_mut().pop_as()?;
		let table = context.module().resolve_table(self.store, DEFAULT_TABLE_INDEX).resolve(self.store);
		let func_ref = table.get(table_func_idx)?;

		let actual_function_type = func_ref.resolve(self.store).func_type().resolve(self.store);
		let required_function_type = context.module().resolve_type(self.store, type_idx).resolve(self.store);

		if required_function_type != actual_function_type {
			return Err(Error::Function(format!("expected function with signature ({:?}) -> {:?} when got with ({:?}) -> {:?}",
				required_function_type.params(), required_function_type.return_type(),
				actual_function_type.params(), actual_function_type.return_type())));
		}

		Ok(InstructionOutcome::ExecuteCall(func_ref))
	}

	fn run_drop<'a>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		context
			.value_stack_mut()
			.pop()
			.map_err(Into::into)
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_select<'a>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		context
			.value_stack_mut()
			.pop_triple()
			.and_then(|(left, mid, right)| {
				let right: Result<_, Error> = right.try_into();
				match (left, mid, right) {
					(left, mid, Ok(condition)) => Ok((left, mid, condition)),
					_ => Err(Error::Stack("expected to get int value from stack".into()))
				}
			})
			.map(|(left, mid, condition)| if condition { left } else { mid })
			.map(|val| context.value_stack_mut().push(val))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_get_local<'a>(&mut self, context: &mut FunctionContext, index: u32) -> Result<InstructionOutcome, Error> {
		context.get_local(index as usize)
			.map(|value| context.value_stack_mut().push(value))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_set_local<'a>(&mut self, context: &mut FunctionContext, index: u32) -> Result<InstructionOutcome, Error> {
		let arg = context.value_stack_mut().pop()?;
		context.set_local(index as usize, arg)
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_tee_local<'a>(&mut self, context: &mut FunctionContext, index: u32) -> Result<InstructionOutcome, Error> {
		let arg = context.value_stack().top()?.clone();
		context.set_local(index as usize, arg)
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_get_global<'a>(
		&mut self,
		context: &mut FunctionContext,
		index: u32,
	) -> Result<InstructionOutcome, Error> {
		let global = context.module().resolve_global(&self.store, index);
		let val = self.store.read_global(global);
		context.value_stack_mut().push(val).map_err(Into::into)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_set_global<'a>(&mut self, context: &mut FunctionContext, index: u32) -> Result<InstructionOutcome, Error> {
		let val = context
			.value_stack_mut()
			.pop()
			.map_err(Into::into)?;

		let global = context.module().resolve_global(&self.store, index);
		self.store.write_global(global, val)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_load<'a, T>(&mut self, context: &mut FunctionContext, _align: u32, offset: u32) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T>, T: LittleEndianConvert {
		let address = effective_address(offset, context.value_stack_mut().pop_as()?)?;
		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		let b = m.get(address, mem::size_of::<T>())?;
		let n = T::from_little_endian(b)?;
		context.value_stack_mut().push(n.into()).map_err(Into::into)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_load_extend<'a, T, U>(&mut self, context: &mut FunctionContext, _align: u32, offset: u32) -> Result<InstructionOutcome, Error>
		where T: ExtendInto<U>, RuntimeValue: From<U>, T: LittleEndianConvert {
		let address = effective_address(offset, context.value_stack_mut().pop_as()?)?;
		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		let b = m.get(address, mem::size_of::<T>())?;
		let v = T::from_little_endian(b)?;
		let stack_value: U = v.extend_into();
		context
			.value_stack_mut()
			.push(stack_value.into())
			.map_err(Into::into)
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_store<'a, T>(&mut self, context: &mut FunctionContext, _align: u32, offset: u32) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: LittleEndianConvert {
		let stack_value = context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|n| n.into_little_endian())?;
		let address = effective_address(offset, context.value_stack_mut().pop_as::<u32>()?)?;

		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		m.set(address, &stack_value)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_store_wrap<'a, T, U>(
		&mut self,
		context: &mut FunctionContext,
		_align: u32,
		offset: u32,
	) -> Result<InstructionOutcome, Error>
	where
		RuntimeValue: TryInto<T, Error>,
		T: WrapInto<U>,
		U: LittleEndianConvert,
	{
		let stack_value: T = context
			.value_stack_mut()
			.pop()
			.map_err(Into::into)
			.and_then(|v| v.try_into())?;
		let stack_value = stack_value.wrap_into().into_little_endian();
		let address = effective_address(offset, context.value_stack_mut().pop_as::<u32>()?)?;
		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		m.set(address, &stack_value)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_current_memory<'a>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		let s = m.size();
		context
			.value_stack_mut()
			.push(RuntimeValue::I32(s as i32))
			.map_err(Into::into)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_grow_memory<'a>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error> {
		let pages: u32 = context.value_stack_mut().pop_as()?;
		let m = context.module()
			.resolve_memory(self.store, DEFAULT_MEMORY_INDEX)
			.resolve(self.store);
		let m = m.grow(pages)?;
		context
			.value_stack_mut()
			.push(RuntimeValue::I32(m as i32))
			.map_err(Into::into)?;
		Ok(InstructionOutcome::RunNextInstruction)
	}

	fn run_const<'a>(&mut self, context: &mut FunctionContext, val: RuntimeValue) -> Result<InstructionOutcome, Error> {
		context
			.value_stack_mut()
			.push(val)
			.map_err(Into::into)
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_eqz<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialEq<T> + Default {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| RuntimeValue::I32(if v == Default::default() { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_eq<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialEq<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left == right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_ne<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialEq<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left != right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_lt<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialOrd<T> + Display {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left < right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_gt<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialOrd<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left > right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_lte<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialOrd<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left <= right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_gte<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: TryInto<T, Error>, T: PartialOrd<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| RuntimeValue::I32(if left >= right { 1 } else { 0 }))
			.and_then(|v| context.value_stack_mut().push(v).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_clz<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Integer<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.leading_zeros())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_ctz<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Integer<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.trailing_zeros())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_popcnt<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Integer<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.count_ones())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_add<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: ArithmeticOps<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.add(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_sub<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: ArithmeticOps<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.sub(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_mul<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: ArithmeticOps<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.mul(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_div<'a, T, U>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: TransmuteInto<U> + Display, U: ArithmeticOps<U> + TransmuteInto<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| (left.transmute_into(), right.transmute_into()))
			.map(|(left, right)| left.div(right))?
			.map(|v| v.transmute_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_rem<'a, T, U>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: TransmuteInto<U>, U: Integer<U> + TransmuteInto<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| (left.transmute_into(), right.transmute_into()))
			.map(|(left, right)| left.rem(right))?
			.map(|v| v.transmute_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_and<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<<T as ops::BitAnd>::Output> + TryInto<T, Error>, T: ops::BitAnd<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.bitand(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_or<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<<T as ops::BitOr>::Output> + TryInto<T, Error>, T: ops::BitOr<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.bitor(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_xor<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<<T as ops::BitXor>::Output> + TryInto<T, Error>, T: ops::BitXor<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.bitxor(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_shl<'a, T>(&mut self, context: &mut FunctionContext, mask: T) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<<T as ops::Shl<T>>::Output> + TryInto<T, Error>, T: ops::Shl<T> + ops::BitAnd<T, Output=T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.shl(right & mask))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_shr<'a, T, U>(&mut self, context: &mut FunctionContext, mask: U) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: TransmuteInto<U>, U: ops::Shr<U> + ops::BitAnd<U, Output=U>, <U as ops::Shr<U>>::Output: TransmuteInto<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| (left.transmute_into(), right.transmute_into()))
			.map(|(left, right)| left.shr(right & mask))
			.map(|v| v.transmute_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_rotl<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Integer<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.rotl(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_rotr<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Integer<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.rotr(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_abs<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.abs())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_neg<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<<T as ops::Neg>::Output> + TryInto<T, Error>, T: ops::Neg {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.neg())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_ceil<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.ceil())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_floor<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.floor())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_trunc<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.trunc())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_nearest<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.nearest())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_sqrt<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.sqrt())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_min<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.min(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_max<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.max(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_copysign<'a, T>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<T> + TryInto<T, Error>, T: Float<T> {
		context
			.value_stack_mut()
			.pop_pair_as::<T>()
			.map(|(left, right)| left.copysign(right))
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_wrap<'a, T, U>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<U> + TryInto<T, Error>, T: WrapInto<U> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(|v| v.wrap_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_trunc_to_int<'a, T, U, V>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<V> + TryInto<T, Error>, T: TryTruncateInto<U, Error>, U: TransmuteInto<V>,  {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.and_then(|v| v.try_truncate_into())
			.map(|v| v.transmute_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_extend<'a, T, U, V>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<V> + TryInto<T, Error>, T: ExtendInto<U>, U: TransmuteInto<V> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map_err(Error::into)
			.map(|v| v.extend_into())
			.map(|v| v.transmute_into())
			.map(|v| context.value_stack_mut().push(v.into()))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	fn run_reinterpret<'a, T, U>(&mut self, context: &mut FunctionContext) -> Result<InstructionOutcome, Error>
		where RuntimeValue: From<U>, RuntimeValue: TryInto<T, Error>, T: TransmuteInto<U> {
		context
			.value_stack_mut()
			.pop_as::<T>()
			.map(TransmuteInto::transmute_into)
			.and_then(|val| context.value_stack_mut().push(val.into()).map_err(Into::into))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}
}

impl<'a> FunctionContext {
	pub fn new(store: &Store, function: FuncId, value_stack_limit: usize, frame_stack_limit: usize, function_type: &FunctionSignature, args: Vec<VariableInstance>) -> Self {
		let func_instance = function.resolve(store);
		let module = match *func_instance {
			FuncInstance::Defined { module, .. } => module,
			FuncInstance::Host { .. } => panic!("Host functions can't be called as internally defined functions; Thus FunctionContext can be created only with internally defined functions; qed"),
		};
		FunctionContext {
			is_initialized: false,
			function: function,
			module: module,
			return_type: function_type.return_type().map(|vt| BlockType::Value(vt)).unwrap_or(BlockType::NoResult),
			value_stack: StackWithLimit::with_limit(value_stack_limit),
			frame_stack: StackWithLimit::with_limit(frame_stack_limit),
			locals: args,
			position: 0,
		}
	}

	pub fn nested(&mut self, store: &Store, function: FuncId) -> Result<Self, Error> {
		let (function_locals, module, function_return_type) = {
			let func_instance = function.resolve(store);
			let module = match *func_instance {
				FuncInstance::Defined { module, .. } => module,
				FuncInstance::Host { .. } => panic!("Host functions can't be called as internally defined functions; Thus FunctionContext can be created only with internally defined functions; qed"),
			};
			let function_type = func_instance.func_type().resolve(store);
			// TODO: function_signature
			let function_signature = FunctionSignature::Module(&function_type);
			let function_return_type = function_type.return_type().map(|vt| BlockType::Value(vt)).unwrap_or(BlockType::NoResult);
			let function_locals = prepare_function_args(&function_signature, &mut self.value_stack)?;
			(function_locals, module, function_return_type)
		};

		Ok(FunctionContext {
			is_initialized: false,
			function: function,
			module: module,
			return_type: function_return_type,
			value_stack: StackWithLimit::with_limit(self.value_stack.limit() - self.value_stack.len()),
			frame_stack: StackWithLimit::with_limit(self.frame_stack.limit() - self.frame_stack.len()),
			locals: function_locals,
			position: 0,
		})
	}

	pub fn is_initialized(&self) -> bool {
		self.is_initialized
	}

	pub fn initialize(&mut self, locals: &[Local]) -> Result<(), Error> {
		debug_assert!(!self.is_initialized);
		self.is_initialized = true;

		let locals = locals.iter()
			.flat_map(|l| repeat(l.value_type().into()).take(l.count() as usize))
			.map(|vt| VariableInstance::new(true, vt, RuntimeValue::default(vt)))
			.collect::<Result<Vec<_>, _>>()?;
		self.locals.extend(locals);
		Ok(())
	}

	pub fn module(&self) -> ModuleId {
		self.module
	}

	pub fn set_local(&mut self, index: usize, value: RuntimeValue) -> Result<InstructionOutcome, Error> {
		self.locals.get_mut(index)
			.ok_or(Error::Local(format!("expected to have local with index {}", index)))
			.and_then(|l| l.set(value))
			.map(|_| InstructionOutcome::RunNextInstruction)
	}

	pub fn get_local(&mut self, index: usize) -> Result<RuntimeValue, Error> {
		self.locals.get(index)
			.ok_or(Error::Local(format!("expected to have local with index {}", index)))
			.map(|l| l.get())
	}

	pub fn value_stack(&self) -> &StackWithLimit<RuntimeValue> {
		&self.value_stack
	}

	pub fn value_stack_mut(&mut self) -> &mut StackWithLimit<RuntimeValue> {
		&mut self.value_stack
	}

	pub fn frame_stack(&self) -> &StackWithLimit<BlockFrame> {
		&self.frame_stack
	}

	pub fn frame_stack_mut(&mut self) -> &mut StackWithLimit<BlockFrame> {
		&mut self.frame_stack
	}

	pub fn push_frame(&mut self, labels: &HashMap<usize, usize>, frame_type: BlockFrameType, block_type: BlockType) -> Result<(), Error> {
		let begin_position = self.position;
		let branch_position = match frame_type {
			BlockFrameType::Function => usize::MAX,
			BlockFrameType::Loop => begin_position,
			BlockFrameType::IfTrue => {
				let else_pos = labels[&begin_position];
				1usize + match labels.get(&else_pos) {
					Some(end_pos) => *end_pos,
					None => else_pos,
				}
			},
			_ => labels[&begin_position] + 1,
		};
		let end_position = match frame_type {
			BlockFrameType::Function => usize::MAX,
			_ => labels[&begin_position] + 1,
		};
		Ok(self.frame_stack.push(BlockFrame {
			frame_type: frame_type,
			block_type: block_type,
			begin_position: begin_position,
			branch_position: branch_position,
			end_position: end_position,
			value_stack_len: self.value_stack.len(),
		})?)
	}

	pub fn discard_frame(&mut self) -> Result<(), Error> {
		Ok(self.frame_stack.pop().map(|_| ())?)
	}

	pub fn pop_frame(&mut self, is_branch: bool) -> Result<(), Error> {
		let frame = self.frame_stack.pop()?;
		if frame.value_stack_len > self.value_stack.len() {
			return Err(Error::Stack("invalid stack len".into()));
		}

		let frame_value = match frame.block_type {
			BlockType::Value(_) if frame.frame_type != BlockFrameType::Loop || !is_branch => Some(self.value_stack.pop()?),
			_ => None,
		};
		self.value_stack.resize(frame.value_stack_len, RuntimeValue::I32(0));
		self.position = if is_branch { frame.branch_position } else { frame.end_position };
		if let Some(frame_value) = frame_value {
			self.value_stack.push(frame_value)?;
		}

		Ok(())
	}
}

impl<'a> fmt::Debug for FunctionContext {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "FunctionContext")
	}
}

fn effective_address(address: u32, offset: u32) -> Result<u32, Error> {
	match offset.checked_add(address) {
		None => Err(Error::Memory(format!("invalid memory access: {} + {}", offset, address))),
		Some(address) => Ok(address),
	}
}

pub fn prepare_function_args(function_type: &FunctionSignature, caller_stack: &mut StackWithLimit<RuntimeValue>) -> Result<Vec<VariableInstance>, Error> {
	let mut args = function_type.params().iter().rev().map(|param_type| {
		let param_value = caller_stack.pop()?;
		let actual_type = param_value.variable_type();
		let expected_type = (*param_type).into();
		if actual_type != Some(expected_type) {
			return Err(Error::Function(format!("invalid parameter type {:?} when expected {:?}", actual_type, expected_type)));
		}

		VariableInstance::new(true, expected_type, param_value)
	}).collect::<Result<Vec<_>, _>>()?;
	args.reverse();
	Ok(args)
}
