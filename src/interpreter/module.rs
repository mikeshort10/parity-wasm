use std::collections::{BTreeSet, HashMap};
use std::iter::repeat;
use std::sync::{Arc, Weak};
use elements::{Module, InitExpr, Opcode, Type, FunctionType, FuncBody, Internal, External, BlockType, ResizableLimits};
use interpreter::Error;
use interpreter::imports::ModuleImports;
use interpreter::memory::MemoryInstance;
use interpreter::program::ProgramInstanceEssence;
use interpreter::runner::{Interpreter, FunctionContext};
use interpreter::stack::StackWithLimit;
use interpreter::table::TableInstance;
use interpreter::validator::{Validator, FunctionValidationContext};
use interpreter::value::{RuntimeValue, TryInto};
use interpreter::variable::{VariableInstance, VariableType};

/// Maximum number of entries in value stack.
const DEFAULT_VALUE_STACK_LIMIT: usize = 16384;
/// Maximum number of entries in frame stack.
const DEFAULT_FRAME_STACK_LIMIT: usize = 1024;

#[derive(Default, Clone)]
/// Execution context.
pub struct ExecutionParams<'a> {
	/// Arguments.
	pub args: Vec<RuntimeValue>,
	/// Execution-local external modules.
	pub externals: HashMap<String, Arc<ModuleInstanceInterface + 'a>>,
}

/// Module instance API.
pub trait ModuleInstanceInterface {
	/// Execute start function of the module.
	fn execute_main(&self, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error>;
	/// Execute function with the given index.
	fn execute_index(&self, index: u32, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error>;
	/// Execute function with the given export name.
	fn execute_export(&self, name: &str, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error>;
	/// Get export entry.
	fn export_entry(&self, name: &str) -> Result<Internal, Error>;
	/// Get table reference.
	fn table(&self, index: ItemIndex) -> Result<Arc<TableInstance>, Error>;
	/// Get memory reference.
	fn memory(&self, index: ItemIndex) -> Result<Arc<MemoryInstance>, Error>;
	/// Get global reference.
	fn global(&self, index: ItemIndex) -> Result<Arc<VariableInstance>, Error>;
	/// Call function with given index in functions index space.
	fn call_function(&self, outer: CallerContext, index: ItemIndex) -> Result<Option<RuntimeValue>, Error>;
	/// Call function with given index in the given table.
	fn call_function_indirect(&self, outer: CallerContext, table_index: ItemIndex, type_index: u32, func_index: u32) -> Result<Option<RuntimeValue>, Error>;
	/// Call function with internal index.
	fn call_internal_function(&self, outer: CallerContext, index: u32, function_type: Option<&FunctionType>) -> Result<Option<RuntimeValue>, Error>;
}

/// Item index in items index space.
#[derive(Debug, Clone, Copy)]
pub enum ItemIndex {
	/// Index in index space.
	IndexSpace(u32),
	/// Internal item index (i.e. index of item in items section).
	Internal(u32),
	/// External module item index (i.e. index of item in the import section).
	External(u32),
}

/// Module instance.
pub struct ModuleInstance {
	/// Module.
	module: Module,
	/// Module imports.
	imports: ModuleImports,
	/// Tables.
	tables: Vec<Arc<TableInstance>>,
	/// Linear memory regions.
	memory: Vec<Arc<MemoryInstance>>,
	/// Globals.
	globals: Vec<Arc<VariableInstance>>,
}

/// Caller context.
pub struct CallerContext<'a> {
	/// Value stack limit
	pub value_stack_limit: usize,
	/// Frame stack limit
	pub frame_stack_limit: usize,
	/// Stack of the input parameters
	pub value_stack: &'a mut StackWithLimit<RuntimeValue>,
	/// Execution-local external modules.
	pub externals: &'a HashMap<String, Arc<ModuleInstanceInterface + 'a>>,
}

impl<'a> ExecutionParams<'a> {
	/// Create new execution params with given externa; module override.
	pub fn with_external(name: String, module: Arc<ModuleInstanceInterface + 'a>) -> Self {
		let mut externals = HashMap::new();
		externals.insert(name, module);
		ExecutionParams {
			args: Vec::new(),
			externals: externals,
		}
	}

	/// Add argument.
	pub fn add_argument(mut self, arg: RuntimeValue) -> Self {
		self.args.push(arg);
		self
	}
}

impl<'a> From<Vec<RuntimeValue>> for ExecutionParams<'a> {
	fn from(args: Vec<RuntimeValue>) -> ExecutionParams<'a> {
		ExecutionParams {
			args: args,
			externals: HashMap::new(),
		}
	}
}

impl ModuleInstance {
	/// Instantiate given module within program context.
	pub fn new(program: Weak<ProgramInstanceEssence>, module: Module) -> Result<Self, Error> {
		Self::new_with_validation_flag(program, module, true)
	}

	/// Instantiate given module within program context.
	pub fn new_with_validation_flag(program: Weak<ProgramInstanceEssence>, module: Module, is_user_module: bool) -> Result<Self, Error> {
		// load entries from import section
		let imports = ModuleImports::new(program, module.import_section());

		// instantiate linear memory regions, if any
		let memory = match module.memory_section() {
			Some(memory_section) => memory_section.entries()
										.iter()
										.map(MemoryInstance::new)
										.collect::<Result<Vec<_>, _>>()?,
			None => Vec::new(),
		};

		// instantiate tables, if any
		let tables = match module.table_section() {
			Some(table_section) => table_section.entries()
										.iter()
										.map(|tt| TableInstance::new(VariableType::AnyFunc, tt)) // TODO: actual table type
										.collect::<Result<Vec<_>, _>>()?,
			None => Vec::new(),
		};

		// instantiate globals, if any
		let globals = match module.global_section() {
			Some(global_section) => global_section.entries()
										.iter()
										.map(|g| {
											get_initializer(g.init_expr(), &module, &imports)
												.map_err(|e| Error::Initialization(e.into()))
												.and_then(|v| VariableInstance::new_global(g.global_type(), v).map(Arc::new))
										})
										.collect::<Result<Vec<_>, _>>()?,
			None => Vec::new(),
		};

		let mut module = ModuleInstance {
			module: module,
			imports: imports,
			memory: memory,
			tables: tables,
			globals: globals,
		};
		module.complete_initialization(is_user_module)?;
		Ok(module)
	}

	/// Complete module initialization.
	fn complete_initialization(&mut self, is_user_module: bool) -> Result<(), Error> {
		// validate start section
		if let Some(start_function) = self.module.start_section() {
			self.require_function(ItemIndex::IndexSpace(start_function))?;
		}

		// validate export section
		if is_user_module { // TODO: env module exports STACKTOP global, which is mutable => check is failed
			if let Some(export_section) = self.module.export_section() {
				// duplicate name check is not on specification, but it seems logical
				let mut names = BTreeSet::new();
				for export in export_section.entries() {
					if !names.insert(export.field()) {
						return Err(Error::Validation(format!("duplicate export with name {}", export.field())));
					}

					// this allows reexporting
					match export.internal() {
						&Internal::Function(function_index) =>
							self.require_function(ItemIndex::IndexSpace(function_index))?,
						&Internal::Global(global_index) =>
							self.global(ItemIndex::IndexSpace(global_index))
								.and_then(|g| if g.is_mutable() {
									Err(Error::Validation(format!("trying to export mutable global {}", export.field())))
								} else {
									Ok(())
								})?,
						&Internal::Memory(memory_index) =>
							self.memory(ItemIndex::IndexSpace(memory_index)).map(|_| ())?,
						&Internal::Table(table_index) =>
							self.table(ItemIndex::IndexSpace(table_index)).map(|_| ())?,
					}
				}
			}
		}

		// validate import section
		if let Some(import_section) = self.module.import_section() {
			// external module + its export existance is checked on runtime
			for import in import_section.entries() {
				match import.external() {
					&External::Function(ref function_index) => self.require_function_type(*function_index).map(|_| ())?,
					&External::Global(ref global_type) => if global_type.is_mutable() {
						return Err(Error::Validation(format!("trying to import mutable global {}", import.field())))
					},
					&External::Memory(ref memory_type) => check_limits(memory_type.limits())?,
					&External::Table(ref table_type) => check_limits(table_type.limits())?,
				}
			}
		}

		// there must be no greater than 1 table in tables index space
		if self.imports.tables_len() + self.tables.len() > 1 {
			return Err(Error::Validation(format!("too many tables in index space: {}", self.imports.tables_len() + self.tables.len())));
		}

		// there must be no greater than 1 memory region in memory regions index space
		if self.imports.memory_regions_len() + self.memory.len() > 1 {
			return Err(Error::Validation(format!("too many memory regions in index space: {}", self.imports.memory_regions_len() + self.memory.len())));
		}

		// for every function section entry there must be corresponding entry in code section and type && vice versa
		let function_section_len = self.module.function_section().map(|s| s.entries().len()).unwrap_or(0);
		let code_section_len = self.module.code_section().map(|s| s.bodies().len()).unwrap_or(0);
		if function_section_len != code_section_len {
			return Err(Error::Validation(format!("length of function section is {}, while len of code section is {}", function_section_len, code_section_len)));
		}

		// validate every function body in user modules
		if is_user_module && function_section_len != 0 {
			let function_section = self.module.function_section().expect("function_section_len != 0; qed");
			let code_section = self.module.code_section().expect("function_section_len != 0; function_section_len == code_section_len; qed");
			// check every function body
			for (index, function) in function_section.entries().iter().enumerate() {
				let function_type = self.require_function_type(function.type_ref())?;
				let function_body = code_section.bodies().get(index as usize).ok_or(Error::Validation(format!("Missing body for function {}", index)))?;
				let mut locals = function_type.params().to_vec();
				locals.extend(function_body.locals().iter().flat_map(|l| repeat(l.value_type()).take(l.count() as usize)));
				let mut context = FunctionValidationContext::new(&self.module, &self.imports, &locals, DEFAULT_VALUE_STACK_LIMIT, DEFAULT_FRAME_STACK_LIMIT, &function_type);
				let block_type = function_type.return_type().map(BlockType::Value).unwrap_or(BlockType::NoResult);
				Validator::validate_block(&mut context, block_type, function_body.code().elements(), Opcode::End)?;
			}
		}

		// use data section to initialize linear memory regions
		if let Some(data_section) = self.module.data_section() {
			for (data_segment_index, data_segment) in data_section.entries().iter().enumerate() {
				let offset: u32 = get_initializer(data_segment.offset(), &self.module, &self.imports)?.try_into()?;
				self.memory(ItemIndex::IndexSpace(data_segment.index()))
					.map_err(|e| Error::Initialization(format!("DataSegment {} initializes non-existant MemoryInstance {}: {:?}", data_segment_index, data_segment.index(), e)))
					.and_then(|m| m.set(offset, data_segment.value()))
					.map_err(|e| Error::Initialization(e.into()))?;
			}
		}

		// use element section to fill tables
		if let Some(element_section) = self.module.elements_section() {
			for (element_segment_index, element_segment) in element_section.entries().iter().enumerate() {
				let offset: u32 = get_initializer(element_segment.offset(), &self.module, &self.imports)?.try_into()?;
				for function_index in element_segment.members() {
					self.require_function(ItemIndex::IndexSpace(*function_index))?;
				}

				self.table(ItemIndex::IndexSpace(element_segment.index()))
					.map_err(|e| Error::Initialization(format!("ElementSegment {} initializes non-existant Table {}: {:?}", element_segment_index, element_segment.index(), e)))
					.and_then(|m| m.set_raw(offset, element_segment.members()))
					.map_err(|e| Error::Initialization(e.into()))?;
			}
		}

		Ok(())
	}

	fn require_function(&self, index: ItemIndex) -> Result<(), Error> {
		match self.imports.parse_function_index(index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_function_index resolves IndexSpace option"),
			ItemIndex::Internal(index) => self.module.function_section()
				.ok_or(Error::Function(format!("initializing table element to value {}, without corresponding internal function", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Function(format!("initializing table element to value {}, without corresponding internal function", index))))
				.map(|_| ()),
			ItemIndex::External(index) => self.module.import_section()
				.ok_or(Error::Function(format!("initializing table element to value {}, without corresponding external function", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Function(format!("initializing table element to value {}, without corresponding external function", index))))
				.map(|_| ()),
		}
	}

	fn require_function_type(&self, type_index: u32) -> Result<&FunctionType, Error> {
		self.module.type_section()
			.ok_or(Error::Validation(format!("type reference {} exists in module without type section", type_index)))
			.and_then(|s| match s.types().get(type_index as usize) {
				Some(&Type::Function(ref function_type)) => Ok(function_type),
				_ => Err(Error::Validation(format!("missing function type with index {}", type_index))),
			})
	}
}

impl ModuleInstanceInterface for ModuleInstance {
	fn execute_main(&self, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error> {
		let index = self.module.start_section().ok_or(Error::Program("module has no start section".into()))?;
		self.execute_index(index, params)
	}

	fn execute_index(&self, index: u32, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error> {
		let args_len = params.args.len();
		let mut args = StackWithLimit::with_data(params.args, args_len);
		let caller_context = CallerContext::topmost(&mut args, &params.externals);
		self.call_function(caller_context, ItemIndex::IndexSpace(index))
	}

	fn execute_export(&self, name: &str, params: ExecutionParams) -> Result<Option<RuntimeValue>, Error> {
		let index = self.module.export_section()
			.ok_or(Error::Function("missing export section".into()))
			.and_then(|s| s.entries().iter()
				.find(|e| e.field() == name && match e.internal() {
					&Internal::Function(_) => true,
					_ => false,
				})
				.ok_or(Error::Function(format!("missing export section exported function with name {}", name)))
				.map(|e| match e.internal() {
					&Internal::Function(index) => index,
					_ => unreachable!(), // checked couple of lines above
				})
			)?;
		self.execute_index(index, params)
	}

	fn export_entry(&self, name: &str) -> Result<Internal, Error> {
		self.module.export_section()
			.ok_or(Error::Program(format!("trying to import {} from module without export section", name)))
			.and_then(|s| s.entries().iter()
				.find(|e| e.field() == name)
				.map(|e| *e.internal())
				.ok_or(Error::Program(format!("unresolved import {}", name))))
	}

	fn table(&self, index: ItemIndex) -> Result<Arc<TableInstance>, Error> {
		match self.imports.parse_table_index(index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_table_index resolves IndexSpace option"),
			ItemIndex::Internal(index) => self.tables.get(index as usize).cloned()
				.ok_or(Error::Table(format!("trying to access table with local index {} when there are only {} local tables", index, self.tables.len()))),
			ItemIndex::External(index) => self.module.import_section()
				.ok_or(Error::Table(format!("trying to access external table with index {} in module without import section", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Table(format!("trying to access external table with index {} in module with {}-entries import section", index, s.entries().len()))))
				.and_then(|e| self.imports.table(None, e)),
		}
	}

	fn memory(&self, index: ItemIndex) -> Result<Arc<MemoryInstance>, Error> {
		match self.imports.parse_memory_index(index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_memory_index resolves IndexSpace option"),
			ItemIndex::Internal(index) => self.memory.get(index as usize).cloned()
				.ok_or(Error::Memory(format!("trying to access memory with local index {} when there are only {} memory regions", index, self.memory.len()))),
			ItemIndex::External(index) => self.module.import_section()
				.ok_or(Error::Memory(format!("trying to access external memory with index {} in module without import section", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Memory(format!("trying to access external memory with index {} in module with {}-entries import section", index, s.entries().len()))))
				.and_then(|e| self.imports.memory(None, e)),
		}
	}

	fn global(&self, index: ItemIndex) -> Result<Arc<VariableInstance>, Error> {
		match self.imports.parse_global_index(index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_global_index resolves IndexSpace option"),
			ItemIndex::Internal(index) => self.globals.get(index as usize).cloned()
				.ok_or(Error::Global(format!("trying to access global with local index {} when there are only {} globals", index, self.globals.len()))),
			ItemIndex::External(index) => self.module.import_section()
				.ok_or(Error::Global(format!("trying to access external global with index {} in module without import section", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Global(format!("trying to access external global with index {} in module with {}-entries import section", index, s.entries().len()))))
				.and_then(|e| self.imports.global(None, e)),
		}
	}

	fn call_function(&self, outer: CallerContext, index: ItemIndex) -> Result<Option<RuntimeValue>, Error> {
		match self.imports.parse_function_index(index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_function_index resolves IndexSpace option"),
			ItemIndex::Internal(index) => self.call_internal_function(outer, index, None),
			ItemIndex::External(index) =>
				self.module.import_section()
				.ok_or(Error::Function(format!("trying to access external function with index {} in module without import section", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Function(format!("trying to access external function with index {} in module with {}-entries import section", index, s.entries().len()))))
				.and_then(|e| Ok((self.imports.module(Some(outer.externals), e.module())?, self.imports.function(Some(outer.externals), e)?)))
				.and_then(|(m, index)| m.call_internal_function(outer, index, None)),
		}
	}

	fn call_function_indirect(&self, outer: CallerContext, table_index: ItemIndex, type_index: u32, func_index: u32) -> Result<Option<RuntimeValue>, Error> {
		let function_type = match self.module.type_section()
			.ok_or(Error::Function(format!("trying to indirect call function {} with non-existent function section", func_index)))
			.and_then(|s| s.types().get(type_index as usize)
				.ok_or(Error::Function(format!("trying to indirect call function {} with non-existent type index {}", func_index, type_index))))? {
			&Type::Function(ref function_type) => function_type,
		};

		match self.imports.parse_table_index(table_index) {
			ItemIndex::IndexSpace(_) => unreachable!("parse_function_index resolves IndexSpace option"),
			ItemIndex::Internal(table_index) => {
				let table = self.table(ItemIndex::Internal(table_index))?;
				let index = match table.get(func_index)? {
					RuntimeValue::AnyFunc(index) => index,
					_ => return Err(Error::Function(format!("trying to indirect call function {} via non-anyfunc table {}", func_index, table_index))),
				};
				self.call_internal_function(outer, index, Some(function_type))
			},
			ItemIndex::External(table_index) => {
				let table = self.table(ItemIndex::External(table_index))?;
				let index = match table.get(func_index)? {
					RuntimeValue::AnyFunc(index) => index,
					_ => return Err(Error::Function(format!("trying to indirect call function {} via non-anyfunc table {}", func_index, table_index))),
				};
				let module = self.module.import_section()
					.ok_or(Error::Function(format!("trying to access external table with index {} in module without import section", table_index)))
					.and_then(|s| s.entries().get(table_index as usize)
						.ok_or(Error::Function(format!("trying to access external table with index {} in module with {}-entries import section", table_index, s.entries().len()))))
					.and_then(|e| self.imports.module(Some(outer.externals), e.module()))?;
				module.call_internal_function(outer, index, Some(function_type))
			}
		}
	}

	fn call_internal_function(&self, mut outer: CallerContext, index: u32, function_type: Option<&FunctionType>) -> Result<Option<RuntimeValue>, Error> {
		// TODO: cache
		// internal index = index of function in functions section && index of code in code section
		// get function type index
		let function_type_index = self.module
			.function_section()
			.ok_or(Error::Function(format!("trying to call function with index {} in module without function section", index)))
			.and_then(|s| s.entries()
				.get(index as usize)
				.ok_or(Error::Function(format!("trying to call function with index {} in module with {} functions", index, s.entries().len()))))?
			.type_ref();
		// function type index = index of function type in types index
		// get function type
		let item_type = self.module
			.type_section()
			.ok_or(Error::Function(format!("trying to call function with index {} in module without types section", index)))
			.and_then(|s| s.types()
				.get(function_type_index as usize)
				.ok_or(Error::Function(format!("trying to call function with type index {} in module with {} types", index, s.types().len()))))?;
		let actual_function_type = match item_type {
			&Type::Function(ref function_type) => function_type,
		};
		if let Some(ref function_type) = function_type {
			if function_type != &actual_function_type {
				return Err(Error::Function(format!("expected function with signature ({:?}) -> {:?} when got with ({:?}) -> {:?}",
					function_type.params(), function_type.return_type(), actual_function_type.params(), actual_function_type.return_type())));
			}
		}
		// get function body
		let function_body = self.module
			.code_section()
			.ok_or(Error::Function(format!("trying to call function with index {} in module without code section", index)))
			.and_then(|s| s.bodies()
				.get(index as usize)
				.ok_or(Error::Function(format!("trying to call function with index {} in module with {} functions codes", index, s.bodies().len()))))?;

		// each functions has its own value stack
		// but there's global stack limit
		// args, locals
		let function_code = function_body.code().elements();
		let value_stack_limit = outer.value_stack_limit;
		let frame_stack_limit = outer.frame_stack_limit;
		let locals = prepare_function_locals(actual_function_type, function_body, &mut outer)?;
		let mut innner = FunctionContext::new(self, outer.externals, value_stack_limit, frame_stack_limit, actual_function_type, locals);
		Interpreter::run_function(&mut innner, function_code)
	}
}

impl<'a> CallerContext<'a> {
	/// Top most args
	pub fn topmost(args: &'a mut StackWithLimit<RuntimeValue>, externals: &'a HashMap<String, Arc<ModuleInstanceInterface + 'a>>) -> Self {
		CallerContext {
			value_stack_limit: DEFAULT_VALUE_STACK_LIMIT,
			frame_stack_limit: DEFAULT_FRAME_STACK_LIMIT,
			value_stack: args,
			externals: externals,
		}
	}

	/// Nested context
	pub fn nested(outer: &'a mut FunctionContext) -> Self {
		CallerContext {
			value_stack_limit: outer.value_stack().limit() - outer.value_stack().len(),
			frame_stack_limit: outer.frame_stack().limit() - outer.frame_stack().len(),
			value_stack: &mut outer.value_stack,
			externals: &outer.externals,
		}
	}
}

pub fn check_limits(limits: &ResizableLimits) -> Result<(), Error> {
	if let Some(maximum) = limits.maximum() {
		if maximum < limits.initial() {
			return Err(Error::Validation(format!("maximum limit {} is lesser than minimum {}", maximum, limits.initial())));
		}
	}

	Ok(())
}

fn prepare_function_locals(function_type: &FunctionType, function_body: &FuncBody, outer: &mut CallerContext) -> Result<Vec<VariableInstance>, Error> {
	// locals = function arguments + defined locals
	function_type.params().iter().rev()
		.map(|param_type| {
			let param_value = outer.value_stack.pop()?;
			let actual_type = param_value.variable_type();
			let expected_type = (*param_type).into();
			if actual_type != Some(expected_type) {
				return Err(Error::Function(format!("invalid parameter type {:?} when expected {:?}", actual_type, expected_type)));
			}

			VariableInstance::new(true, expected_type, param_value)
		})
		.collect::<Vec<_>>().into_iter().rev()
		.chain(function_body.locals()
			.iter()
			.flat_map(|l| repeat(l.value_type().into()).take(l.count() as usize))
			.map(|vt| VariableInstance::new(true, vt, RuntimeValue::default(vt))))
		.collect::<Result<Vec<_>, _>>()
}

fn get_initializer(expr: &InitExpr, module: &Module, imports: &ModuleImports) -> Result<RuntimeValue, Error> {
	let first_opcode = expr.code().get(0).ok_or(Error::Initialization(format!("empty instantiation-time initializer")))?;
	match first_opcode {
		&Opcode::GetGlobal(index) => {
			let index = match imports.parse_global_index(ItemIndex::IndexSpace(index)) {
				ItemIndex::External(index) => index,
				_ => return Err(Error::Global(format!("trying to initialize with non-external global {}", index))),
			};
			module.import_section()
				.ok_or(Error::Global(format!("trying to initialize with external global with index {} in module without import section", index)))
				.and_then(|s| s.entries().get(index as usize)
					.ok_or(Error::Global(format!("trying to initialize with external global with index {} in module with {}-entries import section", index, s.entries().len()))))
				.and_then(|e| imports.global(None, e))
				.map(|g| g.get())
		},
		&Opcode::I32Const(val) => Ok(RuntimeValue::I32(val)),
		&Opcode::I64Const(val) => Ok(RuntimeValue::I64(val)),
		&Opcode::F32Const(val) => Ok(RuntimeValue::decode_f32(val)),
		&Opcode::F64Const(val) => Ok(RuntimeValue::decode_f64(val)),
		_ => Err(Error::Initialization(format!("not-supported {:?} instruction in instantiation-time initializer", first_opcode))),
	}
}