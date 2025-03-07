use super::{JitCompileError, JitSig, JitType};
use cranelift::codegen::ir::FuncRef;
use cranelift::prelude::*;
use num_traits::cast::ToPrimitive;
use rustpython_compiler_core::bytecode::{
    self, BinaryOperator, BorrowedConstant, CodeObject, ComparisonOperator, Instruction, Label,
    OpArg, OpArgState, UnaryOperator,
};
use std::collections::HashMap;

#[repr(u16)]
enum CustomTrapCode {
    /// Raised when shifting by a negative number
    NegativeShiftCount = 0,
}

#[derive(Clone)]
struct Local {
    var: Variable,
    ty: JitType,
}

#[derive(Debug)]
enum JitValue {
    Int(Value),
    Float(Value),
    Bool(Value),
    None,
    Tuple(Vec<JitValue>),
    FuncRef(FuncRef),
}

impl JitValue {
    fn from_type_and_value(ty: JitType, val: Value) -> JitValue {
        match ty {
            JitType::Int => JitValue::Int(val),
            JitType::Float => JitValue::Float(val),
            JitType::Bool => JitValue::Bool(val),
        }
    }

    fn to_jit_type(&self) -> Option<JitType> {
        match self {
            JitValue::Int(_) => Some(JitType::Int),
            JitValue::Float(_) => Some(JitType::Float),
            JitValue::Bool(_) => Some(JitType::Bool),
            JitValue::None | JitValue::Tuple(_) | JitValue::FuncRef(_) => None,
        }
    }

    fn into_value(self) -> Option<Value> {
        match self {
            JitValue::Int(val) | JitValue::Float(val) | JitValue::Bool(val) => Some(val),
            JitValue::None | JitValue::Tuple(_) | JitValue::FuncRef(_) => None,
        }
    }
}

pub struct FunctionCompiler<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    stack: Vec<JitValue>,
    variables: Box<[Option<Local>]>,
    label_to_block: HashMap<Label, Block>,
    pub(crate) sig: JitSig,
}

impl<'a, 'b> FunctionCompiler<'a, 'b> {
    pub fn new(
        builder: &'a mut FunctionBuilder<'b>,
        num_variables: usize,
        arg_types: &[JitType],
        ret_type: Option<JitType>,
        entry_block: Block,
    ) -> FunctionCompiler<'a, 'b> {
        let mut compiler = FunctionCompiler {
            builder,
            stack: Vec::new(),
            variables: vec![None; num_variables].into_boxed_slice(),
            label_to_block: HashMap::new(),
            sig: JitSig {
                args: arg_types.to_vec(),
                ret: ret_type,
            },
        };
        let params = compiler.builder.func.dfg.block_params(entry_block).to_vec();
        for (i, (ty, val)) in arg_types.iter().zip(params).enumerate() {
            compiler
                .store_variable(i as u32, JitValue::from_type_and_value(ty.clone(), val))
                .unwrap();
        }
        compiler
    }

    fn pop_multiple(&mut self, count: usize) -> Vec<JitValue> {
        let stack_len = self.stack.len();
        self.stack.drain(stack_len - count..).collect()
    }

    fn store_variable(
        &mut self,
        idx: bytecode::NameIdx,
        val: JitValue,
    ) -> Result<(), JitCompileError> {
        let builder = &mut self.builder;
        let ty = val.to_jit_type().ok_or(JitCompileError::NotSupported)?;
        let local = self.variables[idx as usize].get_or_insert_with(|| {
            let var = Variable::new(idx as usize);
            let local = Local {
                var,
                ty: ty.clone(),
            };
            builder.declare_var(var, ty.to_cranelift());
            local
        });
        if ty != local.ty {
            Err(JitCompileError::NotSupported)
        } else {
            self.builder.def_var(local.var, val.into_value().unwrap());
            Ok(())
        }
    }

    fn boolean_val(&mut self, val: JitValue) -> Result<Value, JitCompileError> {
        match val {
            JitValue::Float(val) => {
                let zero = self.builder.ins().f64const(0);
                let val = self.builder.ins().fcmp(FloatCC::NotEqual, val, zero);
                Ok(self.builder.ins().bint(types::I8, val))
            }
            JitValue::Int(val) => {
                let zero = self.builder.ins().iconst(types::I64, 0);
                let val = self.builder.ins().icmp(IntCC::NotEqual, val, zero);
                Ok(self.builder.ins().bint(types::I8, val))
            }
            JitValue::Bool(val) => Ok(val),
            JitValue::None => Ok(self.builder.ins().iconst(types::I8, 0)),
            JitValue::Tuple(_) | JitValue::FuncRef(_) => Err(JitCompileError::NotSupported),
        }
    }

    fn get_or_create_block(&mut self, label: Label) -> Block {
        let builder = &mut self.builder;
        *self
            .label_to_block
            .entry(label)
            .or_insert_with(|| builder.create_block())
    }

    pub fn compile<C: bytecode::Constant>(
        &mut self,
        func_ref: FuncRef,
        bytecode: &CodeObject<C>,
    ) -> Result<(), JitCompileError> {
        // TODO: figure out if this is sufficient -- previously individual labels were associated
        // pretty much per-bytecode that uses them, or at least per "type" of block -- in theory an
        // if block and a with block might jump to the same place. Now it's all "flattened", so
        // there might be less distinction between different types of blocks going off
        // label_targets alone
        let label_targets = bytecode.label_targets();

        let mut arg_state = OpArgState::default();
        for (offset, instruction) in bytecode.instructions.iter().enumerate() {
            let (instruction, arg) = arg_state.get(*instruction);
            let label = Label(offset as u32);
            if label_targets.contains(&label) {
                let block = self.get_or_create_block(label);

                // If the current block is not terminated/filled just jump
                // into the new block.
                if !self.builder.is_filled() {
                    self.builder.ins().jump(block, &[]);
                }

                self.builder.switch_to_block(block);
            }

            // Sometimes the bytecode contains instructions after a return
            // just ignore those until we are at the next label
            if self.builder.is_filled() {
                continue;
            }

            self.add_instruction(func_ref, bytecode, instruction, arg)?;
        }

        Ok(())
    }

    fn prepare_const<C: bytecode::Constant>(
        &mut self,
        constant: BorrowedConstant<'_, C>,
    ) -> Result<JitValue, JitCompileError> {
        let value = match constant {
            BorrowedConstant::Integer { value } => {
                let val = self.builder.ins().iconst(
                    types::I64,
                    value.to_i64().ok_or(JitCompileError::NotSupported)?,
                );
                JitValue::Int(val)
            }
            BorrowedConstant::Float { value } => {
                let val = self.builder.ins().f64const(value);
                JitValue::Float(val)
            }
            BorrowedConstant::Boolean { value } => {
                let val = self.builder.ins().iconst(types::I8, value as i64);
                JitValue::Bool(val)
            }
            BorrowedConstant::None => JitValue::None,
            _ => return Err(JitCompileError::NotSupported),
        };
        Ok(value)
    }

    fn return_value(&mut self, val: JitValue) -> Result<(), JitCompileError> {
        if let Some(ref ty) = self.sig.ret {
            if val.to_jit_type().as_ref() != Some(ty) {
                return Err(JitCompileError::NotSupported);
            }
        } else {
            let ty = val.to_jit_type().ok_or(JitCompileError::NotSupported)?;
            self.sig.ret = Some(ty.clone());
            self.builder
                .func
                .signature
                .returns
                .push(AbiParam::new(ty.to_cranelift()));
        }
        self.builder.ins().return_(&[val.into_value().unwrap()]);
        Ok(())
    }

    pub fn add_instruction<C: bytecode::Constant>(
        &mut self,
        func_ref: FuncRef,
        bytecode: &CodeObject<C>,
        instruction: Instruction,
        arg: OpArg,
    ) -> Result<(), JitCompileError> {
        match instruction {
            Instruction::ExtendedArg => Ok(()),
            Instruction::JumpIfFalse { target } => {
                let cond = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let val = self.boolean_val(cond)?;
                let then_block = self.get_or_create_block(target.get(arg));
                self.builder.ins().brz(val, then_block, &[]);

                let block = self.builder.create_block();
                self.builder.ins().jump(block, &[]);
                self.builder.switch_to_block(block);

                Ok(())
            }
            Instruction::JumpIfTrue { target } => {
                let cond = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let val = self.boolean_val(cond)?;
                let then_block = self.get_or_create_block(target.get(arg));
                self.builder.ins().brnz(val, then_block, &[]);

                let block = self.builder.create_block();
                self.builder.ins().jump(block, &[]);
                self.builder.switch_to_block(block);

                Ok(())
            }
            Instruction::Jump { target } => {
                let target_block = self.get_or_create_block(target.get(arg));
                self.builder.ins().jump(target_block, &[]);

                Ok(())
            }
            Instruction::LoadFast(idx) => {
                let local = self.variables[idx.get(arg) as usize]
                    .as_ref()
                    .ok_or(JitCompileError::BadBytecode)?;
                self.stack.push(JitValue::from_type_and_value(
                    local.ty.clone(),
                    self.builder.use_var(local.var),
                ));
                Ok(())
            }
            Instruction::StoreFast(idx) => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                self.store_variable(idx.get(arg), val)
            }
            Instruction::LoadConst { idx } => {
                let val = self
                    .prepare_const(bytecode.constants[idx.get(arg) as usize].borrow_constant())?;
                self.stack.push(val);
                Ok(())
            }
            Instruction::BuildTuple { size } => {
                let elements = self.pop_multiple(size.get(arg) as usize);
                self.stack.push(JitValue::Tuple(elements));
                Ok(())
            }
            Instruction::UnpackSequence { size } => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let elements = match val {
                    JitValue::Tuple(elements) => elements,
                    _ => return Err(JitCompileError::NotSupported),
                };

                if elements.len() != size.get(arg) as usize {
                    return Err(JitCompileError::NotSupported);
                }

                self.stack.extend(elements.into_iter().rev());
                Ok(())
            }
            Instruction::ReturnValue => {
                let val = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                self.return_value(val)
            }
            Instruction::ReturnConst { idx } => {
                let val = self
                    .prepare_const(bytecode.constants[idx.get(arg) as usize].borrow_constant())?;
                self.return_value(val)
            }
            Instruction::CompareOperation { op, .. } => {
                let op = op.get(arg);
                // the rhs is popped off first
                let b = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let a_type: Option<JitType> = a.to_jit_type();
                let b_type: Option<JitType> = b.to_jit_type();

                match (a, b) {
                    (JitValue::Int(a), JitValue::Int(b))
                    | (JitValue::Bool(a), JitValue::Bool(b))
                    | (JitValue::Bool(a), JitValue::Int(b))
                    | (JitValue::Int(a), JitValue::Bool(b)) => {
                        let operand_one = match a_type.unwrap() {
                            JitType::Bool => self.builder.ins().uextend(types::I64, a),
                            _ => a,
                        };

                        let operand_two = match b_type.unwrap() {
                            JitType::Bool => self.builder.ins().uextend(types::I64, b),
                            _ => b,
                        };

                        let cond = match op {
                            ComparisonOperator::Equal => IntCC::Equal,
                            ComparisonOperator::NotEqual => IntCC::NotEqual,
                            ComparisonOperator::Less => IntCC::SignedLessThan,
                            ComparisonOperator::LessOrEqual => IntCC::SignedLessThanOrEqual,
                            ComparisonOperator::Greater => IntCC::SignedGreaterThan,
                            ComparisonOperator::GreaterOrEqual => IntCC::SignedGreaterThanOrEqual,
                        };

                        let val = self.builder.ins().icmp(cond, operand_one, operand_two);
                        // TODO: Remove this `bint` in cranelift 0.90 as icmp now returns i8
                        self.stack
                            .push(JitValue::Bool(self.builder.ins().bint(types::I8, val)));
                        Ok(())
                    }
                    (JitValue::Float(a), JitValue::Float(b)) => {
                        let cond = match op {
                            ComparisonOperator::Equal => FloatCC::Equal,
                            ComparisonOperator::NotEqual => FloatCC::NotEqual,
                            ComparisonOperator::Less => FloatCC::LessThan,
                            ComparisonOperator::LessOrEqual => FloatCC::LessThanOrEqual,
                            ComparisonOperator::Greater => FloatCC::GreaterThan,
                            ComparisonOperator::GreaterOrEqual => FloatCC::GreaterThanOrEqual,
                        };

                        let val = self.builder.ins().fcmp(cond, a, b);
                        // TODO: Remove this `bint` in cranelift 0.90 as fcmp now returns i8
                        self.stack
                            .push(JitValue::Bool(self.builder.ins().bint(types::I8, val)));
                        Ok(())
                    }
                    _ => Err(JitCompileError::NotSupported),
                }
            }
            Instruction::UnaryOperation { op, .. } => {
                let op = op.get(arg);
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                match (op, a) {
                    (UnaryOperator::Minus, JitValue::Int(val)) => {
                        // Compile minus as 0 - a.
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let out = self.compile_sub(zero, val);
                        self.stack.push(JitValue::Int(out));
                        Ok(())
                    }
                    (UnaryOperator::Plus, JitValue::Int(val)) => {
                        // Nothing to do
                        self.stack.push(JitValue::Int(val));
                        Ok(())
                    }
                    (UnaryOperator::Not, a) => {
                        let boolean = self.boolean_val(a)?;
                        let not_boolean = self.builder.ins().bxor_imm(boolean, 1);
                        self.stack.push(JitValue::Bool(not_boolean));
                        Ok(())
                    }
                    _ => Err(JitCompileError::NotSupported),
                }
            }
            Instruction::BinaryOperation { op } | Instruction::BinaryOperationInplace { op } => {
                let op = op.get(arg);
                // the rhs is popped off first
                let b = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                let a = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;

                let a_type = a.to_jit_type();
                let b_type = b.to_jit_type();

                let val = match (op, a, b) {
                    (BinaryOperator::Add, JitValue::Int(a), JitValue::Int(b)) => {
                        let (out, carry) = self.builder.ins().iadd_ifcout(a, b);
                        self.builder.ins().trapif(
                            IntCC::Overflow,
                            carry,
                            TrapCode::IntegerOverflow,
                        );
                        JitValue::Int(out)
                    }
                    (BinaryOperator::Subtract, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.compile_sub(a, b))
                    }
                    (BinaryOperator::Multiply, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().imul(a, b))
                    }
                    (BinaryOperator::FloorDivide, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().sdiv(a, b))
                    }
                    (BinaryOperator::Divide, JitValue::Int(a), JitValue::Int(b)) => {
                        // Convert to float for regular division
                        let a_float = self.builder.ins().fcvt_from_sint(types::F64, a);
                        let b_float = self.builder.ins().fcvt_from_sint(types::F64, b);
                        JitValue::Float(self.builder.ins().fdiv(a_float, b_float))
                    }
                    (BinaryOperator::Modulo, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().srem(a, b))
                    }
                    // Todo: This should return int when possible
                    (BinaryOperator::Power, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Float(self.compile_ipow(a, b))
                    }
                    (
                        BinaryOperator::Lshift | BinaryOperator::Rshift,
                        JitValue::Int(a),
                        JitValue::Int(b),
                    ) => {
                        // Shifts throw an exception if we have a negative shift count
                        // Remove all bits except the sign bit, and trap if its 1 (i.e. negative).
                        let sign = self.builder.ins().ushr_imm(b, 63);
                        self.builder.ins().trapnz(
                            sign,
                            TrapCode::User(CustomTrapCode::NegativeShiftCount as u16),
                        );

                        let out = if op == BinaryOperator::Lshift {
                            self.builder.ins().ishl(a, b)
                        } else {
                            self.builder.ins().sshr(a, b)
                        };
                        JitValue::Int(out)
                    }
                    (BinaryOperator::And, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().band(a, b))
                    }
                    (BinaryOperator::Or, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().bor(a, b))
                    }
                    (BinaryOperator::Xor, JitValue::Int(a), JitValue::Int(b)) => {
                        JitValue::Int(self.builder.ins().bxor(a, b))
                    }

                    // Floats
                    (BinaryOperator::Add, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fadd(a, b))
                    }
                    (BinaryOperator::Subtract, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fsub(a, b))
                    }
                    (BinaryOperator::Multiply, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fmul(a, b))
                    }
                    (BinaryOperator::Divide, JitValue::Float(a), JitValue::Float(b)) => {
                        JitValue::Float(self.builder.ins().fdiv(a, b))
                    }

                    // Floats and Integers
                    (_, JitValue::Int(a), JitValue::Float(b))
                    | (_, JitValue::Float(a), JitValue::Int(b)) => {
                        let operand_one = match a_type.unwrap() {
                            JitType::Int => self.builder.ins().fcvt_from_sint(types::F64, a),
                            _ => a,
                        };

                        let operand_two = match b_type.unwrap() {
                            JitType::Int => self.builder.ins().fcvt_from_sint(types::F64, b),
                            _ => b,
                        };

                        match op {
                            BinaryOperator::Add => {
                                JitValue::Float(self.builder.ins().fadd(operand_one, operand_two))
                            }
                            BinaryOperator::Subtract => {
                                JitValue::Float(self.builder.ins().fsub(operand_one, operand_two))
                            }
                            BinaryOperator::Multiply => {
                                JitValue::Float(self.builder.ins().fmul(operand_one, operand_two))
                            }
                            BinaryOperator::Divide => {
                                JitValue::Float(self.builder.ins().fdiv(operand_one, operand_two))
                            }
                            _ => return Err(JitCompileError::NotSupported),
                        }
                    }
                    _ => return Err(JitCompileError::NotSupported),
                };
                self.stack.push(val);

                Ok(())
            }
            Instruction::SetupLoop { .. } | Instruction::PopBlock => {
                // TODO: block support
                Ok(())
            }
            Instruction::LoadGlobal(idx) => {
                let name = &bytecode.names[idx.get(arg) as usize];

                if name.as_ref() != bytecode.obj_name.as_ref() {
                    Err(JitCompileError::NotSupported)
                } else {
                    self.stack.push(JitValue::FuncRef(func_ref));
                    Ok(())
                }
            }
            Instruction::CallFunctionPositional { nargs } => {
                let nargs = nargs.get(arg);

                let mut args = Vec::new();
                for _ in 0..nargs {
                    let arg = self.stack.pop().ok_or(JitCompileError::BadBytecode)?;
                    args.push(arg.into_value().unwrap());
                }

                match self.stack.pop().ok_or(JitCompileError::BadBytecode)? {
                    JitValue::FuncRef(reference) => {
                        let call = self.builder.ins().call(reference, &args);
                        let returns = self.builder.inst_results(call);
                        self.stack.push(JitValue::Int(returns[0]));

                        Ok(())
                    }
                    _ => Err(JitCompileError::BadBytecode),
                }
            }
            _ => Err(JitCompileError::NotSupported),
        }
    }

    fn compile_sub(&mut self, a: Value, b: Value) -> Value {
        // TODO: this should be fine, but cranelift doesn't special-case isub_ifbout
        // let (out, carry) = self.builder.ins().isub_ifbout(a, b);
        // self.builder
        //     .ins()
        //     .trapif(IntCC::Overflow, carry, TrapCode::IntegerOverflow);
        // TODO: this shouldn't wrap
        let neg_b = self.builder.ins().ineg(b);
        let (out, carry) = self.builder.ins().iadd_ifcout(a, neg_b);
        self.builder
            .ins()
            .trapif(IntCC::Overflow, carry, TrapCode::IntegerOverflow);
        out
    }
    fn compile_ipow(&mut self, a: Value, b: Value) -> Value {
        // Convert base to float since result might not always be a Int
        let float_base = self.builder.ins().fcvt_from_sint(types::F64, a);

        // Create code blocks
        let check_block1 = self.builder.create_block();
        let check_block2 = self.builder.create_block();
        let check_block3 = self.builder.create_block();
        let handle_neg_exp = self.builder.create_block();
        let loop_block = self.builder.create_block();
        let continue_block = self.builder.create_block();
        let exit_block = self.builder.create_block();

        // Set code block params
        // Set code block params
        self.builder.append_block_param(check_block1, types::F64);
        self.builder.append_block_param(check_block1, types::I64);

        self.builder.append_block_param(check_block2, types::F64);
        self.builder.append_block_param(check_block2, types::I64);

        self.builder.append_block_param(check_block3, types::F64);
        self.builder.append_block_param(check_block3, types::I64);

        self.builder.append_block_param(handle_neg_exp, types::F64);
        self.builder.append_block_param(handle_neg_exp, types::I64);

        self.builder.append_block_param(loop_block, types::F64); //base
        self.builder.append_block_param(loop_block, types::F64); //result
        self.builder.append_block_param(loop_block, types::I64); //exponent

        self.builder.append_block_param(continue_block, types::F64); //base
        self.builder.append_block_param(continue_block, types::F64); //result
        self.builder.append_block_param(continue_block, types::I64); //exponent

        self.builder.append_block_param(exit_block, types::F64);

        // Begin evaluating by jumping to first check block
        self.builder.ins().jump(check_block1, &[float_base, b]);

        // Check block one:
        // Checks if input is O ** n where n > 0
        // Jumps to exit_block as 0 if true
        self.builder.switch_to_block(check_block1);
        let paramsc1 = self.builder.block_params(check_block1);
        let basec1 = paramsc1[0];
        let expc1 = paramsc1[1];
        let zero_f64 = self.builder.ins().f64const(0.0);
        let zero_i64 = self.builder.ins().iconst(types::I64, 0);
        let is_base_zero = self.builder.ins().fcmp(FloatCC::Equal, zero_f64, basec1);
        let is_exp_positive = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThan, expc1, zero_i64);
        let is_zero_to_positive = self.builder.ins().band(is_base_zero, is_exp_positive);
        self.builder
            .ins()
            .brnz(is_zero_to_positive, exit_block, &[zero_f64]);
        self.builder.ins().jump(check_block2, &[basec1, expc1]);

        // Check block two:
        // Checks if exponent is negative
        // Jumps to a special handle_neg_exponent block if true
        self.builder.switch_to_block(check_block2);
        let paramsc2 = self.builder.block_params(check_block2);
        let basec2 = paramsc2[0];
        let expc2 = paramsc2[1];
        let zero_i64 = self.builder.ins().iconst(types::I64, 0);
        let is_neg = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThan, expc2, zero_i64);
        self.builder
            .ins()
            .brnz(is_neg, handle_neg_exp, &[basec2, expc2]);
        self.builder.ins().jump(check_block3, &[basec2, expc2]);

        // Check block three:
        // Checks if exponent is one
        // jumps to exit block with the base of the exponents value
        self.builder.switch_to_block(check_block3);
        let paramsc3 = self.builder.block_params(check_block3);
        let basec3 = paramsc3[0];
        let expc3 = paramsc3[1];
        let resc3 = self.builder.ins().f64const(1.0);
        let one_i64 = self.builder.ins().iconst(types::I64, 1);
        let is_one = self.builder.ins().icmp(IntCC::Equal, expc3, one_i64);
        self.builder.ins().brnz(is_one, exit_block, &[basec3]);
        self.builder.ins().jump(loop_block, &[basec3, resc3, expc3]);

        // Handles negative Exponents
        // calculates x^(-n) = (1/x)^n
        // then proceeds to the loop to evaluate
        self.builder.switch_to_block(handle_neg_exp);
        let paramshn = self.builder.block_params(handle_neg_exp);
        let basehn = paramshn[0];
        let exphn = paramshn[1];
        let one_f64 = self.builder.ins().f64const(1.0);
        let base_inverse = self.builder.ins().fdiv(one_f64, basehn);
        let pos_exp = self.builder.ins().ineg(exphn);
        self.builder
            .ins()
            .jump(loop_block, &[base_inverse, one_f64, pos_exp]);

        // Main loop block
        // checks loop condition (exp > 0)
        // Jumps to continue block if true, exit block if false
        self.builder.switch_to_block(loop_block);
        let paramslb = self.builder.block_params(loop_block);
        let baselb = paramslb[0];
        let reslb = paramslb[1];
        let explb = paramslb[2];
        let zero = self.builder.ins().iconst(types::I64, 0);
        let is_zero = self.builder.ins().icmp(IntCC::Equal, explb, zero);
        self.builder.ins().brnz(is_zero, exit_block, &[reslb]);
        self.builder
            .ins()
            .jump(continue_block, &[baselb, reslb, explb]);

        // Continue block
        // Main math logic
        // Always jumps back to loob_block
        self.builder.switch_to_block(continue_block);
        let paramscb = self.builder.block_params(continue_block);
        let basecb = paramscb[0];
        let rescb = paramscb[1];
        let expcb = paramscb[2];
        let is_odd = self.builder.ins().band_imm(expcb, 1);
        let is_odd = self.builder.ins().icmp_imm(IntCC::Equal, is_odd, 1);
        let mul_result = self.builder.ins().fmul(rescb, basecb);
        let new_result = self.builder.ins().select(is_odd, mul_result, rescb);
        let squared_base = self.builder.ins().fmul(basecb, basecb);
        let new_exp = self.builder.ins().sshr_imm(expcb, 1);
        self.builder
            .ins()
            .jump(loop_block, &[squared_base, new_result, new_exp]);

        self.builder.switch_to_block(exit_block);
        let result = self.builder.block_params(exit_block)[0];

        self.builder.seal_block(check_block1);
        self.builder.seal_block(check_block2);
        self.builder.seal_block(check_block3);
        self.builder.seal_block(handle_neg_exp);
        self.builder.seal_block(loop_block);
        self.builder.seal_block(continue_block);
        self.builder.seal_block(exit_block);

        result
    }
}
